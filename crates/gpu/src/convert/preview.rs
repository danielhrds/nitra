//! A small NV12 picture of frames (what a video presenter shows), written
//! on one device and read on another (the share's device and the window's): a texture shared between
//! the two, guarded by a keyed mutex. Pixels never leave the GPU.
//!
//! The writer scales each frame into the shared texture with the video
//! processor (black bars keep the aspect ratio); the reader copies the
//! shared texture into one of its own, which it can then present at leisure.
//! Neither side ever waits: a texture the other side holds is skipped.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR,
    D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource};
use windows::core::{BOOL, HRESULT, Interface};

use super::{ConvertError, ViewCache};
use crate::device::D3d11Device;

/// The keyed mutex's only key: whoever holds it uses the texture.
const KEY: u64 = 0;
/// Cached input views (capture pools are small).
const VIEW_CACHE_LIMIT: usize = 32;

/// The shared texture's handle, for the reading device to open it. Valid
/// while the writer lives (a legacy shared handle: nothing to close).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedHandle(isize);

/// Tries to take the keyed mutex without waiting. The `windows` wrapper
/// folds WAIT_TIMEOUT (a success code) into `Ok`, so the raw HRESULT is read.
fn try_acquire(mutex: &IDXGIKeyedMutex) -> bool {
    // SAFETY: a valid keyed mutex; AcquireSync takes plain values.
    let hr: HRESULT =
        unsafe { (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), KEY, 0) };
    hr.0 == 0
}

fn release(mutex: &IDXGIKeyedMutex) {
    // SAFETY: called only after a successful `try_acquire` on this mutex.
    let _ = unsafe { mutex.ReleaseSync(KEY) };
}

/// The video processor for one input size.
struct Scaler {
    input: (u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: ID3D11VideoProcessorOutputView,
    input_views: ViewCache<ID3D11VideoProcessorInputView>,
}

/// Scales frames into the shared texture.
pub struct PreviewWriter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    texture: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: SharedHandle,
    size: (u32, u32),
    scaler: Option<Scaler>,
}

impl PreviewWriter {
    /// A `width` x `height` shared NV12 texture on `device` (even sizes).
    pub fn new(device: &D3d11Device, width: u32, height: u32) -> Result<Self, ConvertError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
        };
        let mut texture = None;
        // SAFETY: valid device; the descriptor and out-pointer are live locals.
        unsafe {
            device
                .raw()
                .CreateTexture2D(&desc, None, Some(&mut texture))
        }?;
        let texture = texture.expect("CreateTexture2D succeeded");
        let mutex: IDXGIKeyedMutex = texture.cast()?;
        // SAFETY: a texture created shared just above.
        let handle: HANDLE = unsafe { texture.cast::<IDXGIResource>()?.GetSharedHandle() }?;
        Ok(Self {
            video_device: device.raw().cast()?,
            video_context: device.immediate_context()?.cast()?,
            texture,
            mutex,
            handle: SharedHandle(handle.0 as isize),
            size: (width, height),
            scaler: None,
        })
    }

    pub fn handle(&self) -> SharedHandle {
        self.handle
    }

    /// Scales `source` (BGRA8) into the shared texture as NV12, fitted with black
    /// bars. False if the reader holds it now (this frame is skipped).
    pub fn write(&mut self, source: &ID3D11Texture2D) -> Result<bool, ConvertError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; the out-pointer is a live local.
        unsafe { source.GetDesc(&mut desc) };
        let input = (desc.Width, desc.Height);
        if self.scaler.as_ref().is_none_or(|s| s.input != input) {
            self.scaler = Some(self.scaler_for(input)?);
        }
        if !try_acquire(&self.mutex) {
            return Ok(false);
        }
        let result = self.blt(source);
        release(&self.mutex);
        result.map(|()| true)
    }

    fn blt(&mut self, source: &ID3D11Texture2D) -> Result<(), ConvertError> {
        let scaler = self.scaler.as_mut().expect("scaler set in write");
        let (video_device, enumerator) = (&self.video_device, &scaler.enumerator);
        let input_view = scaler.input_views.get_or_create(source, || {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut view = None;
            // SAFETY: valid device, texture and enumerator; out-pointer is a live local.
            unsafe {
                video_device.CreateVideoProcessorInputView(
                    source,
                    enumerator,
                    &desc,
                    Some(&mut view),
                )
            }?;
            Ok::<_, windows::core::Error>(view.expect("CreateVideoProcessorInputView succeeded"))
        })?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ..Default::default()
        };
        // SAFETY: valid processor and views on the same device; one stream.
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &scaler.processor,
                &scaler.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // SAFETY: the stream holds the only extra reference taken above.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        Ok(result?)
    }

    /// A video processor from `input` to the shared texture: the picture
    /// fitted in the middle, black around it.
    fn scaler_for(&self, input: (u32, u32)) -> Result<Scaler, ConvertError> {
        let (width, height) = self.size;
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: valid video device; the descriptor is a live local.
        let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&content) }?;
        // SAFETY: valid device and enumerator; rate conversion index 0 always exists.
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0) }?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        // SAFETY: the shared texture is a render target on this device.
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                &self.texture,
                &enumerator,
                &desc,
                Some(&mut output_view),
            )
        }?;
        let target = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let fitted = fit(input, self.size);
        let source = RECT {
            left: 0,
            top: 0,
            right: input.0 as i32,
            bottom: input.1 as i32,
        };
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                RGBA: D3D11_VIDEO_COLOR_RGBA {
                    R: 0.0,
                    G: 0.0,
                    B: 0.0,
                    A: 1.0,
                },
            },
        };
        let context = &self.video_context;
        // SAFETY: valid context and processor; stream 0 exists; rects and
        // the color are live locals.
        unsafe {
            context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&fitted));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
        }
        Ok(Scaler {
            input,
            enumerator,
            processor,
            output_view: output_view.expect("CreateVideoProcessorOutputView succeeded"),
            input_views: ViewCache::new(VIEW_CACHE_LIMIT),
        })
    }
}

/// Where a `source`-sized picture goes in `target`, keeping its aspect
/// ratio, centered (even sides).
fn fit((sw, sh): (u32, u32), (tw, th): (u32, u32)) -> RECT {
    let (sw, sh) = (f64::from(sw.max(1)), f64::from(sh.max(1)));
    let scale = (f64::from(tw) / sw).min(f64::from(th) / sh);
    let even = |v: f64| ((v / 2.0).round() as i32 * 2).max(2);
    let (w, h) = (
        even(sw * scale).min(tw as i32),
        even(sh * scale).min(th as i32),
    );
    let (x, y) = ((tw as i32 - w) / 2, (th as i32 - h) / 2);
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// The shared texture on the reading device, copied into a texture of its
/// own to present.
pub struct PreviewReader {
    context: ID3D11DeviceContext,
    shared: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    local: ID3D11Texture2D,
}

impl PreviewReader {
    /// Opens the writer's texture on `device` (same adapter).
    pub fn open(device: &D3d11Device, handle: SharedHandle) -> Result<Self, ConvertError> {
        let mut shared: Option<ID3D11Texture2D> = None;
        // SAFETY: a handle from `PreviewWriter`, alive while the writer is;
        // the out-pointer is a live local.
        unsafe {
            device
                .raw()
                .OpenSharedResource(HANDLE(handle.0 as *mut _), &mut shared)
        }?;
        let shared = shared.expect("OpenSharedResource succeeded");
        let mutex: IDXGIKeyedMutex = shared.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; the out-pointer is a live local.
        unsafe { shared.GetDesc(&mut desc) };
        desc.MiscFlags = 0;
        desc.BindFlags = 0;
        let mut local = None;
        // SAFETY: valid device; the descriptor and out-pointer are live locals.
        unsafe { device.raw().CreateTexture2D(&desc, None, Some(&mut local)) }?;
        let local = local.expect("CreateTexture2D succeeded");
        Ok(Self {
            context: device.immediate_context()?,
            shared,
            mutex,
            local,
        })
    }

    /// Copies the latest picture into the reader's own texture; false if
    /// the writer holds it now (the previous picture stays).
    pub fn refresh(&self) -> bool {
        if !try_acquire(&self.mutex) {
            return false;
        }
        // SAFETY: two textures of the same size and format on this device.
        unsafe { self.context.CopyResource(&self.local, &self.shared) };
        release(&self.mutex);
        true
    }

    /// The reader's own copy (what `refresh` last took), to present.
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_fit_with_bars_and_even_sides() {
        let r = |l, t, rr, b| RECT {
            left: l,
            top: t,
            right: rr,
            bottom: b,
        };
        // Same shape: all of it.
        assert_eq!(fit((1920, 1080), (640, 360)), r(0, 0, 640, 360));
        // Square: bars at the sides.
        assert_eq!(fit((1000, 1000), (640, 360)), r(140, 0, 500, 360));
        // Ultrawide: bars above and below.
        assert_eq!(fit((3440, 1440), (640, 360)), r(0, 46, 640, 314));
        // Nothing: still a valid rectangle.
        let empty = fit((0, 0), (640, 360));
        assert!(empty.right > empty.left && empty.bottom > empty.top);
    }
}
