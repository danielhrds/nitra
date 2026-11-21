//! The app's window (`ziz.exe` with no `--role`): the rooms this profile is
//! in, with who is online, in the room and sharing, live from the room
//! server; creating and joining rooms; invites; sharing and watching.
//!
//! Threads, as in the interface prototype: the main thread owns the window
//! and only pumps messages (`window.rs`); the render thread owns the swap
//! chain and Dear ImGui and draws only when something changed (input, a
//! resize, a room event). Each room has its own client thread
//! (`rooms::RoomClient`); a small forwarder per room passes its events to
//! the render thread and wakes it.

mod gfx;
mod model;
mod share;
mod store;
mod textures;
mod view;
mod window;

use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use dear_imgui_rs::{Context, FontSource};
use dear_imgui_sys::backend_shim::{dx11, win32};
use gpu::{D3d11Device, DebugLayer};
use identity::Identity;
use rooms::{Command, Join, RoomClient, RoomConfig, RoomEvent, RoomInvite};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, GetWindowThreadProcessId, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT, SW_SHOW,
    ShowWindow, TranslateMessage, WM_CLOSE, WM_QUIT,
};
use windows::core::Interface;

use crate::profile::Profile;
use model::{Effect, Model};
use store::{RoomStore, SavedRoom};
use view::{Action, ViewState};

/// Frames drawn after input (hover and press states settle).
const FRAMES_AFTER_INPUT: u32 = 3;
const UI_INTERVAL: Duration = Duration::from_micros(16_667);
/// Invites made from the window: one hour, five uses.
const INVITE_TTL_SECS: u32 = 3600;
const INVITE_USES: u32 = 5;

#[derive(Clone, Debug)]
pub struct AppArgs {
    /// Room server for new rooms (saved rooms keep their own).
    pub server: String,
    /// Close after this long (screenshots and measurements).
    pub seconds: Option<f64>,
    /// Window position.
    pub at: Option<(i32, i32)>,
}

/// The Windows fonts used by the interface (always present on Windows 10 and
/// 11, so nothing is bundled).
const FONTS: [(&str, f32); 4] = [
    (r"C:\Windows\Fonts\segoeui.ttf", 15.0),
    (r"C:\Windows\Fonts\seguisb.ttf", 15.0),
    (r"C:\Windows\Fonts\seguisb.ttf", 15.0),
    (r"C:\Windows\Fonts\segoeuib.ttf", 15.0),
];

pub fn run(profile: &Profile, args: &AppArgs) -> Result<(), Box<dyn Error>> {
    // SAFETY: process-wide setting, before any window exists.
    unsafe { win32::dear_imgui_backend_win32_enable_dpi_awareness() };
    let adapters = gpu::enumerate_adapters()?;
    let adapter = adapters
        .iter()
        .find(|a| !a.info.is_software)
        .ok_or("no hardware adapter")?;
    let debug = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };
    let device = gpu::create_device(adapter, debug)?;
    device.enable_multithread_protection()?;

    let window = window::create("ziz", 1280, 800)?;
    let events = window::events();
    let (ready_sender, ready) = mpsc::sync_channel(1);
    let started = Instant::now();
    let render = {
        let (device, raw) = (device.clone(), window.0 as isize);
        let setup = Setup {
            identity: Arc::clone(&profile.identity),
            name: profile.name.clone(),
            dir: profile.dir.clone(),
            server: args.server.clone(),
        };
        std::thread::Builder::new()
            .name("render".to_owned())
            .spawn(move || render_thread(device, raw, setup, ready_sender))?
    };
    if let Err(message) = ready
        .recv()
        .unwrap_or_else(|_| Err("render thread exited".to_owned()))
    {
        let _ = render.join();
        // SAFETY: window owned by this thread.
        let _ = unsafe { DestroyWindow(window) };
        return Err(message.into());
    }
    let render = std::rc::Rc::new(std::cell::RefCell::new(Some(render)));
    let outcome = std::rc::Rc::new(std::cell::RefCell::new(None));
    {
        let (render, outcome, events) = (render.clone(), outcome.clone(), Arc::clone(&events));
        window::set_on_close(move || {
            events.closing.store(true, Ordering::Relaxed);
            events.wake();
            if let Some(handle) = render.borrow_mut().take() {
                *outcome.borrow_mut() = Some(
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("render thread panicked".to_owned())),
                );
            }
        });
    }
    // SAFETY: valid window of this thread.
    unsafe {
        if let Some((x, y)) = args.at {
            use windows::Win32::UI::WindowsAndMessaging::{
                SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
            };
            let _ = SetWindowPos(
                window,
                None,
                x,
                y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
            );
        }
        let _ = ShowWindow(window, SW_SHOW);
    }
    tracing::info!(
        startup_ms = started.elapsed().as_millis() as u64,
        since_process_ms = ms_since_process_created(),
        "window shown"
    );
    let mut close_sent = false;
    'run: loop {
        let mut timeout = u32::MAX;
        if let Some(seconds) = args.seconds {
            let left = (seconds - started.elapsed().as_secs_f64()).max(0.0);
            timeout = (left * 1000.0).ceil() as u32;
            if left == 0.0 && !close_sent {
                close_sent = true;
                // SAFETY: valid window of this thread.
                let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
        }
        // SAFETY: no handles; returns on a message or the timeout.
        let _ =
            unsafe { MsgWaitForMultipleObjectsEx(None, timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        let mut message = MSG::default();
        // SAFETY: standard message pump for this thread's window.
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    break 'run;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    let frames = outcome
        .borrow_mut()
        .take()
        .unwrap_or_else(|| Err("render thread not joined".to_owned()))?;
    tracing::info!(
        frames,
        seconds = started.elapsed().as_secs_f64(),
        "window closed"
    );
    Ok(())
}

/// What the render thread needs from the profile.
struct Setup {
    identity: Arc<Identity>,
    name: String,
    dir: std::path::PathBuf,
    server: String,
}

struct App {
    window: HWND,
    events: Arc<window::Events>,
    gfx: gfx::Gfx,
    context: Context,
    textures: textures::TextureManager,
    fonts: view::Fonts,
    view: ViewState,
    model: Model,
    store: RoomStore,
    clients: HashMap<String, RoomClient>,
    room_events: Receiver<(String, RoomEvent)>,
    room_sender: Sender<(String, RoomEvent)>,
    /// Sharing threads: the current one and any still stopping.
    shares: Vec<share::Sharing>,
    share_events: Receiver<share::ShareEnded>,
    share_sender: Sender<share::ShareEnded>,
    next_share: u64,
    setup: Setup,
    redraw: u32,
    last_draw: Option<Instant>,
    frames: u64,
}

impl App {
    /// Connects to a room in the background; its events reach the render
    /// thread through the forwarder.
    fn connect(&mut self, saved: &SavedRoom, join: Join) {
        let (client, events) = RoomClient::start(
            RoomConfig {
                server: saved.server.clone(),
                room: saved.id.clone(),
                name: self.setup.name.clone(),
                join,
            },
            Arc::clone(&self.setup.identity),
        );
        let (sender, wake, id) = (
            self.room_sender.clone(),
            Arc::clone(&self.events),
            saved.id.clone(),
        );
        let forwarder = std::thread::Builder::new()
            .name("room-events".into())
            .spawn(move || {
                for event in events {
                    if sender.send((id.clone(), event)).is_err() {
                        break;
                    }
                    wake.wake();
                }
            });
        if let Err(error) = forwarder {
            tracing::warn!(%error, "room event thread");
        }
        self.clients.insert(saved.id.clone(), client);
    }

    fn send(&mut self, room: &str, command: Command) {
        match self.clients.get(room) {
            Some(client) if client.send(command).is_ok() => {}
            _ => self.view.notify("A sala não está conectada agora."),
        }
    }

    /// Stops a room's client off the render thread (a polite close waits up
    /// to a second for the server).
    fn disconnect(&mut self, room: &str) {
        if let Some(client) = self.clients.remove(room) {
            let _ = std::thread::Builder::new()
                .name("room-stop".into())
                .spawn(move || client.stop());
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Select(id) => self.model.selected = Some(id),
            Action::Enter(id) => {
                if let Some(previous) = self.model.entered.clone().filter(|p| *p != id) {
                    self.send(&previous, Command::Exit);
                }
                self.send(&id, Command::Enter);
                self.model.entered = Some(id);
            }
            Action::Exit(id) => {
                // Sharing is being in the room: leaving it stops the share.
                self.stop_share();
                self.send(&id, Command::Exit);
                self.model.entered = None;
            }
            Action::PickShare(room) => {
                let sources = share::sources();
                self.view.open_share(room, sources);
            }
            Action::Share { room, source } => self.start_share(room, &source),
            Action::StopShare => self.stop_share(),
            Action::CopyInvite(id) => self.send(
                &id,
                Command::CreateInvite {
                    ttl_secs: INVITE_TTL_SECS,
                    max_uses: INVITE_USES,
                },
            ),
            Action::Create(name) => match rooms::protocol::new_room_id() {
                Ok(id) => {
                    let saved = SavedRoom {
                        id,
                        name: name.clone(),
                        server: self.setup.server.clone(),
                    };
                    self.save(&saved);
                    self.model.add_room(&saved);
                    self.connect(&saved, Join::Create { name });
                }
                Err(error) => self
                    .view
                    .notify(format!("Não deu para criar a sala: {error}")),
            },
            Action::Join(text) => match RoomInvite::parse(&text) {
                Some(invite) if self.model.room(&invite.room).is_some() => {
                    self.model.selected = Some(invite.room);
                    self.view.notify("Você já está nessa sala.");
                }
                Some(invite) => {
                    let saved = SavedRoom {
                        id: invite.room.clone(),
                        name: "Entrando…".into(),
                        server: self.setup.server.clone(),
                    };
                    self.model.add_room(&saved);
                    // Saved once the room accepts us (its welcome names it).
                    self.connect(&saved, Join::Invite(invite.token));
                }
                None => self
                    .view
                    .notify("Isso não é um convite de sala (começa com zizroom:)."),
            },
            Action::Rename { room, name } => self.send(&room, Command::Rename(name)),
            Action::Remove { room, key } => self.send(&room, Command::RemoveMember(key)),
            Action::Leave(room) => {
                if self.model.sharing.as_ref().is_some_and(|s| s.room == room) {
                    self.stop_share();
                }
                self.send(&room, Command::Leave);
            }
        }
    }

    /// Shares `source` in `room` (one screen at a time: a new pick replaces
    /// the current share). Entering the room comes with it.
    fn start_share(&mut self, room: String, source: &share::ShareSource) {
        let Some(server) = self.model.room(&room).map(|r| r.server.clone()) else {
            return;
        };
        self.stop_share();
        if self.model.entered.as_deref() != Some(room.as_str()) {
            self.act(Action::Enter(room.clone()));
        }
        self.next_share += 1;
        let id = self.next_share;
        let wake = Arc::clone(&self.events);
        let started = share::Sharing::start(
            id,
            share::ProfileParts {
                name: self.setup.name.clone(),
                dir: self.setup.dir.clone(),
                identity: Arc::clone(&self.setup.identity),
            },
            server,
            room.clone(),
            source,
            self.share_sender.clone(),
            move || wake.wake(),
        );
        match started {
            Ok(sharing) => {
                tracing::info!(id, source = %source.label(), "sharing");
                self.shares.push(sharing);
                self.model.sharing = Some(model::LocalShare {
                    id,
                    room,
                    label: source.label(),
                });
            }
            Err(error) => self
                .view
                .notify(format!("Não deu para transmitir: {error}")),
        }
    }

    /// Stops the current share; its thread reports back when it is done.
    fn stop_share(&mut self) {
        let Some(current) = self.model.sharing.take() else {
            return;
        };
        if let Some(sharing) = self.shares.iter().find(|s| s.id == current.id) {
            sharing.stop();
        }
    }

    fn share_ended(&mut self, ended: share::ShareEnded) {
        if let Some(index) = self.shares.iter().position(|s| s.id == ended.id) {
            // The thread has finished: joining it is immediate.
            self.shares.swap_remove(index).finish();
        }
        let current = self
            .model
            .sharing
            .as_ref()
            .is_some_and(|s| s.id == ended.id);
        if current {
            self.model.sharing = None;
        }
        if let Some(error) = ended.error {
            self.view.notify(format!("A transmissão parou: {error}"));
        }
    }

    fn save(&mut self, saved: &SavedRoom) {
        if let Err(error) = self.store.upsert(saved.clone()) {
            tracing::warn!(%error, "saving the room list");
            self.view
                .notify(format!("Não deu para salvar a lista de salas: {error}"));
        }
    }

    fn handle(&mut self, room: &str, event: RoomEvent) {
        let welcomed = matches!(event, RoomEvent::Connected(_));
        for effect in self.model.apply(room, event) {
            match effect {
                Effect::SaveRoom { id, name } => {
                    if let Some(server) = self.model.room(&id).map(|r| r.server.clone()) {
                        self.save(&SavedRoom { id, name, server });
                    }
                }
                Effect::ForgetRoom(id) => {
                    if let Err(error) = self.store.remove(&id) {
                        tracing::warn!(%error, "saving the room list");
                    }
                    self.model.remove_room(&id);
                    self.disconnect(&id);
                }
                Effect::CopyInvite(text) => match crate::clipboard::set_text(&text) {
                    Ok(()) => self
                        .view
                        .notify("Convite copiado (vale 1 hora e 5 entradas)."),
                    Err(error) => self
                        .view
                        .notify(format!("Convite: {text} (não copiado: {error})")),
                },
                Effect::Notice(text) => self.view.notify(text),
            }
        }
        // A room joined by invite is saved once it welcomed us.
        if welcomed && let Some(state) = self.model.room(room) {
            let saved = SavedRoom {
                id: state.id.clone(),
                name: state.name.clone(),
                server: state.server.clone(),
            };
            self.save(&saved);
        }
    }

    fn step(&mut self) -> Result<(), String> {
        let input = self.events.take_input();
        if !input.is_empty() {
            for (message, wparam, lparam) in input {
                // SAFETY: Dear ImGui's state is owned by this thread.
                unsafe {
                    win32::dear_imgui_backend_win32_wnd_proc_handler(
                        self.window.0,
                        message,
                        wparam,
                        lparam,
                    )
                };
            }
            self.redraw = FRAMES_AFTER_INPUT;
        }
        while let Ok((room, event)) = self.room_events.try_recv() {
            self.handle(&room, event);
            self.redraw = self.redraw.max(1);
        }
        while let Ok(ended) = self.share_events.try_recv() {
            self.share_ended(ended);
            self.redraw = self.redraw.max(1);
        }
        let resized = self.events.resized.swap(false, Ordering::Relaxed);
        if resized {
            self.gfx.resize(window::client_size(self.window))?;
        }
        if self.view.animating() {
            self.redraw = self.redraw.max(1);
        }
        if self.events.minimized.load(Ordering::Relaxed) {
            return Ok(());
        }
        let ui_due = self.redraw > 0 && self.ui_wait() == 0;
        if !(resized || ui_due) {
            return Ok(());
        }
        self.redraw = self.redraw.saturating_sub(1);
        self.last_draw = Some(Instant::now());
        self.draw()
    }

    fn ui_wait(&self) -> u32 {
        self.last_draw.map_or(0, |last| {
            UI_INTERVAL
                .saturating_sub(last.elapsed())
                .as_micros()
                .div_ceil(1000) as u32
        })
    }

    fn sleep_ms(&self) -> u32 {
        if self.redraw > 0 && !self.events.minimized.load(Ordering::Relaxed) {
            self.ui_wait()
        } else {
            u32::MAX
        }
    }

    fn draw(&mut self) -> Result<(), String> {
        self.gfx.wait_ready();
        self.gfx.begin(view::clear_color())?;
        // SAFETY: both backends were initialized for this context and window on this thread.
        unsafe {
            dx11::dear_imgui_backend_dx11_new_frame();
            win32::dear_imgui_backend_win32_new_frame();
        }
        let ui = self.context.frame();
        let chrome = view::Chrome {
            maximized: self.events.maximized.load(Ordering::Relaxed),
            hovered: self.events.hovered_button.load(Ordering::Relaxed),
        };
        let actions = view::build(ui, &self.fonts, &self.model, &mut self.view, &chrome);
        // Where the title bar and its buttons are, for the window thread.
        let width = ui.io().display_size()[0];
        self.events
            .caption_height
            .store(view::TITLE_H as i32, Ordering::Relaxed);
        self.events.buttons_left.store(
            width as i32 - 3 * window::TITLE_BUTTON_WIDTH,
            Ordering::Relaxed,
        );
        // SAFETY: the context is current on this thread, inside the frame.
        self.events
            .set_cursor(unsafe { dear_imgui_sys::igGetMouseCursor() });
        let pending = self.context.render(self.textures.consumer());
        let frame = self.textures.reconcile(pending)?;
        let draw_data = std::ptr::from_ref(frame.draw_data())
            .cast::<dear_imgui_sys::ImDrawData>()
            .cast_mut();
        // SAFETY: `DrawData` is a transparent wrapper of `ImDrawData`; every
        // texture it uses (the font atlas) is alive.
        unsafe { dx11::dear_imgui_backend_dx11_render_draw_data(draw_data) };
        drop(frame);
        self.gfx.present()?;
        self.frames += 1;
        if !actions.is_empty() {
            for action in actions {
                self.act(action);
            }
            self.redraw = self.redraw.max(FRAMES_AFTER_INPUT);
        }
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        while !self.events.closing.load(Ordering::Relaxed) {
            // SAFETY: valid handle; returns on a signal or the timeout.
            let _ = unsafe { WaitForSingleObject(self.events.wake_handle(), self.sleep_ms()) };
            self.step()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<u64, String> {
        // Ending the shares (the host says goodbye to its viewers) and
        // leaving every room connection politely, all in parallel.
        let mut stops: Vec<_> = self
            .shares
            .drain(..)
            .map(|sharing| std::thread::spawn(move || sharing.finish()))
            .collect();
        stops.extend(
            self.clients
                .drain()
                .map(|(_, client)| std::thread::spawn(move || client.stop())),
        );
        for stop in stops {
            let _ = stop.join();
        }
        self.textures.shutdown(&mut self.context)?;
        // SAFETY: matching shutdowns, renderer first, before the context drops.
        unsafe {
            dx11::dear_imgui_backend_dx11_shutdown();
            win32::dear_imgui_backend_win32_shutdown();
        }
        Ok(self.frames)
    }
}

/// Milliseconds since Windows created this process (startup measurements:
/// it includes loading the executable and its DLLs, which no clock inside
/// `main` sees). None if the times cannot be read.
fn ms_since_process_created() -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: the pseudo-handle of this process and out-pointers to locals.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }
    // FILETIME counts 100 ns units since 1601; Unix time starts in 1970.
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let since_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let now = UNIX_EPOCH_TICKS + (since_unix.as_nanos() / 100) as u64;
    Some(now.checked_sub(ticks(created))? / 10_000)
}

/// A font from the Windows folder, kept for the process's life (Dear ImGui
/// reads it whenever it renders new glyphs).
fn add_font(
    context: &mut Context,
    path: &str,
    size: f32,
) -> Result<(dear_imgui_rs::FontId, f32), String> {
    let bytes: &'static [u8] = Box::leak(
        std::fs::read(path)
            .map_err(|e| format!("font {path}: {e}"))?
            .into_boxed_slice(),
    );
    // Validated as a TrueType font first (the FreeType loader reads it raw).
    dear_imgui_rs::StbTrueTypeFontData::from_slice(bytes)
        .map_err(|e| format!("font {path}: {e:?}"))?;
    let config = dear_imgui_rs::FontConfig::new()
        .pixel_snap_h(true)
        .font_loader_flags(dear_imgui_rs::FontLoaderFlags(font_flags()))
        .rasterizer_multiply(font_multiply());
    // The atlas's own loader (FreeType, which hints glyphs to the pixel grid);
    // `stb_truetype_with_size` would pin stb_truetype, which does not hint
    // (that blurred the prototype's text).
    // SAFETY: `bytes` is a complete font (checked above), leaked: it stays
    // unchanged and alive for the process.
    let source = unsafe { FontSource::ttf_data_with_size(bytes, size) }.with_config(config);
    let id = context.font_atlas().add_font(&[source]);
    Ok((id, line_height_per_em(bytes)))
}

fn render_thread(
    device: D3d11Device,
    window: isize,
    setup: Setup,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> Result<u64, String> {
    let window = HWND(window as *mut _);
    let start = (|| -> Result<App, String> {
        // SAFETY: plain thread queries; both threads belong to this process.
        unsafe {
            let window_thread = GetWindowThreadProcessId(window, None);
            let _ = AttachThreadInput(GetCurrentThreadId(), window_thread, true);
        }
        let gfx = gfx::Gfx::new(&device, window, window::client_size(window))?;
        let mut context = Context::create();
        context
            .set_ini_filename(None::<String>)
            .map_err(|e| e.to_string())?;
        view::apply_style(context.style_mut());
        let mut loaded = Vec::new();
        for (path, size) in FONTS {
            loaded.push(add_font(&mut context, path, size)?);
        }
        // The window button glyphs: Windows 11's icon font, or Windows 10's.
        let icon_path = [
            r"C:\Windows\Fonts\SegoeIcons.ttf",
            r"C:\Windows\Fonts\segmdl2.ttf",
        ]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .ok_or("no Windows icon font (Segoe Fluent Icons or MDL2 Assets)")?;
        let icons = add_font(&mut context, icon_path, 10.0)?.0;
        let fonts = view::Fonts {
            regular: loaded[0].0,
            medium: loaded[1].0,
            semibold: loaded[2].0,
            bold: loaded[3].0,
            icons,
            text_scale: loaded[0].1,
        };
        let immediate = device.immediate_context().map_err(|e| e.to_string())?;
        // SAFETY: valid window, device and its immediate context; shut down in `finish`.
        unsafe {
            if !win32::dear_imgui_backend_win32_init(window.0) {
                return Err("ImGui Win32 backend".to_owned());
            }
            if !dx11::dear_imgui_backend_dx11_init(device.raw().as_raw(), immediate.as_raw()) {
                return Err("ImGui DX11 backend".to_owned());
            }
        }
        let textures = textures::TextureManager::new(&device, &mut context)?;
        // SAFETY: the context is current on this thread; read-only field.
        let loader = unsafe {
            let name = (*(*dear_imgui_sys::igGetIO_Nil()).Fonts).FontLoaderName;
            if name.is_null() {
                "?".to_owned()
            } else {
                std::ffi::CStr::from_ptr(name)
                    .to_string_lossy()
                    .into_owned()
            }
        };
        tracing::info!(%loader, flags = font_flags(), "fonts");
        let (store, warning) = RoomStore::load(&setup.dir).map_err(|e| e.to_string())?;
        let me = setup.identity.public_key().to_string().replace('-', "");
        let model = Model::new(me, store.rooms());
        let (room_sender, room_events) = mpsc::channel();
        let (share_sender, share_events) = mpsc::channel();
        let mut app = App {
            window,
            events: window::events(),
            gfx,
            context,
            textures,
            fonts,
            view: ViewState::default(),
            model,
            store,
            clients: HashMap::new(),
            room_events,
            room_sender,
            shares: Vec::new(),
            share_events,
            share_sender,
            next_share: 0,
            setup,
            redraw: FRAMES_AFTER_INPUT,
            last_draw: None,
            frames: 0,
        };
        if let Some(warning) = warning {
            app.view.notify(warning);
        }
        for saved in app.store.rooms().to_vec() {
            app.connect(&saved, Join::Member);
        }
        Ok(app)
    })();
    let mut app = match start {
        Ok(app) => app,
        Err(message) => {
            let _ = ready.send(Err(message.clone()));
            return Err(message);
        }
    };
    let _ = ready.send(Ok(()));
    let result = app.run();
    let frames = app.finish();
    result.and(frames)
}

/// (ascent - descent) / units per em, from the font's `hhea` and `head`
/// tables: how much taller Dear ImGui's "pixel height" is than a CSS size.
fn line_height_per_em(font: &[u8]) -> f32 {
    let u16_at = |o: usize| u16::from_be_bytes([font[o], font[o + 1]]);
    let i16_at = |o: usize| i16::from_be_bytes([font[o], font[o + 1]]);
    let u32_at = |o: usize| u32::from_be_bytes([font[o], font[o + 1], font[o + 2], font[o + 3]]);
    let tables = usize::from(u16_at(4));
    let find = |tag: &[u8; 4]| {
        (0..tables)
            .map(|i| 12 + 16 * i)
            .find(|&r| &font[r..r + 4] == tag)
            .map(|r| u32_at(r + 8) as usize)
    };
    let (Some(head), Some(hhea)) = (find(b"head"), find(b"hhea")) else {
        return 1.0;
    };
    let units_per_em = f32::from(u16_at(head + 18));
    let (ascent, descent) = (f32::from(i16_at(hhea + 4)), f32::from(i16_at(hhea + 6)));
    (ascent - descent) / units_per_em
}

/// FreeType loader flags for the interface fonts (`ZIZ_FT_FLAGS` overrides,
/// for comparing renderings). 0: the font's own TrueType hinting, which
/// Segoe UI is built for (the prototype's auto-hinter suited Inter, which has
/// little hinting of its own).
fn font_flags() -> u32 {
    std::env::var("ZIZ_FT_FLAGS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Rasterizer alpha multiply (`ZIZ_RASTER_MULT` overrides).
fn font_multiply() -> f32 {
    std::env::var("ZIZ_RASTER_MULT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0)
}
