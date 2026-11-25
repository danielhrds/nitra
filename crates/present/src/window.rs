//! The presenter's two threads.
//!
//! - The window thread owns the window and only handles messages (resize,
//!   F11/Alt+Enter, O, close). While the user drags or resizes the window,
//!   Windows runs a modal loop on this thread; nothing else waits on it.
//! - The render thread owns the flip-model swap chain, the frame renderer
//!   and the overlay. It wakes on a new frame, on window events or when the
//!   swap chain can take a frame, so video keeps playing (and the swap
//!   chain is resized live) while the window is being dragged.
//!
//! A windowed flip-model swap chain never sends messages to its window, so
//! presenting from a thread other than the window's cannot deadlock.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpu::D3d11Device;
use telemetry::{Stage, StageRecorder, qpc_frequency, qpc_now};
use windows::Win32::Foundation::{
    CloseHandle, DXGI_STATUS_OCCLUDED, HANDLE, HWND, LPARAM, LRESULT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_FRAME_PRESENTATION_MODE_COMPOSED,
    DXGI_FRAME_PRESENTATION_MODE_COMPOSITION_FAILURE, DXGI_FRAME_PRESENTATION_MODE_NONE,
    DXGI_FRAME_PRESENTATION_MODE_OVERLAY, DXGI_FRAME_STATISTICS, DXGI_FRAME_STATISTICS_MEDIA,
    DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT, DXGI_PRESENT_TEST, DXGI_SCALING_NONE,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISwapChain2,
    IDXGISwapChainMedia,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetEvent, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F11, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GA_ROOT, GWL_STYLE, GetAncestor, GetClientRect, GetMessageW,
    GetWindowLongPtrW, GetWindowRect, HWND_TOP, IDC_ARROW, IsIconic, LoadCursorW, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED,
    SWP_NOOWNERZORDER, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_PAINT, WM_SIZE, WM_SYSKEYDOWN, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_NOPARENTNOTIFY,
    WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};
use windows::core::{HSTRING, Interface, PCWSTR, w};

use crate::display::{DisplayTracker, ShownSpacing, Statistics};
use crate::overlay::Overlay;
use crate::renderer::FrameRenderer;
use crate::{CompositionModes, PresenterOptions, PresenterReport, QueuedFrame, Shared};

pub(crate) const WM_TOGGLE_FULLSCREEN: u32 = WM_APP + 1;
const CLASS_NAME: PCWSTR = w!("nitra_video_presenter");
/// Upper bound on one wait, so a lost wake-up can never hang the thread.
const MAX_WAIT_MS: u32 = 100;
/// Overlay statistics window.
const LATENCY_SAMPLES: usize = 120;

/// Window events for the render thread (set by the window procedure).
#[derive(Default)]
struct WindowFlags {
    resized: AtomicBool,
    toggle_overlay: AtomicBool,
    repaint: AtomicBool,
    closed: AtomicBool,
}

/// The window thread's state, reachable from its window procedure.
struct WindowContext {
    flags: Arc<WindowFlags>,
    shared: Arc<Shared>,
    render: Option<JoinHandle<PresenterReport>>,
    report: Option<PresenterReport>,
    restore: Option<Restore>,
}

thread_local! {
    // One presenter window per window thread.
    static CONTEXT: RefCell<Option<WindowContext>> = const { RefCell::new(None) };
}

/// The window style and rectangle to restore when leaving fullscreen.
struct Restore {
    style: isize,
    rect: RECT,
}

/// The window thread: creates the window, starts the render thread, runs
/// the message loop until the window is destroyed and returns the report.
pub(crate) fn run(
    device: D3d11Device,
    options: PresenterOptions,
    shared: Arc<Shared>,
    ready: mpsc::SyncSender<Result<isize, String>>,
) -> PresenterReport {
    let fail = |message: String, shared: &Shared| {
        shared.closed.store(true, Ordering::Relaxed);
        let _ = ready.send(Err(message.clone()));
        PresenterReport::failed(shared.counters.snapshot(), message)
    };
    let window = match create_window(&options) {
        Ok(window) => window,
        Err(message) => return fail(message, &shared),
    };
    let flags = Arc::new(WindowFlags::default());
    let (render_ready, render_started) = mpsc::sync_channel(1);
    let render = {
        let (flags, shared) = (Arc::clone(&flags), Arc::clone(&shared));
        let (overlay, skip_when_hidden) = (options.overlay, options.skip_when_hidden);
        let window_raw = window.0 as isize;
        std::thread::Builder::new()
            .name("presenter-render".to_owned())
            .spawn(move || {
                render_thread(
                    device,
                    window_raw,
                    overlay,
                    skip_when_hidden,
                    shared,
                    flags,
                    render_ready,
                )
            })
    };
    let render = match render {
        Ok(render) => render,
        Err(error) => {
            // SAFETY: window owned by this thread.
            let _ = unsafe { DestroyWindow(window) };
            return fail(format!("render thread: {error}"), &shared);
        }
    };
    if let Err(message) = render_started
        .recv()
        .unwrap_or_else(|_| Err("render thread exited".to_owned()))
    {
        let _ = render.join();
        // SAFETY: window owned by this thread.
        let _ = unsafe { DestroyWindow(window) };
        return fail(message, &shared);
    }
    CONTEXT.with(|context| {
        *context.borrow_mut() = Some(WindowContext {
            flags,
            shared: Arc::clone(&shared),
            render: Some(render),
            report: None,
            restore: None,
        });
    });
    let _ = ready.send(Ok(window.0 as isize));

    let mut message = MSG::default();
    // SAFETY: standard message loop for this thread's window; ends with
    // WM_QUIT after the window is destroyed (see WM_CLOSE / WM_DESTROY).
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    CONTEXT.with(|context| {
        let mut context = context.borrow_mut().take().expect("window context");
        stop_render(&mut context);
        context.report.take().expect("render report")
    })
}

/// Stops the render thread (if still running) and keeps its report.
fn stop_render(context: &mut WindowContext) {
    if let Some(render) = context.render.take() {
        context.flags.closed.store(true, Ordering::Relaxed);
        // SAFETY: valid event owned by `shared`.
        let _ = unsafe { SetEvent(context.shared.wake_handle()) };
        context.report = Some(render.join().unwrap_or_else(|_| {
            PresenterReport::failed(
                context.shared.counters.snapshot(),
                "render thread panicked".to_owned(),
            )
        }));
    }
}

/// A waitable timer that wakes the render thread when the next waiting
/// frame is due. High resolution where Windows has it (about 0.5 ms; an
/// ordinary timer or wait timeout rounds to the ~15.6 ms system tick).
struct DueTimer(HANDLE);

impl DueTimer {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: plain creation calls; the handle is closed in `drop`.
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
        }?;
        Ok(Self(handle))
    }

    fn handle(&self) -> HANDLE {
        self.0
    }

    /// Signals at `due` (QPC ticks), or at once if it has passed.
    fn set_at(&self, due: i64) {
        let ticks = (due - qpc_now().ticks()).max(0) as f64;
        let hundred_ns = ticks * 1e7 / qpc_frequency().ticks_per_second() as f64;
        // Negative: relative to now, in 100 ns units (at least one).
        let relative = -(hundred_ns.max(1.0) as i64);
        // SAFETY: valid timer handle; no completion routine.
        let _ = unsafe { SetWaitableTimer(self.0, &relative, 0, None, None, false) };
    }
}

impl Drop for DueTimer {
    fn drop(&mut self) {
        // SAFETY: created in `new`, closed once here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The swap chain and its frame-latency waitable object.
struct SwapChain {
    chain: IDXGISwapChain2,
    /// For the composition mode; `None` if the swap chain lacks it.
    media: Option<IDXGISwapChainMedia>,
    waitable: HANDLE,
    flags: DXGI_SWAP_CHAIN_FLAG,
}

impl Drop for SwapChain {
    fn drop(&mut self) {
        // SAFETY: the handle came from GetFrameLatencyWaitableObject and is
        // closed once, after the last wait.
        let _ = unsafe { CloseHandle(self.waitable) };
    }
}

fn render_thread(
    device: D3d11Device,
    window: isize,
    overlay_enabled: bool,
    skip_when_hidden: bool,
    shared: Arc<Shared>,
    flags: Arc<WindowFlags>,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> PresenterReport {
    let window = HWND(window as *mut _);
    let started = (|| -> Result<(SwapChain, FrameRenderer, Overlay, DueTimer), String> {
        let swap_chain =
            create_swap_chain(&device, window).map_err(|e| format!("swap chain: {e}"))?;
        let renderer = FrameRenderer::new(&device).map_err(|e| format!("video processor: {e}"))?;
        let overlay = Overlay::new(&device).map_err(|e| format!("overlay: {e}"))?;
        let due_timer = DueTimer::new().map_err(|e| format!("timer: {e}"))?;
        Ok((swap_chain, renderer, overlay, due_timer))
    })();
    let (swap_chain, renderer, overlay, due_timer) = match started {
        Ok(parts) => parts,
        Err(message) => {
            let _ = ready.send(Err(message.clone()));
            return PresenterReport::failed(shared.counters.snapshot(), message);
        }
    };
    let _ = ready.send(Ok(()));
    let mut renderer = Renderer {
        window,
        swap_chain,
        renderer,
        overlay: Some(overlay),
        overlay_enabled,
        skip_when_hidden,
        client: client_size(window),
        shared,
        flags,
        stages: StageRecorder::new(),
        display: DisplayTracker::default(),
        spacing: ShownSpacing::default(),
        due_timer,
        last: None,
        presented_at: VecDeque::new(),
        latencies_ms: VecDeque::new(),
        composition: CompositionModes::default(),
    };
    let error = renderer.run().err();
    renderer.shared.closed.store(true, Ordering::Relaxed);
    if let Some(error) = &error {
        *renderer.shared.error.lock().expect("presenter lock") = Some(error.clone());
        // Let the window thread close the window (it then joins us).
        // SAFETY: posting to a window of another thread is allowed.
        let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
    renderer.last = None;
    renderer.overlay = None;
    renderer.renderer.release_views();
    // The last frames may still be on their way to the screen.
    if error.is_none() {
        renderer.settle_display();
    }
    PresenterReport {
        counters: renderer.shared.counters.snapshot(),
        stages: std::mem::replace(&mut renderer.stages, StageRecorder::new()),
        display: std::mem::take(&mut renderer.display).finish(),
        composition: renderer.composition,
        error,
    }
}

struct Renderer {
    window: HWND,
    swap_chain: SwapChain,
    renderer: FrameRenderer,
    overlay: Option<Overlay>,
    overlay_enabled: bool,
    /// Stop drawing while the window cannot be seen (minimized, covered).
    skip_when_hidden: bool,
    client: (u32, u32),
    shared: Arc<Shared>,
    flags: Arc<WindowFlags>,
    stages: StageRecorder,
    display: DisplayTracker,
    /// How long new frames stay on screen (from the waitable object).
    spacing: ShownSpacing,
    /// Wakes the thread when the next waiting frame is due.
    due_timer: DueTimer,
    /// The frame on screen, kept to redraw it (resize, repaint).
    last: Option<QueuedFrame>,
    presented_at: VecDeque<Instant>,
    latencies_ms: VecDeque<f64>,
    composition: CompositionModes,
}

impl Renderer {
    fn run(&mut self) -> Result<(), String> {
        let mut swap_chain_ready = false;
        let mut redraw = false;
        let mut pending: Option<QueuedFrame> = None;
        loop {
            // A new frame, a frame's time, and the swap chain while it is busy.
            let handles = [
                self.shared.wake_handle(),
                self.due_timer.handle(),
                self.swap_chain.waitable,
            ];
            let count = if swap_chain_ready { 2 } else { 3 };
            // SAFETY: valid handles; returns on a signal or the timeout.
            let result = unsafe { WaitForMultipleObjects(&handles[..count], false, MAX_WAIT_MS) };
            if count == 3 && result.0 == WAIT_OBJECT_0.0 + 2 {
                swap_chain_ready = true;
                self.spacing.ready(qpc_now(), true);
            }
            self.read_display_statistics();
            let flags = Arc::clone(&self.flags);
            if flags.closed.load(Ordering::Relaxed) {
                return Ok(());
            }
            if flags.toggle_overlay.swap(false, Ordering::Relaxed) {
                self.overlay_enabled = !self.overlay_enabled;
                redraw = true;
            }
            if flags.resized.swap(false, Ordering::Relaxed) {
                redraw |= self.resize()?;
            }
            redraw |= flags.repaint.swap(false, Ordering::Relaxed);
            // The frames due now (and those without a time): the newest is
            // shown, the ones before it are skipped. Then wake for the next.
            let skipped = {
                let now = qpc_now().ticks();
                let mut next = self.shared.next.lock().expect("presenter lock");
                let due = next
                    .iter()
                    .take_while(|frame| frame.due.is_none_or(|due| due <= now))
                    .count();
                let mut skipped = due.saturating_sub(1);
                if let Some(frame) = next.drain(..due).next_back()
                    && pending.replace(frame).is_some()
                {
                    // A newer frame came before the waiting one was shown.
                    skipped += 1;
                }
                if let Some(due) = next.front().and_then(|frame| frame.due) {
                    self.due_timer.set_at(due);
                }
                skipped
            };
            if skipped > 0 {
                self.shared
                    .counters
                    .replaced
                    .fetch_add(skipped as u64, Ordering::Relaxed);
            }
            let hidden = self.shared.hidden.load(Ordering::Relaxed);
            if hidden || (self.skip_when_hidden && pending.is_some() && self.window_hidden()) {
                // Nothing is drawn (and the owner queues nothing) until the
                // window can be seen again; checked on every wake-up, at
                // least every MAX_WAIT_MS.
                if self.still_hidden()? {
                    self.shared.hidden.store(true, Ordering::Relaxed);
                    continue;
                }
                if hidden {
                    self.shared.hidden.store(false, Ordering::Relaxed);
                    redraw = true;
                }
            }
            let work = pending.is_some() || (redraw && self.last.is_some());
            if work && !swap_chain_ready {
                // SAFETY: valid waitable handle; zero timeout.
                swap_chain_ready =
                    unsafe { WaitForSingleObject(self.swap_chain.waitable, 0) } == WAIT_OBJECT_0;
                if swap_chain_ready {
                    // Signaled some time before: its moment is unknown.
                    self.spacing.ready(qpc_now(), false);
                }
            }
            if work && swap_chain_ready {
                let fresh = pending.is_some();
                if let Some(frame) = pending.take() {
                    self.last = Some(frame);
                }
                self.draw_and_present(fresh)?;
                swap_chain_ready = false;
                redraw = false;
            }
        }
    }

    fn draw_and_present(&mut self, fresh: bool) -> Result<(), String> {
        if fresh {
            self.presented_at.push_back(Instant::now());
            while self
                .presented_at
                .front()
                .is_some_and(|time| time.elapsed() > Duration::from_secs(1))
            {
                self.presented_at.pop_front();
            }
        }
        let custom = self
            .shared
            .overlay_lines
            .lock()
            .expect("overlay lock")
            .clone();
        let show_overlay = custom.is_some() || self.overlay_enabled;
        let lines = match custom {
            Some(lines) => lines,
            None if self.overlay_enabled => self.overlay_lines(),
            None => Vec::new(),
        };
        let frame = self.last.as_ref().expect("a frame to draw");
        // SAFETY: valid swap chain; buffer 0 is the current back buffer.
        let back: ID3D11Texture2D =
            unsafe { self.swap_chain.chain.GetBuffer(0) }.map_err(|e| e.to_string())?;
        self.renderer
            .render(
                &frame.texture,
                (frame.width, frame.height),
                &back,
                self.client,
            )
            .map_err(|e| format!("render: {e}"))?;
        if let (true, Some(overlay)) = (show_overlay, self.overlay.as_mut()) {
            overlay
                .draw(&back, &lines, self.client)
                .map_err(|e| format!("overlay: {e}"))?;
        }
        drop(back);
        // SAFETY: valid swap chain; sync interval 1 shows the frame on the
        // next refresh.
        let result = unsafe { self.swap_chain.chain.Present(1, DXGI_PRESENT(0)) };
        let presented = qpc_now();
        if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
            return Err(format!("GPU device lost ({result:?})"));
        }
        result.ok().map_err(|e| format!("Present: {e}"))?;
        self.spacing.presented(fresh);
        if result == DXGI_STATUS_OCCLUDED && self.skip_when_hidden {
            // Nobody can see the window: stop drawing (see `still_hidden`).
            self.shared.hidden.store(true, Ordering::Relaxed);
        }
        // SAFETY: valid swap chain; plain query.
        match unsafe { self.swap_chain.chain.GetLastPresentCount() } {
            Ok(present_id) => self.display.presented(
                present_id,
                fresh.then(|| self.last.as_ref().expect("drawn above").meta),
                presented,
            ),
            Err(_) => self.display.statistics_failed(),
        }
        self.read_display_statistics();
        if fresh {
            self.sample_composition();
        }
        let counters = &self.shared.counters;
        if fresh {
            counters.presented.fetch_add(1, Ordering::Relaxed);
            let frame = self.last.as_ref().expect("drawn above");
            if frame.meta.decoded_at.ticks() > 0 {
                self.stages
                    .record(Stage::Present, frame.meta.decoded_at, presented);
                let latency = presented
                    .duration_since(frame.meta.decoded_at)
                    .as_micros(qpc_frequency())
                    / 1000.0;
                self.latencies_ms.push_back(latency);
                if self.latencies_ms.len() > LATENCY_SAMPLES {
                    self.latencies_ms.pop_front();
                }
            }
        } else {
            counters.redraws.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Counts how the compositor showed the swap chain lately (composed,
    /// overlay plane, ...).
    fn sample_composition(&mut self) {
        let modes = &mut self.composition;
        let Some(media) = &self.swap_chain.media else {
            modes.unknown += 1;
            return;
        };
        let mut statistics = DXGI_FRAME_STATISTICS_MEDIA::default();
        // SAFETY: valid interface; live out-pointer. Fails without side
        // effects while no statistics are available.
        if unsafe { media.GetFrameStatisticsMedia(&mut statistics) }.is_err() {
            modes.unknown += 1;
            return;
        }
        match statistics.CompositionMode {
            DXGI_FRAME_PRESENTATION_MODE_COMPOSED => modes.composed += 1,
            DXGI_FRAME_PRESENTATION_MODE_OVERLAY => modes.overlay += 1,
            DXGI_FRAME_PRESENTATION_MODE_NONE => modes.none += 1,
            DXGI_FRAME_PRESENTATION_MODE_COMPOSITION_FAILURE => modes.failure += 1,
            _ => modes.unknown += 1,
        }
    }

    /// Whether the window is minimized (of its own, or with its top-level
    /// parent) or cloaked (e.g. on another virtual desktop). A flip-model
    /// swap chain does not report these as occlusion.
    fn window_hidden(&self) -> bool {
        // SAFETY: plain queries on a valid window (owned by the presenter's
        // window thread, readable from any thread); the out-pointer is a
        // live local of the size passed.
        unsafe {
            let root = GetAncestor(self.window, GA_ROOT);
            let root = if root.is_invalid() { self.window } else { root };
            if IsIconic(root).as_bool() {
                return true;
            }
            let mut cloaked = 0_u32;
            DwmGetWindowAttribute(
                root,
                DWMWA_CLOAKED,
                (&raw mut cloaked).cast(),
                size_of::<u32>() as u32,
            )
            .is_ok()
                && cloaked != 0
        }
    }

    /// Whether the window still cannot be seen: minimized or cloaked, or
    /// occluded as the swap chain reports it (asked without presenting).
    fn still_hidden(&self) -> Result<bool, String> {
        if self.window_hidden() {
            return Ok(true);
        }
        // SAFETY: valid swap chain; DXGI_PRESENT_TEST only reports the
        // status and queues nothing.
        let result = unsafe { self.swap_chain.chain.Present(0, DXGI_PRESENT_TEST) };
        if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
            return Err(format!("GPU device lost ({result:?})"));
        }
        Ok(result == DXGI_STATUS_OCCLUDED)
    }

    /// Reads the swap chain's frame statistics while presents are waiting
    /// to be seen on screen.
    fn read_display_statistics(&mut self) {
        if !self.display.has_pending() {
            return;
        }
        let mut statistics = DXGI_FRAME_STATISTICS::default();
        // SAFETY: valid swap chain; live out-pointer. Fails (e.g. with
        // DXGI_ERROR_FRAME_STATISTICS_DISJOINT) without side effects.
        match unsafe { self.swap_chain.chain.GetFrameStatistics(&mut statistics) } {
            Ok(()) => self.display.observe(Statistics {
                present_count: statistics.PresentCount,
                present_refresh_count: statistics.PresentRefreshCount,
                sync_refresh_count: statistics.SyncRefreshCount,
                sync_qpc_time: statistics.SyncQPCTime,
            }),
            Err(_) => self.display.statistics_failed(),
        }
        let intervals = self.spacing.take();
        if !intervals.is_empty() {
            self.shared
                .cadence
                .lock()
                .expect("cadence lock")
                .extend(&intervals);
        }
    }

    /// Gives the frames presented last a few refreshes to reach the screen.
    fn settle_display(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(100);
        while self.display.has_pending() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
            self.read_display_statistics();
        }
    }

    fn overlay_lines(&self) -> Vec<String> {
        let counters = self.shared.counters.snapshot();
        let frame = self.last.as_ref().expect("a frame to draw");
        let mut sorted: Vec<f64> = self.latencies_ms.iter().copied().collect();
        sorted.sort_unstable_by(f64::total_cmp);
        let latency = if sorted.is_empty() {
            "decode->present: n/a".to_owned()
        } else {
            format!(
                "decode->present: p50 {:.2} ms, max {:.2} ms (last {})",
                sorted[sorted.len() / 2],
                sorted[sorted.len() - 1],
                sorted.len()
            )
        };
        vec![
            format!(
                "{}x{} in {}x{} | {:.0} fps shown | frame {}",
                frame.width,
                frame.height,
                self.client.0,
                self.client.1,
                self.presented_at.len(),
                frame.meta.frame_id
            ),
            latency,
            format!(
                "replaced {} | dropped (pool) {} | F11 fullscreen, O overlay",
                counters.replaced, counters.pool_dropped
            ),
        ]
    }

    /// Resizes the swap chain to the window's client area; true if it did.
    fn resize(&mut self) -> Result<bool, String> {
        let (width, height) = client_size(self.window);
        if width == 0 || height == 0 || (width, height) == self.client {
            return Ok(false);
        }
        // Every reference to the back buffers must go before ResizeBuffers.
        self.renderer.release_views();
        // SAFETY: valid swap chain with no outstanding buffer references.
        unsafe {
            self.swap_chain.chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                self.swap_chain.flags,
            )
        }
        .map_err(|e| format!("ResizeBuffers: {e}"))?;
        self.client = (width, height);
        Ok(true)
    }
}

/// Borderless fullscreen on the window's monitor, and back (window thread).
/// Not for child windows: their size belongs to the parent's layout.
fn toggle_fullscreen(window: HWND, restore: &mut Option<Restore>) {
    // SAFETY (whole block): window owned by this thread; plain style and
    // position changes; out-pointers are live locals.
    unsafe {
        if (GetWindowLongPtrW(window, GWL_STYLE) as u32 & WS_CHILD.0) != 0 {
            return;
        }
        if let Some(saved) = restore.take() {
            SetWindowLongPtrW(window, GWL_STYLE, saved.style);
            let rect = saved.rect;
            let _ = SetWindowPos(
                window,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOOWNERZORDER,
            );
            return;
        }
        let style = GetWindowLongPtrW(window, GWL_STYLE);
        let mut rect = RECT::default();
        let _ = GetWindowRect(window, &mut rect);
        let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return;
        }
        *restore = Some(Restore { style, rect });
        SetWindowLongPtrW(window, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        let area = info.rcMonitor;
        let _ = SetWindowPos(
            window,
            Some(HWND_TOP),
            area.left,
            area.top,
            area.right - area.left,
            area.bottom - area.top,
            SWP_FRAMECHANGED | SWP_NOOWNERZORDER,
        );
    }
}

fn client_size(window: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    // SAFETY: valid window (any thread may query it); live out-pointer.
    let _ = unsafe { GetClientRect(window, &mut rect) };
    (
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    )
}

fn create_window(options: &PresenterOptions) -> Result<HWND, String> {
    let parent = options.parent.map(|raw| HWND(raw as *mut _));
    // A top-level window in the background is created hidden and shown
    // without activation below, so it never takes the focus (e.g. from a
    // fullscreen game on another monitor).
    let visible = if options.background && parent.is_none() {
        WINDOW_STYLE(0)
    } else {
        WS_VISIBLE
    };
    let style: WINDOW_STYLE = if parent.is_some() {
        // Clipped against its siblings; the parent clips it too.
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS
    } else if options.borderless {
        WS_POPUP | visible
    } else {
        WS_OVERLAPPEDWINDOW | visible
    };
    let (x, y) = match (parent, options.position) {
        (Some(_), _) => (0, 0),
        (None, Some(position)) => position,
        (None, None) => (100, 100),
    };
    // A child of another thread's window must not notify it synchronously
    // (WM_PARENTNOTIFY) while that thread may be waiting for us.
    let ex_style = if parent.is_some() {
        WS_EX_NOPARENTNOTIFY
    } else {
        WINDOW_EX_STYLE(0)
    };
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: options.width as i32,
        bottom: options.height as i32,
    };
    let title = HSTRING::from(options.title.as_str());
    // SAFETY: plain Win32 window creation on this thread. The class is
    // registered once per process; re-registration fails harmlessly.
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        let _ = RegisterClassW(&class);
        // Outer size for the requested client area.
        let _ = AdjustWindowRectEx(&mut rect, style, false, WINDOW_EX_STYLE(0));
        let window = CreateWindowExW(
            ex_style,
            CLASS_NAME,
            &title,
            style,
            x,
            y,
            rect.right - rect.left,
            rect.bottom - rect.top,
            parent,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        if visible.0 == 0 {
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
        }
        Ok(window)
    }
}

fn create_swap_chain(device: &D3d11Device, window: HWND) -> windows::core::Result<SwapChain> {
    let flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: 0,
        Height: 0,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        // Never stretched by the compositor: a frame drawn for the old size
        // shows unscaled until the next one (drawn letterboxed) arrives.
        Scaling: DXGI_SCALING_NONE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: flags.0 as u32,
    };
    // SAFETY: valid device and window; the factory is the one that created
    // the device's adapter.
    unsafe {
        let dxgi: IDXGIDevice = device.raw().cast()?;
        let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
        let chain = factory.CreateSwapChainForHwnd(device.raw(), window, &desc, None, None)?;
        // Our own F11/Alt+Enter handling instead of DXGI's.
        factory.MakeWindowAssociation(window, DXGI_MWA_NO_ALT_ENTER)?;
        let chain: IDXGISwapChain2 = chain.cast()?;
        // At most one frame queued: shown on the next refresh, no backlog.
        chain.SetMaximumFrameLatency(1)?;
        let waitable = chain.GetFrameLatencyWaitableObject();
        let media = chain.cast::<IDXGISwapChainMedia>().ok();
        Ok(SwapChain {
            chain,
            media,
            waitable,
            flags,
        })
    }
}

/// Raises a window flag and wakes the render thread.
fn signal(pick: fn(&WindowFlags) -> &AtomicBool) {
    CONTEXT.with(|context| {
        if let Some(context) = context.borrow().as_ref() {
            pick(&context.flags).store(true, Ordering::Relaxed);
            // SAFETY: valid event owned by `shared`.
            let _ = unsafe { SetEvent(context.shared.wake_handle()) };
        }
    });
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let fullscreen = || {
        // SetWindowPos re-enters this procedure (WM_SIZE) synchronously, so
        // the context must not stay borrowed while the window changes.
        let mut restore = CONTEXT.with(|context| {
            context
                .borrow_mut()
                .as_mut()
                .and_then(|context| context.restore.take())
        });
        toggle_fullscreen(window, &mut restore);
        CONTEXT.with(|context| {
            if let Some(context) = context.borrow_mut().as_mut() {
                context.restore = restore;
            }
        });
    };
    // SAFETY (whole function): called by the system on the window's own
    // thread with a valid window handle.
    unsafe {
        match message {
            WM_SIZE => {
                signal(|flags| &flags.resized);
                LRESULT(0)
            }
            WM_PAINT => {
                let _ = ValidateRect(Some(window), None);
                // The compositor keeps a flip-model swap chain's picture; a
                // child window is invalidated whenever its parent repaints
                // (possibly every frame of the parent), so only top-level
                // windows redraw here.
                if (GetWindowLongPtrW(window, GWL_STYLE) as u32 & WS_CHILD.0) == 0 {
                    signal(|flags| &flags.repaint);
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_KEYDOWN if wparam.0 == usize::from(VK_F11.0) => {
                fullscreen();
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == usize::from(b'O') => {
                signal(|flags| &flags.toggle_overlay);
                LRESULT(0)
            }
            // Alt+Enter (bit 29 of lParam: the Alt key is down).
            WM_SYSKEYDOWN if wparam.0 == usize::from(VK_RETURN.0) && lparam.0 & (1 << 29) != 0 => {
                fullscreen();
                LRESULT(0)
            }
            WM_TOGGLE_FULLSCREEN => {
                fullscreen();
                LRESULT(0)
            }
            WM_CLOSE => {
                // The render thread lets go of the swap chain before the
                // window it presents to is destroyed.
                CONTEXT.with(|context| {
                    if let Some(context) = context.borrow_mut().as_mut() {
                        stop_render(context);
                    }
                });
                let _ = DestroyWindow(window);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}
