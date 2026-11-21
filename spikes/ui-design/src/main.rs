//! Visual prototype of the ziz interface (design brief: dark, sober theme),
//! in Dear ImGui: the rooms in a sidebar and the selected room with its live
//! screens, with real hardware-decoded video in the tiles.
//!
//! Two threads, as in the UI prototype: the main thread owns the window and
//! only handles messages (`window.rs`); the render thread owns the swap
//! chain, Dear ImGui, the decoders and the video textures, and draws only
//! when something changed (input, a resize, a new picture on screen).
//!
//! Flags: `--seconds N`, `--at X,Y`, `--no-video`, and a starting state for
//! screenshots: `--room I` (selected), `--joined`, `--focus K`, `--maximize K`
//! (screen K of the room), `--sidebar W` (width), `--collapsed`.

mod gfx;
mod streams;
mod textures;
mod tiles;
mod ui;
mod window;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dear_imgui_rs::{Context, FontSource};
use dear_imgui_sys::backend_shim::{dx11, win32};
use gpu::{D3d11Device, DebugLayer};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId, WaitForMultipleObjects};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, GetWindowThreadProcessId, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, ShowWindow, TranslateMessage,
    WM_CLOSE, WM_QUIT,
};
use windows::core::Interface;

const FRAMES_AFTER_INPUT: u32 = 3;
const UI_INTERVAL: Duration = Duration::from_micros(16_667);
/// How often the statistics of the maximized screen are refreshed.
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// How often a preview's still is refreshed (before joining the room).
const STILL_INTERVAL: Duration = Duration::from_secs(10);
const LAB: &str = r"D:\Projects\live\lab";

static INTER_REGULAR: &[u8] = include_bytes!(r"D:\Projects\live\lab\fonts\Inter-Regular.ttf");
static INTER_MEDIUM: &[u8] = include_bytes!(r"D:\Projects\live\lab\fonts\Inter-Medium.ttf");
static INTER_SEMIBOLD: &[u8] = include_bytes!(r"D:\Projects\live\lab\fonts\Inter-SemiBold.ttf");
static INTER_BOLD: &[u8] = include_bytes!(r"D:\Projects\live\lab\fonts\Inter-Bold.ttf");
static BRICOLAGE: &[u8] = include_bytes!(r"D:\Projects\live\lab\fonts\BricolageGrotesque-ExtraBold-opsz32.ttf");

#[derive(Default)]
struct Args {
    seconds: Option<f64>,
    room: Option<usize>,
    joined: bool,
    focus: Option<usize>,
    maximize: Option<usize>,
    sidebar: Option<f32>,
    collapsed: bool,
    no_video: bool,
    at: Option<(i32, i32)>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--seconds" => args.seconds = Some(value()?.parse().map_err(|_| "bad --seconds")?),
            "--room" => args.room = Some(value()?.parse().map_err(|_| "bad --room")?),
            "--joined" => args.joined = true,
            "--focus" => args.focus = Some(value()?.parse().map_err(|_| "bad --focus")?),
            "--maximize" => args.maximize = Some(value()?.parse().map_err(|_| "bad --maximize")?),
            "--sidebar" => args.sidebar = Some(value()?.parse().map_err(|_| "bad --sidebar")?),
            "--collapsed" => args.collapsed = true,
            "--no-video" => args.no_video = true,
            "--at" => {
                let v = value()?;
                let (x, y) = v.split_once(',').ok_or("--at needs X,Y")?;
                args.at = Some((x.trim().parse().map_err(|_| "bad --at")?, y.trim().parse().map_err(|_| "bad --at")?));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

struct Summary {
    frames: u64,
    conversions: u64,
}

struct App {
    window: HWND,
    events: Arc<window::Events>,
    gfx: gfx::Gfx,
    context: Context,
    textures: textures::TextureManager,
    fonts: ui::Fonts,
    state: ui::State,
    streams: streams::Streams,
    tiles: tiles::Tiles,
    redraw: u32,
    last_draw: Option<Instant>,
    frames: u64,
    /// Decoded pictures and bytes per stream at the last statistics refresh.
    stats_mark: (Instant, Vec<(u64, u64)>),
    /// When each preview still was last taken.
    stills: Vec<Option<Instant>>,
}

impl App {
    fn minimized(&self) -> bool {
        self.events.minimized.load(Ordering::Relaxed)
    }

    fn step(&mut self) -> Result<(), String> {
        let input = self.events.take_input();
        if !input.is_empty() {
            for (message, wparam, lparam) in input {
                // SAFETY: Dear ImGui's state is owned by this thread.
                unsafe { win32::dear_imgui_backend_win32_wnd_proc_handler(self.window.0, message, wparam, lparam) };
            }
            self.redraw = FRAMES_AFTER_INPUT;
        }
        let resized = self.events.resized.swap(false, Ordering::Relaxed);
        if resized {
            self.gfx.resize(window::client_size(self.window))?;
        }
        // In the room we joined, the screens on screen are live. Before
        // joining, the preview is a still: a screen wakes only to take one
        // picture every STILL_INTERVAL, then pauses (in the real app the still
        // would come as a small image through signaling, with no video
        // connection at all). Screens not on screen are paused.
        let visible = self.state.visible_streams();
        let live = self.state.is_live();
        let mut new_video = false;
        for index in 0..self.streams.len() {
            let due = self.stills[index].is_none_or(|t| t.elapsed() >= STILL_INTERVAL);
            let want = visible.contains(&index) && (live || due);
            self.streams.set_active(index, want);
            let Some(frame) = self.streams.poll(index)? else { continue };
            if !want {
                continue;
            }
            if !live {
                // A few pictures past the keyframe the stream restarted at.
                if self.streams.since_resume(index) < 3 {
                    continue;
                }
                self.stills[index] = Some(Instant::now());
                self.streams.set_active(index, false);
            }
            self.tiles.set_frame(index, &frame)?;
            new_video = true;
        }
        self.update_stats();
        if self.minimized() {
            return Ok(());
        }
        let ui_due = self.redraw > 0 && self.ui_wait() == 0;
        if !(resized || new_video || ui_due) {
            return Ok(());
        }
        self.redraw = self.redraw.saturating_sub(1);
        self.last_draw = Some(Instant::now());
        self.draw()
    }

    fn ui_wait(&self) -> u32 {
        self.last_draw.map_or(0, |last| UI_INTERVAL.saturating_sub(last.elapsed()).as_micros().div_ceil(1000) as u32)
    }

    fn sleep_ms(&self) -> u32 {
        let ui = if self.redraw > 0 && !self.minimized() { self.ui_wait() } else { u32::MAX };
        // In a preview, wake up for the next still of each screen on screen.
        let still = if self.state.is_live() {
            u32::MAX
        } else {
            self.state
                .visible_streams()
                .into_iter()
                // An active stream wakes us with its pictures.
                .filter(|&i| !self.streams.is_active(i))
                .map(|i| self.stills[i].map_or(0, |t| STILL_INTERVAL.saturating_sub(t.elapsed()).as_millis() as u32 + 1))
                .min()
                .unwrap_or(u32::MAX)
        };
        ui.min(still)
    }

    /// Frame rate and bitrate per stream over the last second (shown with a
    /// maximized screen).
    fn update_stats(&mut self) {
        let elapsed = self.stats_mark.0.elapsed();
        if elapsed < STATS_INTERVAL {
            return;
        }
        let seconds = elapsed.as_secs_f32();
        for index in 0..self.streams.len().min(self.state.stats.len()) {
            let (pictures, bytes) = self.streams.totals(index);
            let (last_pictures, last_bytes) = self.stats_mark.1[index];
            self.state.stats[index] = ui::StreamStats {
                fps: (pictures - last_pictures) as f32 / seconds,
                kbps: (bytes - last_bytes) as f32 * 8.0 / 1000.0 / seconds,
            };
            self.stats_mark.1[index] = (pictures, bytes);
        }
        self.stats_mark.0 = Instant::now();
    }

    fn draw(&mut self) -> Result<(), String> {
        if self.tiles.begin_frame()? {
            self.redraw = self.redraw.max(1);
        }
        self.gfx.wait_ready();
        self.gfx.begin(ui::clear_color())?;
        // SAFETY: both backends were initialized for this context and window on this thread.
        unsafe {
            dx11::dear_imgui_backend_dx11_new_frame();
            win32::dear_imgui_backend_win32_new_frame();
        }
        let ui = self.context.frame();
        let tiles = &mut self.tiles;
        let mut source = |index: usize, [w, h]: [f32; 2], cover: bool| -> Option<(u64, (u32, u32))> {
            let picture = tiles.picture_size(index)?;
            let aspect = picture.0 as f32 / picture.1.max(1) as f32;
            // The picture's drawn size: covering the rect, or inside it.
            let wider = w / h > aspect;
            let (pw, ph) = if wider == cover { (w, w / aspect) } else { (h * aspect, h) };
            tiles.texture(index, (pw.round() as u32, ph.round() as u32)).map(|id| (id, picture))
        };
        ui::build(ui, &self.fonts, &mut self.state, &mut source);
        // SAFETY: the context is current on this thread, inside the frame.
        self.events.set_cursor(unsafe { dear_imgui_sys::igGetMouseCursor() });
        self.tiles.render()?;
        let pending = self.context.render(self.textures.consumer());
        let frame = self.textures.reconcile(pending)?;
        let draw_data = std::ptr::from_ref(frame.draw_data()).cast::<dear_imgui_sys::ImDrawData>().cast_mut();
        // SAFETY: `DrawData` is a transparent wrapper of `ImDrawData`; every
        // texture it uses is alive (font atlas uploaded above, video tiles
        // only replaced in `begin_frame`, before the next build).
        unsafe { dx11::dear_imgui_backend_dx11_render_draw_data(draw_data) };
        drop(frame);
        self.gfx.present()?;
        self.frames += 1;
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        while !self.events.closing.load(Ordering::Relaxed) {
            let handles = [self.events.wake_handle(), self.streams.ready_event()];
            // SAFETY: valid handles; returns on a signal or the timeout.
            let _ = unsafe { WaitForMultipleObjects(&handles, false, self.sleep_ms()) };
            self.step()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Summary, String> {
        self.streams.stop();
        self.textures.shutdown(&mut self.context)?;
        // SAFETY: matching shutdowns, renderer first, before the context drops.
        unsafe {
            dx11::dear_imgui_backend_dx11_shutdown();
            win32::dear_imgui_backend_win32_shutdown();
        }
        Ok(Summary { frames: self.frames, conversions: self.tiles.conversions })
    }
}

fn add_font(context: &mut Context, bytes: &'static [u8], size: f32) -> Result<dear_imgui_rs::FontId, String> {
    let data = dear_imgui_rs::StbTrueTypeFontData::from_slice(bytes).map_err(|e| format!("font: {e:?}"))?;
    // Advances snapped to whole pixels: glyphs land on the pixel grid (sharper).
    // Experiment knobs: FreeType loader flags and the rasterizer multiply
    // (alpha boost: light text on a dark background reads thin and soft).
    // Default: FreeType auto-hinter (stems on whole pixels, crisper) and a
    // light alpha boost; measured against the browser in labI-design.
    let flags = std::env::var("ZIZ_FT_FLAGS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let multiply = std::env::var("ZIZ_RASTER_MULT").ok().and_then(|v| v.parse().ok()).unwrap_or(1.2);
    let config = dear_imgui_rs::FontConfig::new()
        .pixel_snap_h(true)
        .font_loader_flags(dear_imgui_rs::FontLoaderFlags(flags))
        .rasterizer_multiply(multiply);
    Ok(context.font_atlas().add_font(&[FontSource::stb_truetype_with_size(data, size).with_config(config)]))
}

fn render_thread(device: D3d11Device, window: isize, args: Args, ready: mpsc::SyncSender<Result<(), String>>) -> Result<Summary, String> {
    let window = HWND(window as *mut _);
    // SAFETY: joins COM for this thread (Media Foundation decoders); left at the end.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.map_err(|e| e.to_string())?;
    let result = (|| {
        let _media_foundation = mf::MediaFoundation::start().map_err(|e| e.to_string())?;
        let setup = (|| -> Result<App, String> {
            // SAFETY: plain thread queries; both threads belong to this process.
            unsafe {
                let window_thread = GetWindowThreadProcessId(window, None);
                let _ = AttachThreadInput(GetCurrentThreadId(), window_thread, true);
            }
            let gfx = gfx::Gfx::new(&device, window, window::client_size(window))?;
            let mut context = Context::create();
            context.set_ini_filename(None::<String>).map_err(|e| e.to_string())?;
            let fonts = ui::Fonts {
                regular: add_font(&mut context, INTER_REGULAR, 15.0)?,
                medium: add_font(&mut context, INTER_MEDIUM, 15.0)?,
                semibold: add_font(&mut context, INTER_SEMIBOLD, 15.0)?,
                bold: add_font(&mut context, INTER_BOLD, 15.0)?,
                logo: add_font(&mut context, BRICOLAGE, 32.0)?,
                inter_scale: line_height_per_em(INTER_REGULAR),
                logo_scale: line_height_per_em(BRICOLAGE),
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
                if name.is_null() { "?".to_owned() } else { std::ffi::CStr::from_ptr(name).to_string_lossy().into_owned() }
            };
            println!("font loader: {loader}");
            let specs = [("1080p60", 60), ("720p60", 60), ("1080p30", 30)].map(|(name, fps)| streams::StreamSpec {
                file: format!(r"{LAB}\viewer\streams\{name}.h264").into(),
                fps,
            });
            // --no-video: the interface alone (no files loaded, no decoders).
            let streams = streams::Streams::start(&device, if args.no_video { &[] } else { &specs })?;
            let tiles = tiles::Tiles::new(&device, specs.len())?;
            let mut state = ui::State::default();
            state.configure(args.room, args.joined, args.focus, args.maximize, args.sidebar, args.collapsed);
            Ok(App {
                window,
                events: window::events(),
                gfx,
                context,
                textures,
                fonts,
                state,
                streams,
                tiles,
                redraw: FRAMES_AFTER_INPUT,
                last_draw: None,
                frames: 0,
                stats_mark: (Instant::now(), vec![(0, 0); 3]),
                stills: vec![None; 3],
            })
        })();
        let mut app = match setup {
            Ok(app) => app,
            Err(message) => {
                let _ = ready.send(Err(message.clone()));
                return Err(message);
            }
        };
        let _ = ready.send(Ok(()));
        let result = app.run();
        let summary = app.finish();
        result.and(summary)
    })();
    // SAFETY: balances CoInitializeEx above, after every COM object is gone.
    unsafe { CoUninitialize() };
    result
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    // SAFETY: process-wide setting, before any window exists.
    unsafe { win32::dear_imgui_backend_win32_enable_dpi_awareness() };
    let adapters = gpu::enumerate_adapters().map_err(|e| e.to_string())?;
    let adapter = adapters.iter().find(|a| !a.info.is_software).ok_or("no hardware adapter")?;
    let debug = if cfg!(debug_assertions) { DebugLayer::IfAvailable } else { DebugLayer::Off };
    let device = gpu::create_device(adapter, debug).map_err(|e| e.to_string())?;
    // Decoders (DXVA) and the render thread share it; no other thread sets
    // pipeline state on it (the flicker rule).
    device.enable_multithread_protection().map_err(|e| e.to_string())?;

    let window = window::create("ziz", 1280, 800)?;
    let events = window::events();
    let (seconds, at) = (args.seconds, args.at);
    let (ready_sender, ready) = mpsc::sync_channel(1);
    let render: JoinHandle<Result<Summary, String>> = {
        let (device, raw) = (device.clone(), window.0 as isize);
        std::thread::Builder::new()
            .name("render".to_owned())
            .spawn(move || render_thread(device, raw, args, ready_sender))
            .map_err(|e| e.to_string())?
    };
    if let Err(message) = ready.recv().unwrap_or_else(|_| Err("render thread exited".to_owned())) {
        let _ = render.join();
        // SAFETY: window owned by this thread.
        let _ = unsafe { DestroyWindow(window) };
        return Err(message);
    }
    let render = std::rc::Rc::new(std::cell::RefCell::new(Some(render)));
    let summary = std::rc::Rc::new(std::cell::RefCell::new(None));
    {
        let (render, summary, events) = (render.clone(), summary.clone(), Arc::clone(&events));
        window::set_on_close(move || {
            events.closing.store(true, Ordering::Relaxed);
            events.wake();
            if let Some(handle) = render.borrow_mut().take() {
                *summary.borrow_mut() = Some(handle.join().unwrap_or_else(|_| Err("render thread panicked".to_owned())));
            }
        });
    }
    // SAFETY: valid window of this thread.
    unsafe {
        if let Some((x, y)) = at {
            let _ = SetWindowPos(window, None, x, y, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER);
        }
        let _ = ShowWindow(window, SW_SHOW);
    }
    let started = Instant::now();
    let mut close_sent = false;
    'run: loop {
        let mut timeout = u32::MAX;
        if let Some(seconds) = seconds {
            let left = (seconds - started.elapsed().as_secs_f64()).max(0.0);
            timeout = (left * 1000.0).ceil() as u32;
            if left == 0.0 && !close_sent {
                close_sent = true;
                // SAFETY: valid window of this thread.
                let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
        }
        // SAFETY: no handles; returns on a message or the timeout.
        let _ = unsafe { MsgWaitForMultipleObjectsEx(None, timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
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
    let summary = summary.borrow_mut().take().unwrap_or_else(|| Err("render thread not joined".to_owned()))?;
    println!(
        "ui-design: {} frames drawn, {} video conversions in {:.1} s",
        summary.frames,
        summary.conversions,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// (ascent - descent) / units per em, from the font's `hhea` and `head`
/// tables: how much taller Dear ImGui's "pixel height" is than a CSS size.
fn line_height_per_em(font: &[u8]) -> f32 {
    let u16_at = |o: usize| u16::from_be_bytes([font[o], font[o + 1]]);
    let i16_at = |o: usize| i16::from_be_bytes([font[o], font[o + 1]]);
    let u32_at = |o: usize| u32::from_be_bytes([font[o], font[o + 1], font[o + 2], font[o + 3]]);
    let tables = usize::from(u16_at(4));
    let find = |tag: &[u8; 4]| (0..tables).map(|i| 12 + 16 * i).find(|&r| &font[r..r + 4] == tag).map(|r| u32_at(r + 8) as usize);
    let (Some(head), Some(hhea)) = (find(b"head"), find(b"hhea")) else { return 1.0 };
    let units_per_em = f32::from(u16_at(head + 18));
    let (ascent, descent) = (f32::from(i16_at(hhea + 4)), f32::from(i16_at(hhea + 6)));
    (ascent - descent) / units_per_em
}
