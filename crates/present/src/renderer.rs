//! Draws an NV12 frame into a BGRA render target with the D3D11 video
//! processor: BT.709 limited-range YCbCr to full-range RGB, scaled to fit
//! the target with its aspect ratio kept (black bars around it).
//!
//! Independent of any window: it renders into whatever target it is given
//! (the presenter's swap chain back buffer, or a UI's own back buffer), so
//! a UI can reuse it without owning the presenter's window.

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use gpu::D3d11Device;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_RATIONAL,
};
use windows::core::{BOOL, Interface};

/// Views are cached per texture; the presenter's pool and swap chain have
/// a few textures, so the cache is small and cleared when it grows.
const VIEW_CACHE_LIMIT: usize = 16;

/// Where a `source` sized picture goes inside a `target` sized area: as
/// large as possible with the same aspect ratio, centered.
pub fn letterbox(source: (u32, u32), target: (u32, u32)) -> RECT {
    let (sw, sh) = (u64::from(source.0.max(1)), u64::from(source.1.max(1)));
    let (tw, th) = (u64::from(target.0), u64::from(target.1));
    // Width-limited if source is relatively wider than the target.
    let (w, h) = if sw * th >= sh * tw {
        (tw, sh * tw / sw)
    } else {
        (sw * th / sh, th)
    };
    let left = (tw - w) / 2;
    let top = (th - h) / 2;
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + w) as i32,
        bottom: (top + h) as i32,
    }
}

struct Processor {
    source: (u32, u32),
    target: (u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

pub struct FrameRenderer {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    processor: Option<Processor>,
    input_views: HashMap<usize, ID3D11VideoProcessorInputView>,
    output_views: HashMap<usize, ID3D11VideoProcessorOutputView>,
}

impl FrameRenderer {
    pub fn new(device: &D3d11Device) -> windows::core::Result<Self> {
        Ok(Self {
            video_device: device.raw().cast()?,
            video_context: device.immediate_context()?.cast()?,
            processor: None,
            input_views: HashMap::new(),
            output_views: HashMap::new(),
        })
    }

    /// Forgets cached views, e.g. before the swap chain buffers are resized
    /// (views keep their textures alive).
    pub fn release_views(&mut self) {
        self.input_views.clear();
        self.output_views.clear();
    }

    /// Draws the top-left `source_size` area of `source` (an NV12 texture,
    /// array slice 0) letterboxed into the whole of `target` (BGRA, a
    /// render target of `target_size`).
    pub fn render(
        &mut self,
        source: &ID3D11Texture2D,
        source_size: (u32, u32),
        target: &ID3D11Texture2D,
        target_size: (u32, u32),
    ) -> windows::core::Result<()> {
        if target_size.0 == 0 || target_size.1 == 0 {
            return Ok(());
        }
        self.prepare(source_size, target_size)?;
        let input = self.input_view(source)?;
        let output = self.output_view(target)?;
        let processor = &self.processor.as_ref().expect("prepared").processor;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            pInputSurface: ManuallyDrop::new(Some(input)),
            ..Default::default()
        };
        // SAFETY: valid processor and views of this device; one stream.
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                processor,
                &output,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // SAFETY: the stream holds the only extra reference taken above.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result
    }

    /// (Re)creates the processor when the source or target size changes.
    fn prepare(&mut self, source: (u32, u32), target: (u32, u32)) -> windows::core::Result<()> {
        if self
            .processor
            .as_ref()
            .is_some_and(|current| current.source == source && current.target == target)
        {
            return Ok(());
        }
        // Views belong to an enumerator; new enumerator, new views.
        self.release_views();
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source.0,
            InputHeight: source.1,
            OutputFrameRate: rate,
            OutputWidth: target.0,
            OutputHeight: target.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: valid video device; the descriptor is a live local.
        let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&desc) }?;
        // SAFETY: valid device and enumerator; rate conversion index 0 exists.
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0) }?;
        let source_rect = RECT {
            left: 0,
            top: 0,
            right: source.0 as i32,
            bottom: source.1 as i32,
        };
        let target_rect = RECT {
            left: 0,
            top: 0,
            right: target.0 as i32,
            bottom: target.1 as i32,
        };
        let picture = letterbox(source, target);
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
        // SAFETY: valid context and processor; stream 0 exists; the rects
        // and color are live locals.
        unsafe {
            let context = &self.video_context;
            context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            // Plain conversion and scaling: no driver "enhancements".
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source_rect));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&picture));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target_rect));
            context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        }
        self.processor = Some(Processor {
            source,
            target,
            enumerator,
            processor,
        });
        Ok(())
    }

    fn input_view(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> windows::core::Result<ID3D11VideoProcessorInputView> {
        let key = texture.as_raw() as usize;
        if let Some(view) = self.input_views.get(&key) {
            return Ok(view.clone());
        }
        if self.input_views.len() >= VIEW_CACHE_LIMIT {
            self.input_views.clear();
        }
        let enumerator = &self.processor.as_ref().expect("prepared").enumerator;
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
        // SAFETY: valid device, texture and enumerator; live out-pointer.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                enumerator,
                &desc,
                Some(&mut view),
            )
        }?;
        let view = view.expect("CreateVideoProcessorInputView succeeded");
        self.input_views.insert(key, view.clone());
        Ok(view)
    }

    fn output_view(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> windows::core::Result<ID3D11VideoProcessorOutputView> {
        let key = texture.as_raw() as usize;
        if let Some(view) = self.output_views.get(&key) {
            return Ok(view.clone());
        }
        if self.output_views.len() >= VIEW_CACHE_LIMIT {
            self.output_views.clear();
        }
        let enumerator = &self.processor.as_ref().expect("prepared").enumerator;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        // SAFETY: as in `input_view`; the target is a render target.
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                texture,
                enumerator,
                &desc,
                Some(&mut view),
            )
        }?;
        let view = view.expect("CreateVideoProcessorOutputView succeeded");
        self.output_views.insert(key, view.clone());
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn same_aspect_fills_the_target() {
        assert_eq!(letterbox((1920, 1080), (1280, 720)), rect(0, 0, 1280, 720));
        assert_eq!(letterbox((1280, 720), (1280, 720)), rect(0, 0, 1280, 720));
    }

    #[test]
    fn wider_target_gets_side_bars() {
        // 16:9 in 21:9: bars left and right.
        assert_eq!(
            letterbox((1920, 1080), (2560, 1080)),
            rect(320, 0, 2240, 1080)
        );
    }

    #[test]
    fn taller_target_gets_top_and_bottom_bars() {
        // 16:9 in 4:3.
        assert_eq!(letterbox((1920, 1080), (1024, 768)), rect(0, 96, 1024, 672));
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        assert_eq!(letterbox((0, 0), (100, 50)), rect(25, 0, 75, 50));
        assert_eq!(letterbox((1920, 1080), (0, 0)), rect(0, 0, 0, 0));
    }
}
