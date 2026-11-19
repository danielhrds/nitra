//! Dear ImGui prototype: member list, go live / watch,
//! settings, and a 1080p60 hardware-decoded video drawn in the same window
//! (`FrameRenderer`), the UI on top.
//!
//! The loop sleeps until something happens: input, a window event, or (while
//! watching) a new access unit. With nothing changing, nothing is drawn.
//!
//! Flags: `--watch` (start watching), `--seconds N` (quit after N s),
//! `--minimized`. Prints start-up time and counters on exit.

mod gfx;
mod textures;
mod ui;
mod window;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use dear_imgui_rs::Context;
use dear_imgui_sys::backend_shim::{dx11, win32};
use feed::{Feed, FeedConfig, MediaThread};
use gpu::{AdapterInfo, D3d11Device, DebugLayer};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, QS_ALLINPUT, SW_MINIMIZE, ShowWindow, TranslateMessage, WM_QUIT,
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
    /// Resize the window in a loop and count wrongly scaled pictures.
    resize_test: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        watch: false,
        seconds: None,
        minimized: false,
        resize_test: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--watch" => args.watch = true,
            "--minimized" => args.minimized = true,
            "--resize-test" => {
                args.resize_test = true;
                args.watch = true;
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

struct App {
    device: D3d11Device,
    adapter: AdapterInfo,
    window: HWND,
    gfx: gfx::Gfx,
    context: Context,
    textures: textures::TextureManager,
    state: ui::State,
    watching: Option<Watching>,
    fullscreen: Option<window::Fullscreen>,
    redraw: u32,
    last_draw: Option<Instant>,
    frames: u64,
    video_frames: u64,
    first_frame_ms: Option<f64>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

impl App {
    fn ready_event(&self) -> Option<HANDLE> {
        self.watching.as_ref().map(|w| w.feed.ready_event())
    }

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

    /// Takes new video and window events; draws a frame if one is due:
    /// at once after a resize; while watching, only with a new video
    /// picture (UI changes ride along, at most a frame late); otherwise
    /// for pending UI changes, at most every `UI_INTERVAL`.
    fn step(&mut self) -> Result<(), String> {
        let resized = window::take_resized();
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
        if window::is_minimized() {
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

    /// How long the main loop may sleep (it also wakes on input and video).
    fn sleep_ms(&self) -> u32 {
        if self.redraw > 0 && self.watching.is_none() {
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
                window::toggle_fullscreen(self.window, &mut self.fullscreen);
                self.redraw = FRAMES_AFTER_INPUT;
            }
        }
        Ok(())
    }
}

/// One step from inside Windows' modal move/resize loop.
fn modal_step() {
    APP.with(|app| {
        if let Ok(mut app) = app.try_borrow_mut()
            && let Some(app) = app.as_mut()
        {
            app.redraw = app.redraw.max(1);
            if let Err(error) = app.step() {
                eprintln!("step: {error}");
            }
        }
    });
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    // SAFETY: process-wide setting, before any window exists.
    unsafe { win32::dear_imgui_backend_win32_enable_dpi_awareness() };
    let _media = MediaThread::start()?;
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
    // The feed's encode thread shares the device.
    device
        .enable_multithread_protection()
        .map_err(|e| e.to_string())?;

    let window = window::create("ziz - Dear ImGui prototype", 1280, 800)?;
    if args.minimized {
        // SAFETY: valid window of this thread.
        let _ = unsafe { ShowWindow(window, SW_MINIMIZE) };
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
    // the context created above is current. Shut down in reverse below.
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
        device: device.clone(),
        adapter: adapter.info.clone(),
        window,
        gfx,
        context,
        textures,
        state: ui::State::default(),
        watching: None,
        fullscreen: None,
        redraw: FRAMES_AFTER_INPUT,
        last_draw: None,
        frames: 0,
        video_frames: 0,
        first_frame_ms: None,
    };
    if args.watch {
        app.watch(0)?;
    }
    APP.with(|cell| *cell.borrow_mut() = Some(app));
    window::set_modal_step(modal_step);

    let started = Instant::now();
    let mut result = Ok(());
    let mut probe = None;
    let mut next_resize = 0_usize;
    'run: loop {
        if args.resize_test {
            resize_test_tick(&device, window, started, &mut probe, &mut next_resize)?;
        }
        let (event, sleep_ms) = APP.with(|cell| {
            let app = cell.borrow();
            let app = app.as_ref().expect("app");
            (app.ready_event(), app.sleep_ms())
        });
        let handles: Vec<HANDLE> = event.into_iter().collect();
        let timeout = match args.seconds {
            // Wake in time to quit.
            Some(seconds) => {
                let left = (seconds - started.elapsed().as_secs_f64()).max(0.0);
                sleep_ms.min((left * 1000.0).ceil() as u32)
            }
            None => sleep_ms,
        };
        let timeout = if args.resize_test {
            timeout.min(5)
        } else {
            timeout
        };
        // SAFETY: valid handles (owned by the feed, alive while watching);
        // returns on a signal, input or the timeout.
        let _ = unsafe {
            MsgWaitForMultipleObjectsEx(Some(&handles), timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
        };
        let mut message = MSG::default();
        let mut had_input = false;
        // SAFETY: standard message pump for this thread's window.
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    break 'run;
                }
                had_input = true;
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let step = APP.with(|cell| {
            let mut app = cell.borrow_mut();
            let app = app.as_mut().expect("app");
            if had_input {
                app.redraw = FRAMES_AFTER_INPUT;
            }
            app.step()
        });
        if let Err(error) = step {
            result = Err(error);
            break;
        }
        if args
            .seconds
            .is_some_and(|seconds| started.elapsed().as_secs_f64() >= seconds)
        {
            break;
        }
    }

    if let Some(probe) = &mut probe {
        let probe: &mut feed::resize_probe::ResizeProbe = probe;
        probe.check();
        println!("resize test: {} resizes, {:?}", next_resize, probe.counts);
    }
    drop(probe);
    let mut app = APP.with(|cell| cell.borrow_mut().take()).expect("app");
    let counters = app.watching.as_ref().map(|w| w.feed.counters());
    app.leave()?;
    app.textures.shutdown(&mut app.context)?;
    // SAFETY: matching shutdowns, renderer first, before the context drops.
    unsafe {
        dx11::dear_imgui_backend_dx11_shutdown();
        win32::dear_imgui_backend_win32_shutdown();
    }
    println!(
        "imgui: first frame {:.1} ms after process start; {} frames drawn ({} with new video) in {:.1} s; feed {:?}",
        app.first_frame_ms.unwrap_or(f64::NAN),
        app.frames,
        app.video_frames,
        started.elapsed().as_secs_f64(),
        counters
    );
    result
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
