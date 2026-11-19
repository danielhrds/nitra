//! DXGI Desktop Duplication for monitors.
//!
//! The capture thread polls `AcquireNextFrame` without waiting, with a
//! short sleep between attempts (see `POLL_INTERVAL`), hands each new desktop image to the shared
//! delivery and calls `ReleaseFrame` right away; every acquire is paired
//! with a release before the next one, as the API requires. When the
//! duplication is lost (display mode change, fullscreen transitions,
//! secure desktop such as UAC or the lock screen) it is recreated and
//! counted in `access_lost`.
//!
//! Differences from WGC: monitors only, the cursor is never drawn into the
//! image, and there is no system-side rate cap, so every composed frame
//! (up to the refresh rate) is acquired and the excess is dropped by the
//! rate limiter.

use gpu::{D3d11Device, Output};
use telemetry::{QpcTimestamp, qpc_now};
use windows::Win32::Foundation::{E_POINTER, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIOutput,
    IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Interface;

use crate::common::{
    AcceptedSettings, Backend, Capture, CaptureEnd, CaptureOptions, Counters, Delivery, Samples,
    ThreadContext, spawn,
};

/// `AcquireNextFrame` never waits: while it waits it blocks other threads'
/// calls on the (multithread-protected) device, which stalled the encode
/// thread of the pipeline for up to the whole timeout on a static desktop.
const ACQUIRE_TIMEOUT_MS: u32 = 0;
/// Pause between polls when no new desktop image is ready (std's sleep uses
/// a high-resolution timer on Windows).
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_micros(500);
/// Pause between attempts to recreate a lost duplication.
const RETRY_DELAY_MS: u32 = 250;

/// Makes the process per-monitor DPI aware (v2), which `DuplicateOutput1`
/// requires. Process-wide; call once at start-up, before creating windows.
/// Returns `false` if the awareness was already set (e.g. by a manifest).
pub fn enable_per_monitor_dpi_awareness() -> bool {
    // SAFETY: plain process setting; fails harmlessly if already set.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.is_ok()
}

/// Starts duplicating `output`, which must belong to the adapter `device`
/// was created on. The process must be per-monitor DPI aware.
pub fn start(
    device: &D3d11Device,
    output: &Output,
    options: CaptureOptions,
) -> Result<Capture, String> {
    let output = output.handle().clone();
    spawn("dxgi", device, options, move |context| {
        DxgiBackend::start(context, &output)
    })
}

struct DxgiBackend {
    output: IDXGIOutput5,
    duplication: Option<IDXGIOutputDuplication>,
    delivery: Delivery,
}

impl DxgiBackend {
    fn start(context: &ThreadContext, output: &IDXGIOutput) -> windows::core::Result<Self> {
        let output: IDXGIOutput5 = output.cast()?;
        let duplication = duplicate(&output, &context.device)?;
        // SAFETY: valid duplication; the descriptor is returned by value.
        let desc = unsafe { duplication.GetDesc() };
        let delivery = Delivery::new(
            &context.device,
            &context.options,
            desc.ModeDesc.Width,
            desc.ModeDesc.Height,
        )?;
        Ok(Self {
            output,
            duplication: Some(duplication),
            delivery,
        })
    }

    fn lose_access(&mut self, context: &ThreadContext) {
        Counters::add(&context.counters.access_lost, 1);
        self.duplication = None;
    }

    /// Handles one acquired frame; the caller releases it afterwards.
    /// Returns when handling started if the frame was delivered.
    fn handle_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
        context: &ThreadContext,
    ) -> windows::core::Result<Option<QpcTimestamp>> {
        // A zero present time means only the mouse moved: no new image.
        if info.LastPresentTime == 0 {
            return Ok(None);
        }
        let counters = &context.counters;
        let accumulated = u64::from(info.AccumulatedFrames.max(1));
        Counters::add(&counters.arrived, accumulated);
        Counters::add(&counters.superseded, accumulated - 1);

        let started = qpc_now();
        if !self.delivery.admit(started, counters) {
            return Ok(None);
        }
        let texture: ID3D11Texture2D = resource
            .ok_or(windows::core::Error::from(E_POINTER))?
            .cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; the out-pointer references a live local.
        unsafe { texture.GetDesc(&mut desc) };
        if (desc.Width, desc.Height) != self.delivery.size() {
            self.delivery
                .resize(&context.device, desc.Width, desc.Height, counters)?;
        }
        let captured_at = QpcTimestamp::from_ticks(info.LastPresentTime);
        let delivered = self.delivery.deliver(&texture, captured_at, context);
        Ok(delivered.then_some(started))
    }
}

fn duplicate(
    output: &IDXGIOutput5,
    device: &D3d11Device,
) -> windows::core::Result<IDXGIOutputDuplication> {
    // SAFETY: valid output and device (same adapter, checked by the API);
    // the format list is a live slice.
    unsafe { output.DuplicateOutput1(device.raw(), 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) }
}

impl Backend for DxgiBackend {
    fn run(&mut self, context: &ThreadContext) -> CaptureEnd {
        loop {
            if context.stop_requested() {
                return CaptureEnd::Stopped;
            }
            let Some(duplication) = self.duplication.clone() else {
                match duplicate(&self.output, &context.device) {
                    Ok(duplication) => self.duplication = Some(duplication),
                    Err(_) => {
                        // Still on a secure desktop or mid mode change: retry
                        // later, waking up early if asked to stop.
                        // SAFETY: valid event handle.
                        if unsafe { WaitForSingleObject(context.stop_handle(), RETRY_DELAY_MS) }
                            == WAIT_OBJECT_0
                        {
                            return CaptureEnd::Stopped;
                        }
                    }
                }
                continue;
            };

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            // SAFETY: valid duplication; out-pointers reference live locals.
            match unsafe {
                duplication.AcquireNextFrame(ACQUIRE_TIMEOUT_MS, &mut info, &mut resource)
            } {
                Ok(()) => {}
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
                Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
                    self.lose_access(context);
                    continue;
                }
                Err(error) => return CaptureEnd::Failed(format!("AcquireNextFrame: {error}")),
            }

            let handled = self.handle_frame(&info, resource, context);
            // SAFETY: releases the frame acquired above, before any new acquire.
            let released = unsafe { duplication.ReleaseFrame() };
            if let (Ok(Some(started)), Ok(())) = (&handled, &released) {
                self.delivery.record_cost(*started);
            }
            for result in [handled.map(|_| ()), released] {
                match result {
                    Ok(()) => {}
                    Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
                        self.lose_access(context)
                    }
                    Err(error) => return CaptureEnd::Failed(error.to_string()),
                }
            }
        }
    }

    fn close(&mut self) {
        self.duplication = None;
    }

    fn into_results(self) -> (Samples, AcceptedSettings) {
        (self.delivery.into_samples(), AcceptedSettings::default())
    }
}
