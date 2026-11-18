//! Conversion with the D3D11 video processor (`VideoProcessorBlt`): color
//! conversion and scaling in one call, implemented by the driver.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL,
};
use windows::core::{BOOL, Interface};

use super::{ConvertConfig, ConvertError, Converter, ViewCache};
use crate::device::D3d11Device;
use crate::frame::{TexturePool, create_nv12_pool};
use crate::pool::Lease;

/// Cached views per texture; inputs come from small pools or rings.
const VIEW_CACHE_LIMIT: usize = 32;

pub struct VideoProcessorConverter {
    config: ConvertConfig,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    outputs: TexturePool,
    input_views: ViewCache<ID3D11VideoProcessorInputView>,
    output_views: ViewCache<ID3D11VideoProcessorOutputView>,
    pool_dropped: u64,
}

impl VideoProcessorConverter {
    pub fn new(device: &D3d11Device, config: ConvertConfig) -> Result<Self, ConvertError> {
        config.validate()?;
        let video_device: ID3D11VideoDevice = device.raw().cast()?;
        let video_context: ID3D11VideoContext1 = device.immediate_context()?.cast()?;

        let frame_rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: frame_rate,
            InputWidth: config.input_width,
            InputHeight: config.input_height,
            OutputFrameRate: frame_rate,
            OutputWidth: config.output_width,
            OutputHeight: config.output_height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: valid video device; the descriptor is a live local.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }?;

        // SAFETY: valid enumerator; plain capability queries.
        let (input_support, output_support) = unsafe {
            (
                enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?,
                enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?,
            )
        };
        if input_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
            || output_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            return Err(ConvertError::Unsupported(
                "video processor cannot convert BGRA8 to NV12".to_owned(),
            ));
        }

        // SAFETY: valid device and enumerator; rate conversion index 0 always exists.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }?;
        let source = RECT {
            left: 0,
            top: 0,
            right: config.input_width as i32,
            bottom: config.input_height as i32,
        };
        let target = RECT {
            left: 0,
            top: 0,
            right: config.output_width as i32,
            bottom: config.output_height as i32,
        };
        // SAFETY: valid context and processor; stream 0 exists; rects are live locals.
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            // Plain conversion and scaling: no driver "enhancements".
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamColorSpace1(
                &processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            video_context.VideoProcessorSetOutputColorSpace1(
                &processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&target));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
        }

        Ok(Self {
            outputs: create_nv12_pool(
                device,
                config.output_width,
                config.output_height,
                config.pool_size,
            )?,
            config,
            video_device,
            video_context,
            enumerator,
            processor,
            input_views: ViewCache::new(VIEW_CACHE_LIMIT),
            output_views: ViewCache::new(VIEW_CACHE_LIMIT),
            pool_dropped: 0,
        })
    }

    fn input_view(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> windows::core::Result<ID3D11VideoProcessorInputView> {
        let (video_device, enumerator) = (&self.video_device, &self.enumerator);
        self.input_views.get_or_create(texture, || {
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
                    texture,
                    enumerator,
                    &desc,
                    Some(&mut view),
                )
            }?;
            Ok(view.expect("CreateVideoProcessorInputView succeeded"))
        })
    }

    fn output_view(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> windows::core::Result<ID3D11VideoProcessorOutputView> {
        let (video_device, enumerator) = (&self.video_device, &self.enumerator);
        self.output_views.get_or_create(texture, || {
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view = None;
            // SAFETY: as in `input_view`; NV12 pool textures are render targets.
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    texture,
                    enumerator,
                    &desc,
                    Some(&mut view),
                )
            }?;
            Ok(view.expect("CreateVideoProcessorOutputView succeeded"))
        })
    }
}

impl Converter for VideoProcessorConverter {
    fn method(&self) -> &'static str {
        "video-processor"
    }

    fn config(&self) -> ConvertConfig {
        self.config
    }

    fn convert(
        &mut self,
        source: &ID3D11Texture2D,
    ) -> Result<Option<Lease<ID3D11Texture2D>>, ConvertError> {
        let Some(output) = self.outputs.try_acquire() else {
            self.pool_dropped += 1;
            return Ok(None);
        };
        let input_view = self.input_view(source)?;
        let output_view = self.output_view(&output)?;

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ..Default::default()
        };
        // SAFETY: valid processor and views on the same device; one stream.
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // SAFETY: the stream holds the only extra reference taken above.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result?;
        Ok(Some(output))
    }

    fn pool_dropped(&self) -> u64 {
        self.pool_dropped
    }
}
