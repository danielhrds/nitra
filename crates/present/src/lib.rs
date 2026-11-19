//! `VideoPresenter`: shows decoded video in a window of its own, on its own
//! thread, independent of any UI.
//!
//! ```text
//! decoder thread: present(texture, slice, size, meta)
//!   -> GPU copy of the slice into a presenter texture (pool of 3), decoder surface released
//!   -> newest-wins hand-off (a frame not shown yet is replaced and counted)
//! presenter thread: window + flip-model swap chain (frame latency 1)
//!   -> FrameRenderer (NV12 -> RGB, scaled, letterboxed) -> overlay -> Present
//! ```
//!
//! Frames are shown on the next refresh after they arrive; the swap chain
//! holds at most one frame in flight, so nothing queues up. F11 or
//! Alt+Enter toggle a borderless fullscreen window; O toggles the overlay.

mod display;
mod overlay;
mod renderer;
mod window;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use gpu::{D3d11Device, FrameFormat, Lease, TexturePool};
use serde::Serialize;
use telemetry::{QpcTimestamp, StageRecorder};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

pub use display::{DisplayCounters, DisplayReport, DisplayedFrame};
pub use renderer::{FrameRenderer, letterbox};

/// Presenter textures: one shown, one waiting, one being copied.
const POOL_SIZE: usize = 3;

#[derive(Clone, Debug)]
pub struct PresenterOptions {
    pub title: String,
    /// Initial client area size.
    pub width: u32,
    pub height: u32,
    /// A window without frame or title bar (its client area is the whole
    /// window, e.g. to capture it in tests).
    pub borderless: bool,
    pub overlay: bool,
    /// Open as a child window of this window (raw `HWND`), placed at the
    /// parent's top-left until [`VideoPresenter::set_bounds`] moves it: the
    /// video shown inside another app's window. `None`: a top-level window.
    pub parent: Option<isize>,
}

/// Identity and timing of a frame handed to the presenter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameMeta {
    pub frame_id: u64,
    pub captured_at: QpcTimestamp,
    /// When the decoder handed the frame out (start of the presenter's
    /// latency measurement).
    pub decoded_at: QpcTimestamp,
}

/// What `present` did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presented {
    Queued,
    /// Every presenter texture was in use; the frame was dropped (counted).
    Dropped,
    /// The window was closed (by the user, or after a device error).
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentError(pub String);

impl std::fmt::Display for PresentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PresentError {}

impl From<windows::core::Error> for PresentError {
    fn from(error: windows::core::Error) -> Self {
        PresentError(error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PresenterCounters {
    pub submitted: u64,
    /// Dropped in `present` because every presenter texture was in use.
    pub pool_dropped: u64,
    /// Replaced before being shown by a newer frame.
    pub replaced: u64,
    pub presented: u64,
    /// Presents of the last frame again (resize, overlay refresh).
    pub redraws: u64,
}

#[derive(Debug, Default)]
struct Counters {
    submitted: AtomicU64,
    pool_dropped: AtomicU64,
    replaced: AtomicU64,
    presented: AtomicU64,
    redraws: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> PresenterCounters {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        PresenterCounters {
            submitted: read(&self.submitted),
            pool_dropped: read(&self.pool_dropped),
            replaced: read(&self.replaced),
            presented: read(&self.presented),
            redraws: read(&self.redraws),
        }
    }
}

/// A frame waiting for the presenter thread.
struct QueuedFrame {
    texture: Lease<ID3D11Texture2D>,
    width: u32,
    height: u32,
    meta: FrameMeta,
}

/// State shared by the owner and the presenter thread.
struct Shared {
    next: Mutex<Option<QueuedFrame>>,
    /// Auto-reset event: a new frame (or a stop request) is waiting.
    wake: isize,
    closed: AtomicBool,
    error: Mutex<Option<String>>,
    counters: Counters,
}

impl Shared {
    fn wake_handle(&self) -> HANDLE {
        HANDLE(self.wake as *mut _)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: the event was created in `VideoPresenter::open` and both
        // users (owner and thread) are gone once the last Arc drops.
        let _ = unsafe { CloseHandle(self.wake_handle()) };
    }
}

/// What the presenter measured. `Present` stage samples: from the
/// decoder's hand-off (`FrameMeta::decoded_at`) to the `Present` call.
#[derive(Debug)]
pub struct PresenterReport {
    pub counters: PresenterCounters,
    pub stages: StageRecorder,
    /// When each new frame reached the screen (DXGI frame statistics).
    pub display: DisplayReport,
    /// Why the window closed early (device lost), if it did.
    pub error: Option<String>,
}

impl PresenterReport {
    fn failed(counters: PresenterCounters, error: String) -> Self {
        Self {
            counters,
            stages: StageRecorder::new(),
            display: DisplayReport::default(),
            error: Some(error),
        }
    }
}

pub struct VideoPresenter {
    context: ID3D11DeviceContext,
    device: D3d11Device,
    shared: Arc<Shared>,
    pool: Option<((u32, u32), TexturePool)>,
    window: isize,
    thread: Option<JoinHandle<PresenterReport>>,
}

impl VideoPresenter {
    /// Opens the window (on its own thread) and returns once it is ready.
    pub fn open(device: &D3d11Device, options: PresenterOptions) -> Result<Self, PresentError> {
        device.enable_multithread_protection()?;
        // SAFETY: unnamed auto-reset event, closed in `Shared::drop`.
        let wake = unsafe { CreateEventW(None, false, false, None) }?;
        let shared = Arc::new(Shared {
            next: Mutex::new(None),
            wake: wake.0 as isize,
            closed: AtomicBool::new(false),
            error: Mutex::new(None),
            counters: Counters::default(),
        });
        let (ready_sender, ready) = mpsc::sync_channel(1);
        let thread_device = device.clone();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("presenter".to_owned())
            .spawn(move || window::run(thread_device, options, thread_shared, ready_sender))
            .map_err(|error| PresentError(format!("presenter thread: {error}")))?;
        match ready.recv() {
            Ok(Ok(window)) => Ok(Self {
                context: device.immediate_context()?,
                device: device.clone(),
                shared,
                pool: None,
                window,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(PresentError(message))
            }
            Err(_) => {
                let _ = thread.join();
                Err(PresentError(
                    "presenter thread exited during start".to_owned(),
                ))
            }
        }
    }

    pub fn hwnd(&self) -> HWND {
        HWND(self.window as *mut _)
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    pub fn counters(&self) -> PresenterCounters {
        self.shared.counters.snapshot()
    }

    /// Moves and resizes the window (child windows: in the parent's client
    /// coordinates), in physical pixels. Asynchronous: returns without
    /// waiting for the presenter's window thread.
    pub fn set_bounds(&self, x: i32, y: i32, width: i32, height: i32) {
        use windows::Win32::UI::WindowsAndMessaging::{
            SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
        };
        // SAFETY: SetWindowPos may target a window of another thread; with
        // SWP_ASYNCWINDOWPOS it only posts the request. Fails harmlessly if
        // the window is gone.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd(),
                None,
                x,
                y,
                width.max(1),
                height.max(1),
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER,
            )
        };
    }

    /// Toggles the borderless fullscreen window (like F11).
    pub fn toggle_fullscreen(&self) {
        // SAFETY: posting to a window of another thread is allowed; it
        // fails harmlessly if the window is gone.
        let _ = unsafe {
            PostMessageW(
                Some(self.hwnd()),
                window::WM_TOGGLE_FULLSCREEN,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }

    /// Hands a decoded NV12 picture (array slice `slice` of `source`,
    /// `width` x `height` at its top-left) to the presenter. The picture is
    /// copied on the GPU, so `source` can be released (returned to the
    /// decoder) as soon as this returns.
    pub fn present(
        &mut self,
        source: &ID3D11Texture2D,
        slice: u32,
        width: u32,
        height: u32,
        meta: FrameMeta,
    ) -> Result<Presented, PresentError> {
        if self.is_closed() {
            return Ok(Presented::Closed);
        }
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(PresentError(format!(
                "NV12 pictures need even, non-zero sizes; got {width}x{height}"
            )));
        }
        self.shared
            .counters
            .submitted
            .fetch_add(1, Ordering::Relaxed);
        let size = (width, height);
        if self
            .pool
            .as_ref()
            .is_none_or(|(current, _)| *current != size)
        {
            self.pool = Some((size, create_pool(&self.device, width, height)?));
        }
        let pool = &self.pool.as_ref().expect("created above").1;
        let Some(texture) = pool.try_acquire() else {
            self.shared
                .counters
                .pool_dropped
                .fetch_add(1, Ordering::Relaxed);
            return Ok(Presented::Dropped);
        };
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; live out-pointer.
        unsafe { source.GetDesc(&mut source_desc) };
        if slice >= source_desc.ArraySize
            || width > source_desc.Width
            || height > source_desc.Height
        {
            return Err(PresentError(format!(
                "slice {slice} / {width}x{height} outside the source texture"
            )));
        }
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: same device and format (NV12); the even-aligned box lies
        // inside both textures (checked above). The copy is queued: the GPU
        // orders it after the decode and before the presenter's draw.
        unsafe {
            self.context.CopySubresourceRegion(
                &*texture,
                0,
                0,
                0,
                0,
                source,
                slice * source_desc.MipLevels,
                Some(&region),
            );
        }
        let frame = QueuedFrame {
            texture,
            width,
            height,
            meta,
        };
        let replaced = self
            .shared
            .next
            .lock()
            .expect("presenter lock")
            .replace(frame)
            .is_some();
        if replaced {
            self.shared
                .counters
                .replaced
                .fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: valid event owned by `shared`.
        unsafe { SetEvent(self.shared.wake_handle()) }?;
        Ok(Presented::Queued)
    }

    /// Closes the window and returns what the presenter measured.
    pub fn close(mut self) -> PresenterReport {
        self.join().expect("joined once")
    }

    fn join(&mut self) -> Option<PresenterReport> {
        let thread = self.thread.take()?;
        // SAFETY: as in `toggle_fullscreen`.
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        let _ = unsafe { SetEvent(self.shared.wake_handle()) };
        Some(thread.join().unwrap_or_else(|_| {
            PresenterReport::failed(
                self.shared.counters.snapshot(),
                "presenter thread panicked".to_owned(),
            )
        }))
    }
}

impl Drop for VideoPresenter {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// NV12 textures the presenter copies decoded slices into: copy
/// destinations and video processor inputs.
fn create_pool(device: &D3d11Device, width: u32, height: u32) -> Result<TexturePool, PresentError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: FrameFormat::Nv12.dxgi_format(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let textures = (0..POOL_SIZE)
        .map(|_| {
            let mut texture = None;
            // SAFETY: valid device; descriptor and out-pointer are live locals.
            unsafe {
                device
                    .raw()
                    .CreateTexture2D(&desc, None, Some(&mut texture))
            }?;
            Ok(texture.expect("CreateTexture2D succeeded"))
        })
        .collect::<windows::core::Result<Vec<_>>>()?;
    Ok(gpu::Pool::new(textures))
}
