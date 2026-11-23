//! The app's window (`nitra.exe` with no `--role`): the rooms this profile is
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
mod settings;
mod share;
mod store;
mod textures;
mod view;
mod watch;
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
use windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView;
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, GetWindowThreadProcessId, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT, SW_SHOW,
    ShowWindow, TranslateMessage, WM_QUIT,
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

/// The interface font, Inter (SIL Open Font License, `assets/fonts/OFL.txt`),
/// built into the executable: regular, medium, semibold, bold and medium
/// italic (an empty room's name), in the order of `view::Fonts`.
const FONTS: [(&str, &[u8]); 5] = [
    (
        "Inter 400",
        include_bytes!("../../assets/fonts/Inter-400.ttf"),
    ),
    (
        "Inter 500",
        include_bytes!("../../assets/fonts/Inter-500.ttf"),
    ),
    (
        "Inter 600",
        include_bytes!("../../assets/fonts/Inter-600.ttf"),
    ),
    (
        "Inter 700",
        include_bytes!("../../assets/fonts/Inter-700.ttf"),
    ),
    (
        "Inter 500 italic",
        include_bytes!("../../assets/fonts/Inter-500Italic.ttf"),
    ),
];

/// The design size the text fonts are loaded at (the view scales from it).
const FONT_SIZE: f32 = 15.0;

pub fn run(profile: &Profile, args: &AppArgs) -> Result<(), Box<dyn Error>> {
    // One window per profile; another start just brings it up.
    let profile_key = window::profile_key(&profile.dir);
    let Some(instance) = window::single_instance(&profile_key) else {
        tracing::info!("this profile's window is already open: asked it to show");
        return Ok(());
    };
    // SAFETY: process-wide setting, before any window exists.
    unsafe { win32::dear_imgui_backend_win32_enable_dpi_awareness() };
    let adapters = gpu::enumerate_adapters()?;
    let adapter = crate::hardware::main_adapter(&adapters).ok_or("no hardware adapter")?;
    let gpu = crate::hardware::Gpu::of(&adapter.info);
    // Which GPU, so a report from another PC says what it ran on (two-GPU
    // laptops: the first one is used, maybe not the game's).
    tracing::info!(
        name = %adapter.info.name.trim(),
        vendor = adapter.info.vendor.name(),
        driver = adapter.info.driver_version.as_deref().unwrap_or("?"),
        hardware_adapters = adapters.iter().filter(|a| !a.info.is_software).count(),
        support = ?gpu.support,
        "GPU"
    );
    let debug = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };
    let device = gpu::create_device(adapter, debug)?;
    device.enable_multithread_protection()?;

    let window = window::create("nitra", 1280, 800)?;
    window::start_tray(window, &profile_key);
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
            gpu,
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
                window::quit(window);
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
    let frames = match outcome
        .borrow_mut()
        .take()
        .unwrap_or_else(|| Err("render thread not joined".to_owned()))
    {
        Err(error) if error.starts_with(gfx::DEVICE_LOST) => {
            // The GPU was lost (a driver reset): start again in a new
            // process (every D3D object is gone), as this profile.
            tracing::warn!(%error, "GPU lost; reopening the app");
            drop(instance);
            return restart_after_gpu_loss(&error);
        }
        other => other?,
    };
    tracing::info!(
        frames,
        seconds = started.elapsed().as_secs_f64(),
        "window closed"
    );
    Ok(())
}

/// How many times in a row the app may reopen itself after losing the GPU
/// (counted in the environment it passes on); then it stops with the error.
const MAX_GPU_RESTARTS: u32 = 3;
/// The count of restarts so far, passed to the new process.
const GPU_RESTARTS_VAR: &str = "NITRA_GPU_RESTARTS";

/// Starts this program again with the same arguments, unless it already
/// did so `MAX_GPU_RESTARTS` times in a row.
fn restart_after_gpu_loss(error: &str) -> Result<(), Box<dyn Error>> {
    let restarts: u32 = std::env::var(GPU_RESTARTS_VAR)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if restarts >= MAX_GPU_RESTARTS {
        return Err(format!("{error} ({restarts} restarts in a row; giving up)").into());
    }
    std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env(GPU_RESTARTS_VAR, (restarts + 1).to_string())
        // The testing switch must not lose the new window's GPU too.
        .env_remove("NITRA_TEST_DEVICE_LOST")
        .spawn()?;
    Ok(())
}

/// What the render thread needs from the profile.
struct Setup {
    identity: Arc<Identity>,
    name: String,
    dir: std::path::PathBuf,
    server: String,
    gpu: crate::hardware::Gpu,
}

struct App {
    window: HWND,
    events: Arc<window::Events>,
    gfx: gfx::Gfx,
    context: Context,
    textures: textures::TextureManager,
    fonts: view::Fonts,
    /// Pictures the view draws (the logo), with the texture it keeps alive.
    art: view::Art,
    _logo_view: Option<ID3D11ShaderResourceView>,
    view: ViewState,
    model: Model,
    store: RoomStore,
    settings: settings::SettingsStore,
    clients: HashMap<String, RoomClient>,
    room_events: Receiver<(String, RoomEvent)>,
    room_sender: Sender<(String, RoomEvent)>,
    /// Sharing threads: the current one and any still stopping.
    shares: Vec<share::Sharing>,
    share_events: Receiver<share::ShareEnded>,
    share_sender: Sender<share::ShareEnded>,
    next_share: u64,
    /// Watching threads: current ones and any still stopping.
    watches: Vec<watch::Watching>,
    watch_events: Receiver<watch::WatchEnded>,
    watch_sender: Sender<watch::WatchEnded>,
    next_watch: u64,
    /// The window is in full screen (posted to the window thread).
    fullscreen: bool,
    /// When the live numbers were last redrawn.
    last_stats: Instant,
    /// The device the window draws with (preview textures are made on it).
    device: D3d11Device,
    /// Preview textures, by the id the model and Dear ImGui use.
    preview_views: HashMap<u64, ID3D11ShaderResourceView>,
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
                name: self.display_name(),
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
                // Sharing and watching are being in the room: leaving it
                // stops them.
                self.stop_share();
                self.stop_watches_in(&id);
                self.send(&id, Command::Exit);
                self.model.entered = None;
            }
            Action::PickShare(room) => {
                let sources = share::sources();
                // Testing: `NITRA_TEST_PICK=<text>` shares the first source whose
                // label contains it, without the picker (scripts cannot rely
                // on where a window sits in the list). Unset in normal use.
                let picked = std::env::var("NITRA_TEST_PICK")
                    .ok()
                    .and_then(|text| sources.iter().find(|s| s.label().contains(&text)).cloned());
                match picked {
                    Some(source) => self.share_or_ask(room, source),
                    None => self.view.open_share(room, sources),
                }
            }
            Action::Share { room, source } => self.share_or_ask(room, source),
            Action::ShareAnyway {
                room,
                source,
                remember,
            } => {
                if remember {
                    let settings = settings::Settings {
                        untested_gpu_ok: true,
                        ..self.settings.get().clone()
                    };
                    self.save_settings(settings);
                }
                self.start_share(room, &source);
            }
            Action::StopShare => self.stop_share(),
            Action::OpenSettings => {
                let current = self.settings.get().clone();
                let name = self.display_name();
                self.view.open_settings(current, name);
            }
            Action::SaveSettings(settings) => self.save_settings(*settings),
            Action::OpenLogs => {
                let logs = self.setup.dir.join("logs");
                if let Err(error) = std::process::Command::new("explorer.exe")
                    .arg(&logs)
                    .spawn()
                {
                    self.view
                        .notify(format!("Não deu para abrir a pasta de logs: {error}"));
                }
            }
            Action::EraseData => self.erase_data(),
            Action::Watch { room, key } => self.start_watch(room, &key),
            Action::StopWatch(key) => {
                let ids: Vec<u64> = self
                    .model
                    .watching
                    .iter()
                    .filter(|w| w.key == key)
                    .map(|w| w.id)
                    .collect();
                for id in ids {
                    self.stop_watch(id);
                }
            }
            Action::Focus(key) => {
                self.model.focus = if self.model.focus.as_deref() == Some(key.as_str()) {
                    None
                } else {
                    Some(key)
                };
            }
            Action::Fullscreen(key) => {
                self.model.fullscreen = if self.model.fullscreen.is_some() {
                    None
                } else {
                    Some(key)
                };
                self.sync_fullscreen();
            }
            Action::Mute(key) => {
                if let Some(local) = self.model.watching.iter_mut().find(|w| w.key == key) {
                    local.muted = !local.muted;
                    if let Some(watching) = self.watches.iter().find(|w| w.id == local.id) {
                        watching.set_muted(local.muted);
                    }
                }
            }
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
                    .notify("Isso não é um convite de sala (começa com nitraroom:)."),
            },
            Action::Rename { room, name } => self.send(&room, Command::Rename(name)),
            Action::Remove { room, key } => self.send(&room, Command::RemoveMember(key)),
            Action::Leave(room) => {
                if self.model.sharing.as_ref().is_some_and(|s| s.room == room) {
                    self.stop_share();
                }
                self.stop_watches_in(&room);
                self.send(&room, Command::Leave);
            }
        }
    }

    /// The name others see in the rooms: the chosen one, else the Windows
    /// user's (until the first-start question is answered).
    fn display_name(&self) -> String {
        self.settings
            .get()
            .name
            .clone()
            .or_else(|| {
                std::env::var("USERNAME")
                    .ok()
                    .and_then(|n| settings::clean_name(&n))
            })
            .unwrap_or_else(|| self.setup.name.clone())
    }

    /// What the rest of the app takes from the settings.
    fn apply_settings(&mut self) {
        self.model.my_name = self.display_name();
        self.model.quality_label = self.settings.get().quality.label().to_owned();
        self.events
            .close_to_tray
            .store(self.settings.get().close_to_tray, Ordering::Relaxed);
    }

    /// Saves new settings; a new name reaches the rooms by reconnecting to
    /// them (the room takes the name at sign-in and tells the others).
    fn save_settings(&mut self, settings: settings::Settings) {
        let renamed = settings.name != self.settings.get().name;
        if let Err(error) = self.settings.set(settings) {
            self.view
                .notify(format!("Não deu para salvar as configurações: {error}"));
            return;
        }
        self.apply_settings();
        if renamed {
            self.reconnect_all();
        }
    }

    /// New connections to every room (a new name, or after sleeping), and
    /// back in the room where we were (a new connection starts out).
    fn reconnect_all(&mut self) {
        for saved in self.store.rooms().to_vec() {
            self.disconnect(&saved.id);
            self.connect(&saved, Join::Member);
        }
        if let Some(room) = self.model.entered.clone() {
            self.send(&room, Command::Enter);
        }
    }

    /// The PC is about to sleep: the network goes with it, so the share and
    /// the watched screens end now, cleanly (the others get a goodbye).
    fn before_sleep(&mut self) {
        let had_media = self.has_media();
        self.stop_share();
        let ids: Vec<u64> = self.model.watching.iter().map(|w| w.id).collect();
        for id in ids {
            self.stop_watch(id);
        }
        tracing::info!(had_media, "suspending");
        if had_media {
            self.view
                .notify("O PC foi suspenso: a transmissão e as telas assistidas pararam.");
        }
    }

    /// "Apagar meus dados deste PC": stops sharing and watching, starts this
    /// program again to erase the profile folder once this process has
    /// ended (its log is open until then), and closes.
    fn erase_data(&mut self) {
        self.stop_share();
        let ids: Vec<u64> = self.model.watching.iter().map(|w| w.id).collect();
        for id in ids {
            self.stop_watch(id);
        }
        let started = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .args(crate::erase::helper_args(&self.setup.dir))
                .spawn()
        });
        match started {
            Ok(_) => {
                tracing::info!("erasing this profile, then closing");
                window::quit(self.window);
            }
            Err(error) => self
                .view
                .notify(format!("Não deu para apagar os dados: {error}")),
        }
    }

    /// Shares `source` in `room`, after asking on a GPU nitra was not tested
    /// on (until the answer is remembered).
    fn share_or_ask(&mut self, room: String, source: share::ShareSource) {
        let untested = self
            .model
            .gpu
            .as_ref()
            .is_some_and(|gpu| gpu.support == crate::hardware::Support::Experimental);
        if untested && !self.settings.get().untested_gpu_ok {
            self.view.open_untested_gpu(room, source);
        } else {
            self.start_share(room, &source);
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
                name: self.display_name(),
                dir: self.setup.dir.clone(),
                identity: Arc::clone(&self.setup.identity),
            },
            crate::room::RoomArgs {
                server,
                room: room.clone(),
                join: Join::Member,
                invite: false,
            },
            source,
            &share::ShareOptions::from_settings(self.settings.get()),
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

    /// Watches `key`'s screen in `room`, inside this window. Watching is
    /// being in the room, so it enters it too.
    fn start_watch(&mut self, room: String, key: &str) {
        if self.model.watch_of(&room, key).is_some() {
            return;
        }
        let Some(server) = self.model.room(&room).map(|r| r.server.clone()) else {
            return;
        };
        let Ok(host) = key.parse::<identity::PublicKey>() else {
            return;
        };
        if self.model.entered.as_deref() != Some(room.as_str()) {
            self.act(Action::Enter(room.clone()));
        }
        self.next_watch += 1;
        let id = self.next_watch;
        let wake = Arc::clone(&self.events);
        let started = watch::Watching::start(
            id,
            share::ProfileParts {
                name: self.display_name(),
                dir: self.setup.dir.clone(),
                identity: Arc::clone(&self.setup.identity),
            },
            watch::WatchTarget {
                server,
                room: room.clone(),
                host,
            },
            // The window's own device: watching adds no device. Safe beside
            // the window's drawing: the viewer's calls use no pipeline state.
            self.device.clone(),
            self.window.0 as isize,
            self.watch_sender.clone(),
            move || wake.wake(),
        );
        match started {
            Ok(watching) => {
                tracing::info!(id, %key, "watching");
                self.watches.push(watching);
                self.model.watching.push(model::LocalWatch {
                    id,
                    room,
                    key: key.to_owned(),
                    muted: false,
                });
            }
            Err(error) => self.view.notify(format!("Não deu para assistir: {error}")),
        }
    }

    /// Stops watching; the thread reports back when it is done.
    fn stop_watch(&mut self, id: u64) {
        if let Some(watching) = self.watches.iter_mut().find(|w| w.id == id) {
            watching.place(None);
            watching.stop();
        }
        self.model.forget_watch(id);
        self.sync_fullscreen();
    }

    fn stop_watches_in(&mut self, room: &str) {
        let ids: Vec<u64> = self
            .model
            .watching
            .iter()
            .filter(|w| w.room == room)
            .map(|w| w.id)
            .collect();
        for id in ids {
            self.stop_watch(id);
        }
    }

    fn watch_ended(&mut self, ended: watch::WatchEnded) {
        if let Some(index) = self.watches.iter().position(|w| w.id == ended.id) {
            // The thread has finished: joining it is immediate.
            self.watches.swap_remove(index).finish();
        }
        if self.watches.is_empty() {
            self.trim_video_memory();
        }
        self.model.forget_watch(ended.id);
        self.sync_fullscreen();
        if let Some(error) = ended.error {
            self.view.notify(error);
        }
    }

    /// Hands back what the driver kept from the decoders (they ran on the
    /// window's device, which lives on): `IDXGIDevice3::Trim` after
    /// `ClearState`, on this thread, between frames (Dear ImGui sets all its
    /// state again every frame).
    fn trim_video_memory(&self) {
        use windows::Win32::Graphics::Dxgi::IDXGIDevice3;
        let Ok(context) = self.device.immediate_context() else {
            return;
        };
        // SAFETY: valid device and its immediate context, used on the
        // render thread between frames.
        unsafe {
            context.ClearState();
            if let Ok(dxgi) = self.device.raw().cast::<IDXGIDevice3>() {
                dxgi.Trim();
            }
        }
    }

    /// Puts the window in full screen or back, as the model asks.
    fn sync_fullscreen(&mut self) {
        let wanted = self.model.fullscreen.is_some();
        if wanted != self.fullscreen {
            self.fullscreen = wanted;
            // SAFETY: posting to our window; the window thread toggles it.
            let _ = unsafe {
                PostMessageW(
                    Some(self.window),
                    window::WM_TOGGLE_FULLSCREEN,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
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

    /// A sealed preview `from` sent in `room`: opened, decoded and kept as a
    /// texture while their screen is live. Anything that does not open
    /// (not for us, altered, not a picture) is dropped.
    fn receive_preview(&mut self, room: &str, from: &identity::PublicKey, data: &str) {
        let Some(jpeg) = crate::preview::open_from_room(&self.setup.identity, room, from, data)
        else {
            tracing::warn!(%from, "preview did not open; ignored");
            return;
        };
        let picture = match crate::preview::picture_from_jpeg(&jpeg) {
            Ok(picture) => picture,
            Err(error) => {
                tracing::warn!(%from, %error, "preview is not a picture; ignored");
                return;
            }
        };
        let view = match preview_texture(&self.device, &picture) {
            Ok(view) => view,
            Err(error) => {
                tracing::warn!(%error, "preview texture");
                return;
            }
        };
        let image = model::PreviewImage {
            texture: view.as_raw() as u64,
            width: picture.width,
            height: picture.height,
        };
        let key = from.to_string().replace('-', "");
        if self.model.set_preview(room, &key, image) {
            self.preview_views.insert(image.texture, view);
        }
        self.free_previews();
    }

    /// Releases the textures no preview uses any more.
    fn free_previews(&mut self) {
        let used = self.model.preview_textures();
        self.preview_views
            .retain(|texture, _| used.contains(texture));
    }

    fn handle(&mut self, room: &str, event: RoomEvent) {
        if let RoomEvent::Preview { from, data } = &event {
            self.receive_preview(room, from, data);
            return;
        }
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
        match self.events.power.swap(0, Ordering::Relaxed) {
            window::POWER_SUSPENDING => self.before_sleep(),
            window::POWER_RESUMED => {
                tracing::info!("resumed: reconnecting to the rooms");
                self.reconnect_all();
            }
            _ => {}
        }
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
        // Screens that ended took their previews: free those textures.
        self.free_previews();
        while let Ok(ended) = self.share_events.try_recv() {
            self.share_ended(ended);
            self.redraw = self.redraw.max(1);
        }
        while let Ok(ended) = self.watch_events.try_recv() {
            self.watch_ended(ended);
            self.redraw = self.redraw.max(1);
        }
        // A sharer stopped or left: stop watching them.
        for id in self.model.watches_without_screen() {
            self.stop_watch(id);
            self.redraw = self.redraw.max(1);
        }
        // Live numbers (watched screens, who watches ours): once a second.
        if self.has_media() && self.last_stats.elapsed() >= Duration::from_secs(1) {
            self.last_stats = Instant::now();
            self.redraw = self.redraw.max(1);
        }
        let resized = self.events.resized.swap(false, Ordering::Relaxed);
        if resized {
            self.gfx.resize(window::client_size(self.window))?;
        }
        if self.view.animating() {
            self.redraw = self.redraw.max(1);
        }
        // The tray's menu shows where we are.
        let status = self.model.my_status();
        let mut tray = self.events.tray_status.lock().expect("tray status");
        if *tray != status {
            *tray = status;
        }
        drop(tray);
        if !self.visible() {
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
        if self.redraw > 0 && self.visible() {
            self.ui_wait()
        } else if self.has_media() {
            // Wake for the once-a-second numbers.
            1000_u32.saturating_sub(self.last_stats.elapsed().as_millis() as u32)
        } else {
            u32::MAX
        }
    }

    /// Whether there is anything on screen to draw: not minimized, not
    /// hidden to the tray. (Waiting for a frame while invisible would spin.)
    fn visible(&self) -> bool {
        !self.events.minimized.load(Ordering::Relaxed)
            && !self.events.hidden.load(Ordering::Relaxed)
    }

    /// Something live to show numbers of (watched screens, our share).
    fn has_media(&self) -> bool {
        !self.watches.is_empty() || self.model.sharing.is_some()
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
        // The live numbers, as of now.
        for watching in &self.watches {
            if let Some(local) = self.model.watching.iter().find(|w| w.id == watching.id) {
                self.model
                    .watch_stats
                    .insert(local.key.clone(), watching.stats());
            }
        }
        self.model.audience = self
            .model
            .sharing
            .as_ref()
            .and_then(|s| self.shares.iter().find(|sh| sh.id == s.id))
            .map(share::Sharing::audience)
            .unwrap_or_default();
        let actions = view::build(
            ui,
            &self.fonts,
            &self.model,
            &mut self.view,
            &chrome,
            &self.art,
        );
        // The watched pictures where the view put them (hidden otherwise).
        for watching in &mut self.watches {
            let rect = self
                .view
                .video_rects
                .iter()
                .find(|(id, _)| *id == watching.id)
                .map(|(_, [x, y, w, h])| [*x as i32, *y as i32, *w as i32, *h as i32]);
            watching.place(rect);
        }
        // Where the title bar and its buttons are, for the window thread
        // (none in full screen).
        let width = ui.io().display_size()[0];
        let caption = if self.fullscreen {
            0
        } else {
            view::TITLE_H as i32
        };
        self.events.caption_height.store(caption, Ordering::Relaxed);
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
        // Dear ImGui takes queued input a little per frame (a burst of keys
        // or a paste spreads over frames): keep drawing until it is all in.
        if pending_input() > 0 {
            self.redraw = self.redraw.max(1);
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
            self.watches
                .drain(..)
                .map(|watching| std::thread::spawn(move || watching.finish())),
        );
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

/// Input events Dear ImGui has queued but not taken yet.
fn pending_input() -> i32 {
    // SAFETY: reads one field of the current context, on its own thread,
    // between frames (nothing else touches it then); null when none.
    unsafe {
        let context = dear_imgui_sys::igGetCurrentContext();
        if context.is_null() {
            0
        } else {
            (*context).InputEventsQueue.Size
        }
    }
}

/// A preview picture as an immutable texture Dear ImGui can draw (by its
/// view). A few hundred KiB of UI pixels, made once per preview.
fn preview_texture(
    device: &D3d11Device,
    picture: &crate::preview::Picture,
) -> Result<ID3D11ShaderResourceView, String> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_IMMUTABLE,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
    let desc = D3D11_TEXTURE2D_DESC {
        Width: picture.width,
        Height: picture.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: picture.rgba.as_ptr().cast(),
        SysMemPitch: picture.width * 4,
        SysMemSlicePitch: 0,
    };
    let (mut texture, mut view) = (None, None);
    // SAFETY: valid device; `rgba` holds width * height * 4 bytes (the
    // decoder made it so) and lives through the call; out-pointers are locals.
    unsafe {
        device
            .raw()
            .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
            .map_err(|e| e.to_string())?;
        let texture = texture.ok_or("no texture")?;
        device
            .raw()
            .CreateShaderResourceView(&texture, None, Some(&mut view))
            .map_err(|e| e.to_string())?;
    }
    view.ok_or_else(|| "no texture view".to_owned())
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
/// Loads a font file from Windows' font folder (the icon font).
fn add_font_file(
    context: &mut Context,
    path: &str,
    size: f32,
) -> Result<(dear_imgui_rs::FontId, f32), String> {
    let bytes: &'static [u8] = Box::leak(
        std::fs::read(path)
            .map_err(|e| format!("font {path}: {e}"))?
            .into_boxed_slice(),
    );
    add_font(context, path, bytes, size)
}

fn add_font(
    context: &mut Context,
    name: &str,
    bytes: &'static [u8],
    size: f32,
) -> Result<(dear_imgui_rs::FontId, f32), String> {
    // Validated as a TrueType font first (the FreeType loader reads it raw).
    dear_imgui_rs::StbTrueTypeFontData::from_slice(bytes)
        .map_err(|e| format!("font {name}: {e:?}"))?;
    let config = dear_imgui_rs::FontConfig::new()
        .pixel_snap_h(true)
        .font_loader_flags(dear_imgui_rs::FontLoaderFlags(font_flags()))
        .rasterizer_multiply(font_multiply());
    // The atlas's own loader (FreeType, which hints glyphs to the pixel grid);
    // `stb_truetype_with_size` would pin stb_truetype, which does not hint
    // (that blurred the prototype's text).
    // SAFETY: `bytes` is a complete font (checked above) that lives for the
    // process unchanged (built in, or leaked from the file).
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
    // This thread decodes previews with WIC (COM). Never left: the thread
    // ends with the window.
    // SAFETY: joins the MTA for this thread.
    let _ = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };
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
        for (name, bytes) in FONTS {
            loaded.push(add_font(&mut context, name, bytes, FONT_SIZE)?);
        }
        // The window button glyphs: Windows 11's icon font, or Windows 10's.
        let icon_path = [
            r"C:\Windows\Fonts\SegoeIcons.ttf",
            r"C:\Windows\Fonts\segmdl2.ttf",
        ]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .ok_or("no Windows icon font (Segoe Fluent Icons or MDL2 Assets)")?;
        let icons = add_font_file(&mut context, icon_path, 10.0)?.0;
        let fonts = view::Fonts {
            regular: loaded[0].0,
            medium: loaded[1].0,
            semibold: loaded[2].0,
            bold: loaded[3].0,
            italic: loaded[4].0,
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
        let (art, logo_view) = load_art(&device);
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
        let (settings, settings_warning) =
            settings::SettingsStore::load(&setup.dir).map_err(|e| e.to_string())?;
        let me = setup.identity.public_key().to_string().replace('-', "");
        let mut model = Model::new(me, store.rooms());
        model.gpu = Some(setup.gpu.clone());
        let (room_sender, room_events) = mpsc::channel();
        let (share_sender, share_events) = mpsc::channel();
        let (watch_sender, watch_events) = mpsc::channel();
        let mut app = App {
            window,
            events: window::events(),
            gfx,
            context,
            textures,
            fonts,
            art,
            _logo_view: logo_view,
            view: ViewState::default(),
            model,
            store,
            settings,
            clients: HashMap::new(),
            room_events,
            room_sender,
            shares: Vec::new(),
            share_events,
            share_sender,
            next_share: 0,
            watches: Vec::new(),
            watch_events,
            watch_sender,
            next_watch: 0,
            fullscreen: false,
            last_stats: Instant::now(),
            device: device.clone(),
            preview_views: HashMap::new(),
            setup,
            redraw: FRAMES_AFTER_INPUT,
            last_draw: None,
            frames: 0,
        };
        if let Some(warning) = warning.or(settings_warning) {
            app.view.notify(warning);
        }
        if std::env::var_os(GPU_RESTARTS_VAR).is_some() {
            app.view.notify(
                "A placa de vídeo reiniciou e o nitra foi reaberto. Transmissões e telas assistidas pararam.",
            );
        }
        app.apply_settings();
        // The first start asks for the name others will see.
        if app.settings.get().name.is_none() {
            // Empty, with a hint: the Windows account name is often not a
            // name ("User", the start of an email address).
            app.view.open_welcome(String::new());
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
    let result = app.run().map_err(|error| {
        // Any failure with the device removed is a lost GPU, however it
        // showed (Present, resizing, a texture).
        // SAFETY: valid device; only queries its state.
        let removed = unsafe { device.raw().GetDeviceRemovedReason() }.is_err();
        if removed && !error.starts_with(gfx::DEVICE_LOST) {
            format!("{}: {error}", gfx::DEVICE_LOST)
        } else {
            error
        }
    });
    if result.is_err() {
        // Nothing draws any more: close the window (the main thread then
        // reports the error, or reopens the app after a lost GPU).
        window::quit(window);
    }
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

/// FreeType loader flags for the interface fonts (`NITRA_FT_FLAGS` overrides,
/// for comparing renderings). 0: the font's own TrueType hinting, which
/// Segoe UI is built for (the prototype's auto-hinter suited Inter, which has
/// little hinting of its own).
fn font_flags() -> u32 {
    std::env::var("NITRA_FT_FLAGS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Rasterizer alpha multiply (`NITRA_RASTER_MULT` overrides).
fn font_multiply() -> f32 {
    std::env::var("NITRA_RASTER_MULT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0)
}

/// The logo, drawn at 1x by `scripts/make-icon.ps1` (the window does not
/// scale with the display yet; the 1.5x and 2x copies are there for when it
/// does).
const LOGO_PNG: &[u8] = include_bytes!("../../assets/logo@1x.png");

/// Loads the view's pictures on the window's device. A picture that fails
/// is left out (the view draws a stand-in) rather than stopping the app.
fn load_art(device: &D3d11Device) -> (view::Art, Option<ID3D11ShaderResourceView>) {
    let loaded = crate::preview::picture_from_png(LOGO_PNG)
        .map_err(|e| e.to_string())
        .and_then(|picture| {
            preview_texture(device, &picture).map(|view| (view, picture.width, picture.height))
        });
    match loaded {
        Ok((view, width, height)) => {
            let logo = view::Picture {
                texture: view.as_raw() as u64,
                size: [width as f32, height as f32],
            };
            (view::Art { logo: Some(logo) }, Some(view))
        }
        Err(error) => {
            tracing::warn!(%error, "logo picture");
            (view::Art { logo: None }, None)
        }
    }
}
