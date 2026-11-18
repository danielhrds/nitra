//! The capture thread: owns the WGC session, copies frames into the pool
//! and hands them to the consumer slot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use gpu::{D3d11Device, GpuFrame, TexturePool, create_texture_pool};
use telemetry::{QpcDuration, QpcTimestamp, qpc_frequency, qpc_now};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, ID3D11DeviceContext};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{SetEvent, WaitForMultipleObjects};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use super::interop::{
    item_for_monitor, item_for_window, surface_texture, timespan_to_qpc, winrt_device,
};
use super::limiter::RateLimiter;
use super::slot::LatestSlot;
use super::{CaptureEnd, CaptureOptions, CaptureReport, CaptureTarget, Counters, OwnedEvent};

/// WGC's own buffers; frames are copied out immediately, so two suffice.
const WGC_BUFFERS: i32 = 2;
const PIXEL_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
/// Wake up at least this often to notice a closed source.
const WAIT_TIMEOUT_MS: u32 = 250;

/// `CaptureTarget` as plain integers so it can move to the capture thread.
#[derive(Clone, Copy, Debug)]
pub(super) enum RawTarget {
    Monitor(isize),
    Window(isize),
}

impl From<CaptureTarget> for RawTarget {
    fn from(target: CaptureTarget) -> Self {
        match target {
            CaptureTarget::Monitor(monitor) => RawTarget::Monitor(monitor.0 as isize),
            CaptureTarget::Window(window) => RawTarget::Window(window.0 as isize),
        }
    }
}

pub(super) struct SessionContext {
    pub device: D3d11Device,
    pub target: RawTarget,
    pub options: CaptureOptions,
    pub slot: Arc<LatestSlot<GpuFrame, CaptureEnd>>,
    pub counters: Arc<Counters>,
    /// Owned by `WgcCapture`, which joins this thread before closing it.
    pub stop_event: isize,
}

/// Thread entry point. Reports start-up success or failure through `ready`.
pub(super) fn run(
    context: SessionContext,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> CaptureReport {
    // SAFETY: joins the MTA for this thread; balanced by `Apartment::drop`.
    let apartment = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map(|()| Apartment);
    if let Err(error) = apartment {
        let message = format!("COM initialization: {error}");
        let _ = ready.send(Err(message.clone()));
        return failed_report(&context, message);
    }

    let mut session = match Session::start(&context) {
        Ok(session) => session,
        Err(error) => {
            let message = format!("WGC start: {error}");
            let _ = ready.send(Err(message.clone()));
            context.slot.end(CaptureEnd::Failed(message.clone()));
            return failed_report(&context, message);
        }
    };
    let _ = ready.send(Ok(()));

    let end = session.run_loop(&context);
    session.close();
    context.slot.end(end.clone());
    CaptureReport {
        end,
        counters: context.counters.snapshot(),
        cost: session.cost,
        frame_time_offset: session.frame_time_offset,
        intervals: session.intervals,
        border_hidden: session.border_hidden,
        min_update_interval_set: session.min_update_interval_set,
    }
}

fn failed_report(context: &SessionContext, message: String) -> CaptureReport {
    CaptureReport {
        end: CaptureEnd::Failed(message),
        counters: context.counters.snapshot(),
        cost: Vec::new(),
        frame_time_offset: Vec::new(),
        intervals: Vec::new(),
        border_hidden: false,
        min_update_interval_set: false,
    }
}

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

struct Session {
    item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    capture: GraphicsCaptureSession,
    winrt_device: IDirect3DDevice,
    context: ID3D11DeviceContext,
    frame_event: OwnedEvent,
    closed: Arc<AtomicBool>,
    frame_arrived_token: i64,
    closed_token: i64,
    textures: TexturePool,
    size: SizeInt32,
    limiter: RateLimiter,
    next_frame_id: u64,
    last_delivery: Option<QpcTimestamp>,
    cost: Vec<QpcDuration>,
    frame_time_offset: Vec<i64>,
    intervals: Vec<QpcDuration>,
    border_hidden: bool,
    min_update_interval_set: bool,
}

impl Session {
    fn start(context: &SessionContext) -> windows::core::Result<Self> {
        let item = match context.target {
            RawTarget::Monitor(raw) => item_for_monitor(HMONITOR(raw as *mut _))?,
            RawTarget::Window(raw) => item_for_window(HWND(raw as *mut _))?,
        };
        let size = item.Size()?;
        let winrt_device = winrt_device(&context.device)?;
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            PIXEL_FORMAT,
            WGC_BUFFERS,
            size,
        )?;
        let capture = frame_pool.CreateCaptureSession(&item)?;

        let options = context.options;
        let border_hidden = options.hide_border && capture.SetIsBorderRequired(false).is_ok();
        // Optional settings: older Windows builds lack them, which is fine.
        let _ = capture.SetIsCursorCaptureEnabled(options.cursor);
        // Let WGC send at most twice the target rate: fewer wasted copies,
        // while the rate limiter keeps the exact cadence.
        let min_interval = TimeSpan {
            Duration: 10_000_000 / (2 * i64::from(options.fps)),
        };
        let min_update_interval_set = capture.SetMinUpdateInterval(min_interval).is_ok();

        let frame_event = OwnedEvent::new(false)?;
        let closed = Arc::new(AtomicBool::new(false));
        let frame_signal = frame_event.raw();
        let frame_arrived_token =
            frame_pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
                // SAFETY: the event outlives the handler: it is unregistered in
                // `close` before `frame_event` is dropped.
                let _ = unsafe { SetEvent(HANDLE(frame_signal as *mut _)) };
                Ok(())
            }))?;
        let closed_flag = Arc::clone(&closed);
        let closed_token = item.Closed(&TypedEventHandler::new(move |_, _| {
            closed_flag.store(true, Ordering::Release);
            // SAFETY: as above.
            let _ = unsafe { SetEvent(HANDLE(frame_signal as *mut _)) };
            Ok(())
        }))?;

        let textures = create_texture_pool(
            &context.device,
            size.Width.max(1) as u32,
            size.Height.max(1) as u32,
            options.pool_size,
        )?;
        let device_context = context.device.immediate_context()?;
        capture.StartCapture()?;

        Ok(Self {
            item,
            frame_pool,
            capture,
            winrt_device,
            context: device_context,
            frame_event,
            closed,
            frame_arrived_token,
            closed_token,
            textures,
            size,
            limiter: RateLimiter::new(options.fps, qpc_frequency()),
            next_frame_id: 0,
            last_delivery: None,
            cost: Vec::new(),
            frame_time_offset: Vec::new(),
            intervals: Vec::new(),
            border_hidden,
            min_update_interval_set,
        })
    }

    fn run_loop(&mut self, context: &SessionContext) -> CaptureEnd {
        let handles = [
            self.frame_event.handle(),
            HANDLE(context.stop_event as *mut _),
        ];
        loop {
            // SAFETY: both handles are valid events for the whole loop.
            let wait = unsafe { WaitForMultipleObjects(&handles, false, WAIT_TIMEOUT_MS) };
            if wait.0 == WAIT_OBJECT_0.0 + 1 {
                return CaptureEnd::Stopped;
            }
            if self.closed.load(Ordering::Acquire) || self.window_gone(context.target) {
                return CaptureEnd::SourceClosed;
            }

            // Keep only the newest waiting frame.
            let mut newest: Option<Direct3D11CaptureFrame> = None;
            while let Ok(frame) = self.frame_pool.TryGetNextFrame() {
                Counters::add(&context.counters.arrived);
                if let Some(older) = newest.replace(frame) {
                    Counters::add(&context.counters.superseded);
                    let _ = older.Close();
                }
            }
            if let Some(frame) = newest
                && let Err(error) = self.process(frame, context)
            {
                return CaptureEnd::Failed(error.to_string());
            }
        }
    }

    /// Whether a captured window no longer exists. WGC's `Closed` event did
    /// not fire for a destroyed window in testing (Windows 11 25H2), so the
    /// window is also checked on every wake-up (at least every 250 ms).
    fn window_gone(&self, target: RawTarget) -> bool {
        match target {
            // SAFETY: IsWindow accepts any value and only queries.
            RawTarget::Window(raw) => !unsafe { IsWindow(Some(HWND(raw as *mut _))) }.as_bool(),
            RawTarget::Monitor(_) => false,
        }
    }

    fn process(
        &mut self,
        frame: Direct3D11CaptureFrame,
        context: &SessionContext,
    ) -> windows::core::Result<()> {
        let start = qpc_now();
        let counters = &context.counters;
        if !self.limiter.admit(start) {
            Counters::add(&counters.rate_dropped);
            return frame.Close();
        }

        let content = frame.ContentSize()?;
        if content.Width <= 0 || content.Height <= 0 {
            return frame.Close();
        }
        if content != self.size {
            // Resize: WGC's buffers and our pool must match the new size.
            // Leases of the old pool stay valid until their holders drop them.
            frame.Close()?;
            self.frame_pool
                .Recreate(&self.winrt_device, PIXEL_FORMAT, WGC_BUFFERS, content)?;
            self.textures = create_texture_pool(
                &context.device,
                content.Width as u32,
                content.Height as u32,
                context.options.pool_size,
            )?;
            self.size = content;
            Counters::add(&counters.resizes);
            return Ok(());
        }

        let Some(texture) = self.textures.try_acquire() else {
            Counters::add(&counters.pool_dropped);
            return frame.Close();
        };
        let source = surface_texture(&frame)?;
        let (width, height) = (content.Width as u32, content.Height as u32);
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: both textures are BGRA8 on the same device; the box lies in
        // both (the pool matches the content size, WGC's buffer is at least
        // that big). The device is multithread-protected.
        unsafe {
            self.context
                .CopySubresourceRegion(&*texture, 0, 0, 0, 0, &source, 0, Some(&region));
        }
        let captured_at = timespan_to_qpc(frame.SystemRelativeTime()?.Duration, qpc_frequency());
        drop(source);
        frame.Close()?;

        let gpu_frame = GpuFrame {
            texture,
            width,
            height,
            frame_id: self.next_frame_id,
            captured_at,
        };
        self.next_frame_id += 1;
        if context.slot.put(gpu_frame) {
            Counters::add(&counters.replaced);
        }
        Counters::add(&counters.delivered);

        let end = qpc_now();
        if self.cost.len() < telemetry::DEFAULT_SAMPLE_CAPACITY {
            self.cost.push(end.duration_since(start));
            self.frame_time_offset
                .push(end.ticks() - captured_at.ticks());
            if let Some(previous) = self.last_delivery {
                self.intervals.push(end.duration_since(previous));
            }
        }
        self.last_delivery = Some(end);
        Ok(())
    }

    fn close(&mut self) {
        let _ = self.frame_pool.RemoveFrameArrived(self.frame_arrived_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.capture.Close();
        let _ = self.frame_pool.Close();
    }
}
