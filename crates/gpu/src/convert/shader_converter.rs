//! Conversion with our own pixel shaders, rendering straight into the two
//! planes of the NV12 output (R8 view = Y plane, R8G8 view = CbCr plane,
//! supported for NV12 render targets since D3D 11.1).

use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_COMPARISON_NEVER, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RENDER_TARGET_VIEW_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC,
    D3D11_TEX2D_RTV, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_VIEWPORT, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
};

use super::{ConvertConfig, ConvertError, Converter, ViewCache};
use crate::device::D3d11Device;
use crate::frame::{TexturePool, create_nv12_pool};
use crate::pool::Lease;
use crate::shader::compile_shader;

const SHADER_SOURCE: &str = include_str!("nv12.hlsl");
const VIEW_CACHE_LIMIT: usize = 32;

pub struct ShaderConverter {
    config: ConvertConfig,
    device: D3d11Device,
    context: ID3D11DeviceContext,
    vertex_shader: ID3D11VertexShader,
    luma_shader: ID3D11PixelShader,
    chroma_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    outputs: TexturePool,
    source_views: ViewCache<ID3D11ShaderResourceView>,
    luma_targets: ViewCache<ID3D11RenderTargetView>,
    chroma_targets: ViewCache<ID3D11RenderTargetView>,
    pool_dropped: u64,
}

impl ShaderConverter {
    pub fn new(device: &D3d11Device, config: ConvertConfig) -> Result<Self, ConvertError> {
        config.validate()?;
        let raw = device.raw();
        let compile = |entry: &str, target: &str| {
            compile_shader(SHADER_SOURCE, "nv12.hlsl", entry, target)
                .map_err(|error| ConvertError::Unsupported(error.to_string()))
        };
        let (vs_code, luma_code, chroma_code) = (
            compile("vs_main", "vs_5_0")?,
            compile("ps_luma", "ps_5_0")?,
            compile("ps_chroma", "ps_5_0")?,
        );

        let (mut vertex_shader, mut luma_shader, mut chroma_shader, mut sampler) =
            (None, None, None, None);
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        // SAFETY: valid device; bytecode slices and descriptors are live for
        // the calls; out-pointers reference live locals.
        unsafe {
            raw.CreateVertexShader(vs_code.bytecode(), None, Some(&mut vertex_shader))?;
            raw.CreatePixelShader(luma_code.bytecode(), None, Some(&mut luma_shader))?;
            raw.CreatePixelShader(chroma_code.bytecode(), None, Some(&mut chroma_shader))?;
            raw.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;
        }

        Ok(Self {
            outputs: create_nv12_pool(
                device,
                config.output_width,
                config.output_height,
                config.pool_size,
            )?,
            config,
            device: device.clone(),
            context: device.immediate_context()?,
            vertex_shader: vertex_shader.expect("CreateVertexShader succeeded"),
            luma_shader: luma_shader.expect("CreatePixelShader succeeded"),
            chroma_shader: chroma_shader.expect("CreatePixelShader succeeded"),
            sampler: sampler.expect("CreateSamplerState succeeded"),
            source_views: ViewCache::new(VIEW_CACHE_LIMIT),
            luma_targets: ViewCache::new(VIEW_CACHE_LIMIT),
            chroma_targets: ViewCache::new(VIEW_CACHE_LIMIT),
            pool_dropped: 0,
        })
    }

    fn plane_target(
        device: &D3d11Device,
        cache: &mut ViewCache<ID3D11RenderTargetView>,
        texture: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11RenderTargetView, ConvertError> {
        cache.get_or_create(texture, || {
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut view = None;
            // SAFETY: valid device; the NV12 texture is a render target; an
            // R8 view selects its Y plane and an R8G8 view its CbCr plane.
            unsafe {
                device
                    .raw()
                    .CreateRenderTargetView(texture, Some(&desc), Some(&mut view))
            }
            .map_err(|error| {
                ConvertError::Unsupported(format!("NV12 plane render target ({format:?}): {error}"))
            })?;
            Ok(view.expect("CreateRenderTargetView succeeded"))
        })
    }
}

impl Converter for ShaderConverter {
    fn method(&self) -> &'static str {
        "shader"
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
        let device = &self.device;
        let source_view = self.source_views.get_or_create(source, || {
            let mut view = None;
            // SAFETY: valid device; BGRA8 inputs are created as shader resources.
            unsafe {
                device
                    .raw()
                    .CreateShaderResourceView(source, None, Some(&mut view))
            }?;
            Ok::<_, ConvertError>(view.expect("CreateShaderResourceView succeeded"))
        })?;
        let luma_target = Self::plane_target(
            device,
            &mut self.luma_targets,
            &output,
            DXGI_FORMAT_R8_UNORM,
        )?;
        let chroma_target = Self::plane_target(
            device,
            &mut self.chroma_targets,
            &output,
            DXGI_FORMAT_R8G8_UNORM,
        )?;

        let viewport = |width: u32, height: u32| D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let (width, height) = (self.config.output_width, self.config.output_height);
        let context = &self.context;
        // SAFETY: every object belongs to this device and outlives the
        // calls. Targets and the source view are unbound at the end so the
        // textures can be used elsewhere.
        unsafe {
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(None);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShaderResources(0, Some(&[Some(source_view)]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));

            context.OMSetRenderTargets(Some(&[Some(luma_target)]), None);
            context.RSSetViewports(Some(&[viewport(width, height)]));
            context.PSSetShader(&self.luma_shader, None);
            context.Draw(3, 0);

            context.OMSetRenderTargets(Some(&[Some(chroma_target)]), None);
            context.RSSetViewports(Some(&[viewport(width / 2, height / 2)]));
            context.PSSetShader(&self.chroma_shader, None);
            context.Draw(3, 0);

            context.OMSetRenderTargets(None, None);
            context.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(Some(output))
    }

    fn pool_dropped(&self) -> u64 {
        self.pool_dropped
    }
}
