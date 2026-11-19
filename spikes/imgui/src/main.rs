//! Dear ImGui prototype: member list, go live / watch,
//! settings, and a 1080p60 hardware-decoded video drawn in the same window
//! (`FrameRenderer`), the UI on top.
//!
//! Two threads, as in `VideoPresenter`: the main thread owns the window and
//! only handles messages (see `window.rs`); the render thread owns the swap
//! chain, Dear ImGui, the textures and the feed, and draws. It sleeps until
//! something happens: input, a window event, or (while watching) a new
//! access unit. With nothing changing, nothing is drawn.
//!
//! Flags: `--watch` (start watching), `--seconds N` (quit after N s),
//! `--minimized`, `--background`, `--resize-test`. Prints start-up time and
//! counters on exit.

mod gfx;
mod textures;
mod ui;
mod window;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dear_imgui_rs::Context;
use dear_imgui_sys::backend_shim::{dx11, win32};
use feed::{ComApartment, Feed, FeedConfig, MediaThread};
use gpu::{AdapterInfo, D3d11Device, DebugLayer};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, WaitForMultipleObjects,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DispatchMessageW, GetWindowThreadProcessId, HWND_BOTTOM, MSG,
    MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostMessageW,
    QS_ALLINPUT, SW_SHOW, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetWindowPos, ShowWindow, TranslateMessage, WM_CLOSE, WM_QUIT,
};
use windows::core::Interface;

/// Frames drawn after an input event (Dear ImGui settles hover/active
/// states over a couple of frames).
const FRAMES_AFTER_INPUT: u32 = 3;
/// With nothing happening, sleep until the next event (no text fields or
/// tooltips here need a periodic redraw).
const IDLE_WAIT_MS: u32 = u32::MAX; // INFINITE
/// Shortest time between two frames drawn for UI changes alone (input):
/// about 60 per second, whatever the monitor's refresh rate.
const UI_INTERVAL: Duration = Duration::from_micros(16_667);

struct Args {
    watch: bool,
    seconds: Option<f64>,
    minimized: bool,
    /// Open behind other windows without taking the focus.
    background: bool,
    /// Resize the window in a loop and count wrongly scaled pictures.
    resize_test: bool,
    /// Window position (screen coordinates, physical pixels).
    at: Option<(i32, i32)>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        watch: false,
        seconds: None,
        minimized: false,
        background: false,
        resize_test: false,
        at: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--watch" => args.watch = true,
            "--minimized" => args.minimized = true,
            "--background" => args.background = true,
            "--resize-test" => {
                args.resize_test = true;
                args.watch = true;
            }
            "--at" => {
                let value = iter.next().ok_or("--at needs X,Y")?;
                let (x, y) = value.split_once(',').ok_or("--at needs X,Y")?;
                args.at = Some((
                    x.trim().parse().map_err(|_| "bad --at")?,
                    y.trim().parse().map_err(|_| "bad --at")?,
                ));
            }
            "--seconds" => {
                let value = iter.next().ok_or("--seconds needs a value")?;
                args.seconds = Some(value.parse().map_err(|_| "bad --seconds")?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

struct Watching {
    member: usize,
    feed: Feed,
    shown: VecDeque<Instant>,
}

/// What the render thread reports when it ends.
struct Summary {
    first_frame_ms: Option<f64>,
    frames: u64,
    video_frames: u64,
    feed: Option<feed::FeedCounters>,
}

/// The render thread's state.
struct App {
    device: D3d11Device,
    adapter: AdapterInfo,
    window: HWND,
    events: Arc<window::Events>,
    gfx: gfx::Gfx,
    context: Context,
    textures: textures::TextureManager,
    state: ui::State,
    watching: Option<Watching>,
    redraw: u32,
    last_draw: Option<Instant>,
    frames: u64,
    video_frames: u64,
    first_frame_ms: Option<f64>,
}

impl App {
    fn watch(&mut self, member: usize) -> Result<(), String> {
        let feed = Feed::start(&self.device, &self.adapter, FeedConfig::default())?;
        self.watching = Some(Watching {
            member,
            feed,
            shown: VecDeque::new(),
        });
        Ok(())
    }

    fn leave(&mut self) -> Result<(), String> {
        self.gfx.clear_video();
        if let Some(watching) = self.watching.take() {
            watching.feed.stop()?;
        }
        Ok(())
    }

    fn minimized(&self) -> bool {
        self.events.minimized.load(Ordering::Relaxed)
    }

    /// Takes new video and window events; draws a frame if one is due:
    /// at once after a resize; while watching, only with a new video
    /// picture (UI changes ride along, at most a frame late); otherwise
    /// for pending UI changes, at most every `UI_INTERVAL`.
    fn step(&mut self) -> Result<(), String> {
        let input = self.events.take_input();
        if !input.is_empty() {
            for (message, wparam, lparam) in input {
                // SAFETY: Dear ImGui's state is owned by this thread; the
                // handler only reads the message (and the window handle).
                unsafe {
                    win32::dear_imgui_backend_win32_wnd_proc_handler(
                        self.window.0,
                        message,
                        wparam,
                        lparam,
                    );
                }
            }
            self.redraw = FRAMES_AFTER_INPUT;
        }
        let resized = self.events.resized.swap(false, Ordering::Relaxed);
        if resized {
            self.gfx.resize(window::client_size(self.window))?;
        }
        let mut new_video = false;
        if let Some(watching) = &mut self.watching
            && let Some(frame) = watching.feed.poll()?
        {
            self.gfx.set_video(&frame)?;
            new_video = true;
        }
        if self.minimized() {
            return Ok(());
        }
        let ui_due = self.redraw > 0 && self.watching.is_none() && self.ui_wait() == 0;
        if !(resized || new_video || ui_due) {
            return Ok(());
        }
        self.redraw = self.redraw.saturating_sub(1);
        self.last_draw = Some(Instant::now());
        self.draw(new_video)
    }

    /// Milliseconds until a pending UI redraw may be drawn (0 = now).
    fn ui_wait(&self) -> u32 {
        self.last_draw.map_or(0, |last| {
            UI_INTERVAL
                .saturating_sub(last.elapsed())
                .as_micros()
                .div_ceil(1000) as u32
        })
    }

    /// How long the render thread may sleep (it also wakes on window events
    /// and video).
    fn sleep_ms(&self) -> u32 {
        // Minimized, nothing is drawn: a pending redraw must not keep the
        // thread awake (restoring the window wakes it).
        if self.redraw > 0 && self.watching.is_none() && !self.minimized() {
            self.ui_wait()
        } else {
            IDLE_WAIT_MS
        }
    }

    fn draw(&mut self, new_video: bool) -> Result<(), String> {
        let info = self.watching.as_mut().map(|watching| {
            if new_video {
                watching.shown.push_back(Instant::now());
            }
            while watching
                .shown
                .front()
                .is_some_and(|time| time.elapsed() > Duration::from_secs(1))
            {
                watching.shown.pop_front();
            }
            let counters = watching.feed.counters();
            ui::WatchInfo {
                member: watching.member,
                fps: watching.shown.len() as f64,
                decoded: counters.decoded,
                dropped: counters.dropped,
            }
        });
        self.gfx.wait_ready();
        self.gfx.begin([0.07, 0.07, 0.09, 1.0])?;
        // SAFETY: both backends were initialized for this context and window
        // on this thread; calls happen in the documented order.
        unsafe {
            dx11::dear_imgui_backend_dx11_new_frame();
            win32::dear_imgui_backend_win32_new_frame();
        }
        let ui = self.context.frame();
        let action = ui::build(ui, &mut self.state, info.as_ref());
        // SAFETY: the context is current on this thread, inside the frame.
        self.events
            .set_cursor(unsafe { dear_imgui_sys::igGetMouseCursor() });
        let pending = self.context.render(self.textures.consumer());
        let frame = self.textures.reconcile(pending)?;
        let draw_data = std::ptr::from_ref(frame.draw_data())
            .cast::<dear_imgui_sys::ImDrawData>()
            .cast_mut();
        // SAFETY: `DrawData` is a transparent wrapper of `ImDrawData`; the
        // reconciled frame holds the context exclusively, so nothing else
        // aliases it, and every texture it uses was uploaded above (the
        // backend only binds them).
        unsafe { dx11::dear_imgui_backend_dx11_render_draw_data(draw_data) };
        drop(frame);
        self.gfx.present()?;
        self.frames += 1;
        if new_video {
            self.video_frames += 1;
        }
        if self.first_frame_ms.is_none() {
            self.first_frame_ms = Some(feed::ms_since_process_start());
        }
        match action {
            ui::Action::None => {}
            ui::Action::Watch(member) => {
                self.watch(member)?;
                self.redraw = FRAMES_AFTER_INPUT;
            }
            ui::Action::Leave => {
                self.leave()?;
                self.redraw = FRAMES_AFTER_INPUT;
            }
            ui::Action::ToggleFullscreen => {
                // Window changes belong to the window thread.
                // SAFETY: posting to a window of another thread is allowed.
                let _ = unsafe {
                    PostMessageW(
                        Some(self.window),
                        window::WM_TOGGLE_FULLSCREEN,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
                self.redraw = FRAMES_AFTER_INPUT;
            }
        }
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        while !self.events.closing.load(Ordering::Relaxed) {
            let mut handles = vec![self.events.wake_handle()];
            if let Some(watching) = &self.watching {
                handles.push(watching.feed.ready_event());
            }
            // SAFETY: valid handles (the feed's lives while watching);
            // returns on a signal or the timeout.
            let _ = unsafe { WaitForMultipleObjects(&handles, false, self.sleep_ms()) };
            self.step()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Summary, String> {
        let feed = self.watching.as_ref().map(|w| w.feed.counters());
        self.leave()?;
        self.textures.shutdown(&mut self.context)?;
        // SAFETY: matching shutdowns, renderer first, before the context drops.
        unsafe {
            dx11::dear_imgui_backend_dx11_shutdown();
            win32::dear_imgui_backend_win32_shutdown();
        }
        Ok(Summary {
            first_frame_ms: self.first_frame_ms,
            frames: self.frames,
            video_frames: self.video_frames,
            feed,
        })
    }
}

/// Sets up the swap chain, Dear ImGui and the feed on this (render) thread,
/// reports readiness, then draws until the window closes.
fn render_thread(
    device: D3d11Device,
    adapter: AdapterInfo,
    window: isize,
    watch: bool,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> Result<Summary, String> {
    let window = HWND(window as *mut _);
    // COM and Media Foundation for the decoder; declared first, so dropped
    // last (after the app and its COM objects).
    let _media = match MediaThread::start() {
        Ok(media) => media,
        Err(message) => {
            let _ = ready.send(Err(message.clone()));
            return Err(message);
        }
    };
    let setup = (|| -> Result<App, String> {
        // Share input state with the window thread, so the Win32 backend's
        // key-state queries see the keys pressed in the window.
        // SAFETY: plain thread queries; both threads belong to this process.
        unsafe {
            let window_thread = GetWindowThreadProcessId(window, None);
            let _ = AttachThreadInput(GetCurrentThreadId(), window_thread, true);
        }
        // SAFETY: valid window; plain query.
        let dpi = unsafe { win32::dear_imgui_backend_win32_get_dpi_scale_for_hwnd(window.0) };
        let gfx = gfx::Gfx::new(&device, window, window::client_size(window))?;
        let mut context = Context::create();
        context
            .set_ini_filename(None::<String>)
            .map_err(|e| e.to_string())?;
        {
            let style = context.style_mut();
            style.scale_all_sizes(dpi);
            style.set_font_scale_dpi(dpi);
        }
        let immediate = device.immediate_context().map_err(|e| e.to_string())?;
        // SAFETY: valid window, device and immediate context of that device;
        // the context created above is current on this thread. Shut down in
        // reverse in `App::finish`.
        unsafe {
            if !win32::dear_imgui_backend_win32_init(window.0) {
                return Err("ImGui Win32 backend".to_owned());
            }
            if !dx11::dear_imgui_backend_dx11_init(device.raw().as_raw(), immediate.as_raw()) {
                return Err("ImGui DX11 backend".to_owned());
            }
        }
        let textures = textures::TextureManager::new(&device, &mut context)?;
        let mut app = App {
            device,
            adapter,
            window,
            events: window::events(),
            gfx,
            context,
            textures,
            state: ui::State::default(),
            watching: None,
            redraw: FRAMES_AFTER_INPUT,
            last_draw: None,
            frames: 0,
            video_frames: 0,
            first_frame_ms: None,
        };
        if watch {
            app.watch(0)?;
        }
        Ok(app)
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
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    // SAFETY: process-wide setting, before any window exists.
    unsafe { win32::dear_imgui_backend_win32_enable_dpi_awareness() };
    // For the resize test's capture probe (WGC) on this thread.
    let _com = ComApartment::multithreaded()?;
    let adapters = gpu::enumerate_adapters().map_err(|e| e.to_string())?;
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .ok_or("no hardware adapter")?;
    let debug = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };
    let device = gpu::create_device(adapter, debug).map_err(|e| e.to_string())?;
    // The render thread, the feed's encode thread and the probe share it.
    device
        .enable_multithread_protection()
        .map_err(|e| e.to_string())?;

    let window = window::create("ziz - Dear ImGui prototype", 1280, 800)?;
    let events = window::events();
    let (ready_sender, ready) = mpsc::sync_channel(1);
    let render: JoinHandle<Result<Summary, String>> = {
        let (device, adapter, raw, watch) = (
            device.clone(),
            adapter.info.clone(),
            window.0 as isize,
            args.watch,
        );
        std::thread::Builder::new()
            .name("render".to_owned())
            .spawn(move || render_thread(device, adapter, raw, watch, ready_sender))
            .map_err(|e| e.to_string())?
    };
    if let Err(message) = ready
        .recv()
        .unwrap_or_else(|_| Err("render thread exited".to_owned()))
    {
        let _ = render.join();
        // SAFETY: window owned by this thread.
        let _ = unsafe { DestroyWindow(window) };
        return Err(message);
    }
    // On close: stop and join the render thread before the window goes.
    let render = std::rc::Rc::new(std::cell::RefCell::new(Some(render)));
    let summary = std::rc::Rc::new(std::cell::RefCell::new(None));
    {
        let (render, summary, events) = (render.clone(), summary.clone(), Arc::clone(&events));
        window::set_on_close(move || {
            events.closing.store(true, Ordering::Relaxed);
            events.wake();
            if let Some(handle) = render.borrow_mut().take() {
                *summary.borrow_mut() = Some(
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("render thread panicked".to_owned())),
                );
            }
        });
    }
    // SAFETY: valid window of this thread; plain show/z-order changes.
    unsafe {
        if let Some((x, y)) = args.at {
            let _ = SetWindowPos(
                window,
                None,
                x,
                y,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            );
        }
        if args.minimized {
            let _ = ShowWindow(window, SW_SHOWMINNOACTIVE);
        } else if args.background {
            // Shown without taking the focus (e.g. from a fullscreen game);
            // behind everything unless placed somewhere (--at, e.g. on
            // another monitor, where it should stay visible).
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
            if args.at.is_none() {
                let _ = SetWindowPos(
                    window,
                    Some(HWND_BOTTOM),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                );
            }
        } else {
            let _ = ShowWindow(window, SW_SHOW);
        }
    }

    let started = Instant::now();
    let mut probe = None;
    let mut next_resize = 0_usize;
    let mut close_sent = false;
    'run: loop {
        if args.resize_test {
            resize_test_tick(&device, window, started, &mut probe, &mut next_resize)?;
        }
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
        if args.resize_test {
            timeout = timeout.min(5);
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

    if let Some(probe) = &mut probe {
        let probe: &mut feed::resize_probe::ResizeProbe = probe;
        probe.check();
        println!("resize test: {} resizes, {:?}", next_resize, probe.counts);
    }
    drop(probe);
    let summary = summary
        .borrow_mut()
        .take()
        .unwrap_or_else(|| Err("render thread not joined".to_owned()))?;
    println!(
        "imgui: first frame {:.1} ms after process start; {} frames drawn ({} with new video) in {:.1} s; feed {:?}",
        summary.first_frame_ms.unwrap_or(f64::NAN),
        summary.frames,
        summary.video_frames,
        started.elapsed().as_secs_f64(),
        summary.feed
    );
    Ok(())
}

/// Window sizes cycled through by `--resize-test` (client areas, wide and
/// tall, so the video gets bars on either side).
const TEST_SIZES: [(i32, i32); 6] = [
    (1280, 720),
    (600, 900),
    (1600, 500),
    (900, 900),
    (700, 1000),
    (1400, 600),
];

/// `--resize-test`: after a second of playback, resizes the window every
/// 50 ms (through the normal WM_SIZE path) while a probe checks every
/// picture captured from it.
fn resize_test_tick(
    device: &D3d11Device,
    window: HWND,
    started: Instant,
    probe: &mut Option<feed::resize_probe::ResizeProbe>,
    next_resize: &mut usize,
) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos,
    };
    let elapsed = started.elapsed();
    if elapsed < Duration::from_secs(1) {
        return Ok(());
    }
    if probe.is_none() {
        *probe = Some(feed::resize_probe::ResizeProbe::start(
            device,
            window,
            (1920, 1080),
        )?);
    }
    if let Some(probe) = probe {
        probe.check();
    }
    let due = ((elapsed - Duration::from_secs(1)).as_millis() / 50) as usize;
    if due >= *next_resize {
        let (w, h) = TEST_SIZES[*next_resize % TEST_SIZES.len()];
        // Outer size for the client area (same style as at creation).
        let mut rect = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        // SAFETY: valid window of this thread; live out-pointer.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::AdjustWindowRectEx(
                &mut rect,
                windows::Win32::UI::WindowsAndMessaging::WS_OVERLAPPEDWINDOW,
                false,
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            );
            let _ = SetWindowPos(
                window,
                None,
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        *next_resize += 1;
    }
    Ok(())
}
