//! Cameras as frame sources: list them by name and read one into BGRA
//! frames on the GPU, delivered like any capture (a [`Capture`] handle).
//!
//! Cameras hand their frames over in memory, so each frame is copied to the
//! GPU once (the one agreed exception to keeping pixels off the CPU) and
//! converted there; see `convert`. Media Foundation for now; the DirectShow
//! cameras (the OBS Virtual Camera) come next.
//!
//! A camera turns on (its light too) only in [`start_camera`] and off when
//! the capture stops.

mod convert;
mod media_foundation;
mod mode;

use std::sync::{Arc, Mutex};

use gpu::D3d11Device;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

pub use mode::{CameraMode, CameraRequest, PixelFormat, choose};

use crate::common::spawn;
use crate::{Capture, CaptureOptions};

/// A camera the system offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraInfo {
    /// What Windows calls it, e.g. "Iriun Webcam"; how a camera is picked.
    pub name: String,
}

/// The cameras there are now, without opening any (no light goes on).
pub fn list_cameras() -> Result<Vec<CameraInfo>, String> {
    // On a thread of its own, in the MTA, whatever apartment the caller is in.
    std::thread::Builder::new()
        .name("camera-list".into())
        .spawn(|| {
            // SAFETY: joins the MTA for this short thread; balanced below.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .map_err(|error| format!("COM initialization: {error}"))?;
            let names = mf::MediaFoundation::start()
                .and_then(|_media_foundation| media_foundation::camera_names())
                .map_err(|error| format!("listing cameras: {error}"));
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
            Ok(names?.into_iter().map(|name| CameraInfo { name }).collect())
        })
        .map_err(|error| format!("camera list thread: {error}"))?
        .join()
        .map_err(|_| "the camera list thread panicked".to_owned())?
}

/// Turns on the first camera called `name` in the mode closest to
/// `request`, and delivers its frames (BGRA, the camera's size) at up to
/// `options.fps`. Returns the capture and the camera's mode.
pub fn start_camera(
    device: &D3d11Device,
    name: &str,
    request: CameraRequest,
    options: CaptureOptions,
) -> Result<(Capture, CameraMode), String> {
    let chosen = Arc::new(Mutex::new(None));
    let (name, mode_out) = (name.to_owned(), Arc::clone(&chosen));
    let capture = spawn("camera", device, options, move |context| {
        let (camera, mode) = media_foundation::MfCamera::open(context, &name, request)?;
        *mode_out.lock().expect("not poisoned") = Some(mode);
        Ok(camera)
    })?;
    let mode = chosen
        .lock()
        .expect("not poisoned")
        .take()
        .expect("set before the capture reported it started");
    Ok((capture, mode))
}
