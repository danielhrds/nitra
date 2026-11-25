//! Cameras as frame sources: list them by name and read one into BGRA
//! frames on the GPU, delivered like any capture (a [`Capture`] handle).
//!
//! Cameras hand their frames over in memory, so each frame is copied to the
//! GPU once (the one agreed exception to keeping pixels off the CPU) and
//! converted there; see `convert`. A camera Media Foundation sees is read
//! through it; one only DirectShow offers (the OBS Virtual Camera), through
//! DirectShow.
//!
//! A camera turns on (its light too) only in [`start_camera`] and off when
//! the capture stops.

mod convert;
mod direct_show;
mod media_foundation;
mod mode;

use std::sync::{Arc, Mutex};

use gpu::D3d11Device;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

pub use mode::{CameraMode, CameraRequest, PixelFormat, choose};

use crate::common::{AcceptedSettings, Backend, Samples, ThreadContext, spawn};
use crate::{Capture, CaptureEnd, CaptureOptions};

/// Which system API reads a camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraApi {
    MediaFoundation,
    DirectShow,
}

/// A camera the system offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraInfo {
    /// What Windows calls it, e.g. "Iriun Webcam"; how a camera is picked.
    pub name: String,
    pub api: CameraApi,
}

/// The cameras there are now, without opening any (no light goes on):
/// Media Foundation's, then those only DirectShow offers.
pub fn list_cameras() -> Result<Vec<CameraInfo>, String> {
    on_mta_thread(|| {
        let _media_foundation = mf::MediaFoundation::start()
            .map_err(|error| format!("starting Media Foundation: {error}"))?;
        cameras()
    })
}

fn cameras() -> Result<Vec<CameraInfo>, String> {
    let mut cameras: Vec<CameraInfo> = media_foundation::camera_names()
        .map_err(|error| format!("listing cameras: {error}"))?
        .into_iter()
        .map(|name| CameraInfo {
            name,
            api: CameraApi::MediaFoundation,
        })
        .collect();
    // DirectShow lists the Media Foundation cameras too; a failure here
    // only loses the DirectShow-only ones.
    match direct_show::camera_names() {
        Ok(names) => {
            for name in names {
                if !cameras.iter().any(|c| c.name == name) {
                    cameras.push(CameraInfo {
                        name,
                        api: CameraApi::DirectShow,
                    });
                }
            }
        }
        Err(error) => tracing::warn!(%error, "listing DirectShow cameras"),
    }
    Ok(cameras)
}

/// Runs `f` on a short thread of its own in the MTA, whatever apartment the
/// caller is in.
fn on_mta_thread<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    std::thread::Builder::new()
        .name("camera-list".into())
        .spawn(|| {
            // SAFETY: joins the MTA for this short thread; balanced below.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .map_err(|error| format!("COM initialization: {error}"))?;
            let result = f();
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
            result
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
        let media_foundation = mf::MediaFoundation::start()?;
        let through_media_foundation = media_foundation::camera_names()?.contains(&name);
        let (camera, mode) = if through_media_foundation {
            let (camera, mode) = media_foundation::MfCamera::open(context, &name, request)?;
            (AnyCamera::MediaFoundation(camera), mode)
        } else {
            let (camera, mode) = direct_show::DsCamera::open(context, &name, request)?;
            (AnyCamera::DirectShow(camera), mode)
        };
        *mode_out.lock().expect("not poisoned") = Some(mode);
        // The camera keeps its own Media Foundation start when it needs one.
        drop(media_foundation);
        Ok(camera)
    })?;
    let mode = chosen
        .lock()
        .expect("not poisoned")
        .take()
        .expect("set before the capture reported it started");
    Ok((capture, mode))
}

enum AnyCamera {
    MediaFoundation(media_foundation::MfCamera),
    DirectShow(direct_show::DsCamera),
}

impl Backend for AnyCamera {
    fn run(&mut self, context: &ThreadContext) -> CaptureEnd {
        match self {
            AnyCamera::MediaFoundation(camera) => camera.run(context),
            AnyCamera::DirectShow(camera) => camera.run(context),
        }
    }

    fn close(&mut self) {
        match self {
            AnyCamera::MediaFoundation(camera) => camera.close(),
            AnyCamera::DirectShow(camera) => camera.close(),
        }
    }

    fn into_results(self) -> (Samples, AcceptedSettings) {
        match self {
            AnyCamera::MediaFoundation(camera) => camera.into_results(),
            AnyCamera::DirectShow(camera) => camera.into_results(),
        }
    }
}
