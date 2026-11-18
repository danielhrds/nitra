//! Windows.Graphics.Capture (WGC) for monitors and windows.
//!
//! WGC signals new frames through an event; the capture thread takes the
//! newest one and hands it to the shared delivery (rate limit, copy into
//! the pool, newest-wins hand-off). WGC's buffer is released right after
//! the copy, so WGC never waits for consumers.

mod interop;
mod session;

use gpu::D3d11Device;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HMONITOR;

use crate::common::{Capture, CaptureOptions, spawn};

pub use interop::{find_window_by_title, timespan_to_qpc};

/// What to capture.
#[derive(Clone, Copy, Debug)]
pub enum CaptureTarget {
    Monitor(HMONITOR),
    Window(HWND),
}

/// Starts capturing `target` with frames copied on `device`.
pub fn start(
    device: &D3d11Device,
    target: CaptureTarget,
    options: CaptureOptions,
) -> Result<Capture, String> {
    let target = session::RawTarget::from(target);
    spawn("wgc", device, options, move |context| {
        session::WgcBackend::start(context, target)
    })
}
