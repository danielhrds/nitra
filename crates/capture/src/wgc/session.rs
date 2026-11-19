//! The WGC backend running on the capture thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use telemetry::{qpc_frequency, qpc_now};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Threading::{SetEvent, WaitForMultipleObjects};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use super::CaptureTarget;
use super::interop::{
    item_for_monitor, item_for_window, surface_texture, timespan_to_qpc, winrt_device,
};
use crate::common::{
    AcceptedSettings, Backend, CaptureEnd, Counters, Delivery, OwnedEvent, Samples, ThreadContext,
};

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

pub(super) struct WgcBackend {
    target: RawTarget,
    item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    winrt_device: IDirect3DDevice,
    frame_event: OwnedEvent,
    closed: Arc<AtomicBool>,
    frame_arrived_token: i64,
    closed_token: i64,
    delivery: Delivery,
    settings: AcceptedSettings,
}

impl WgcBackend {
    pub(super) fn start(context: &ThreadContext, target: RawTarget) -> windows::core::Result<Self> {
        let item = match target {
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
        let session = frame_pool.CreateCaptureSession(&item)?;

        let options = context.options;
        let border_hidden = options.hide_border && session.SetIsBorderRequired(false).is_ok();
        // Optional settings: older Windows builds lack them, which is fine.
        let _ = session.SetIsCursorCaptureEnabled(options.cursor);
        // Cap how often WGC sends frames (each one is a copy in the
        // compositor); the rate limiter keeps the exact cadence.
        let min_interval = TimeSpan {
            Duration: min_update_interval(options.fps, options.wgc_interval_factor),
        };
        let min_update_interval_set = session.SetMinUpdateInterval(min_interval).is_ok();

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

        let delivery = Delivery::new(
            &context.device,
            &options,
            size.Width.max(1) as u32,
            size.Height.max(1) as u32,
        )?;
        session.StartCapture()?;

        Ok(Self {
            target,
            item,
            frame_pool,
            session,
            winrt_device,
            frame_event,
            closed,
            frame_arrived_token,
            closed_token,
            delivery,
            settings: AcceptedSettings {
                border_hidden,
                min_update_interval_set,
            },
        })
    }

    /// Whether a captured window no longer exists. WGC's `Closed` event did
    /// not fire for a destroyed window in testing (Windows 11 25H2), so the
    /// window is also checked on every wake-up (at least every 250 ms).
    fn window_gone(&self) -> bool {
        match self.target {
            // SAFETY: IsWindow accepts any value and only queries.
            RawTarget::Window(raw) => !unsafe { IsWindow(Some(HWND(raw as *mut _))) }.as_bool(),
            RawTarget::Monitor(_) => false,
        }
    }

    fn process(
        &mut self,
        frame: Direct3D11CaptureFrame,
        context: &ThreadContext,
    ) -> windows::core::Result<()> {
        let started = qpc_now();
        if !self.delivery.admit(started, &context.counters) {
            return frame.Close();
        }
        let content = frame.ContentSize()?;
        if content.Width <= 0 || content.Height <= 0 {
            return frame.Close();
        }
        let content_size = (content.Width as u32, content.Height as u32);
        if content_size != self.delivery.size() {
            // WGC's buffers and our pool must match the new size; this frame
            // is skipped and the next one arrives at the new size.
            frame.Close()?;
            self.frame_pool
                .Recreate(&self.winrt_device, PIXEL_FORMAT, WGC_BUFFERS, content)?;
            return self.delivery.resize(
                &context.device,
                content_size.0,
                content_size.1,
                &context.counters,
            );
        }

        let source = surface_texture(&frame)?;
        let captured_at = timespan_to_qpc(frame.SystemRelativeTime()?.Duration, qpc_frequency());
        let delivered = self.delivery.deliver(&source, captured_at, context);
        drop(source);
        frame.Close()?;
        if delivered {
            self.delivery.record_cost(started);
        }
        Ok(())
    }
}

impl Backend for WgcBackend {
    fn run(&mut self, context: &ThreadContext) -> CaptureEnd {
        let handles = [self.frame_event.handle(), context.stop_handle()];
        loop {
            // SAFETY: both handles are valid events for the whole loop.
            let wait = unsafe { WaitForMultipleObjects(&handles, false, WAIT_TIMEOUT_MS) };
            if wait.0 == WAIT_OBJECT_0.0 + 1 {
                return CaptureEnd::Stopped;
            }
            if self.closed.load(Ordering::Acquire) || self.window_gone() {
                return CaptureEnd::SourceClosed;
            }

            // Keep only the newest waiting frame.
            let mut newest: Option<Direct3D11CaptureFrame> = None;
            while let Ok(frame) = self.frame_pool.TryGetNextFrame() {
                Counters::add(&context.counters.arrived, 1);
                if let Some(older) = newest.replace(frame) {
                    Counters::add(&context.counters.superseded, 1);
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

    fn close(&mut self) {
        let _ = self.frame_pool.RemoveFrameArrived(self.frame_arrived_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }

    fn into_results(self) -> (Samples, AcceptedSettings) {
        (self.delivery.into_samples(), self.settings)
    }
}

/// `MinUpdateInterval` in 100 ns units: `factor` times the frame period
/// of `fps`, at least one unit.
pub(crate) fn min_update_interval(fps: u32, factor: f64) -> i64 {
    let period = 10_000_000.0 / f64::from(fps.max(1));
    ((period * factor).round() as i64).max(1)
}

#[cfg(test)]
mod interval_tests {
    use super::min_update_interval;

    #[test]
    fn interval_is_a_fraction_of_the_frame_period() {
        assert_eq!(min_update_interval(60, 0.5), 83_333);
        assert_eq!(min_update_interval(60, 0.95), 158_333);
        assert_eq!(min_update_interval(60, 1.0), 166_667);
        assert_eq!(min_update_interval(0, 0.5), 5_000_000);
        assert_eq!(min_update_interval(60, 0.0), 1);
    }
}
