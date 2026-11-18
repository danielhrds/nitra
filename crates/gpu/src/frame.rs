//! Frames that live on the GPU: pooled D3D11 textures with metadata.

use telemetry::QpcTimestamp;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Result;

use crate::device::D3d11Device;
use crate::pool::{Lease, Pool};

/// A pooled BGRA8 texture.
pub type TexturePool = Pool<ID3D11Texture2D>;

/// A frame on the GPU. Dropping it returns the texture to its pool.
#[derive(Debug)]
pub struct GpuFrame {
    pub texture: Lease<ID3D11Texture2D>,
    /// Size of the image inside the texture (top-left aligned).
    pub width: u32,
    pub height: u32,
    /// Increasing per source in delivery order.
    pub frame_id: u64,
    /// The source's timestamp for the frame, on the QPC timeline. For WGC
    /// this is `SystemRelativeTime`, which can be a few ms in the future
    /// (DWM's target display time); compare sources only with care.
    pub captured_at: QpcTimestamp,
}

/// Creates `count` BGRA8 textures of `width` x `height` usable as copy
/// destinations, render targets and shader inputs.
pub fn create_texture_pool(
    device: &D3d11Device,
    width: u32,
    height: u32,
    count: usize,
) -> Result<TexturePool> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let textures = (0..count)
        .map(|_| {
            let mut texture = None;
            // SAFETY: valid device; descriptor and out-pointer reference live locals.
            unsafe {
                device
                    .raw()
                    .CreateTexture2D(&desc, None, Some(&mut texture))
            }?;
            Ok(texture.expect("CreateTexture2D succeeded"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Pool::new(textures))
}

/// Test/diagnostics only: reads one BGRA8 pixel back to the CPU. Stalls
/// until the GPU is done; never use it in the media path.
pub fn read_pixel(
    device: &D3d11Device,
    texture: &ID3D11Texture2D,
    x: u32,
    y: u32,
) -> Result<[u8; 4]> {
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture; the out-pointer references a live local.
    unsafe { texture.GetDesc(&mut source_desc) };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: 1,
        Height: 1,
        MipLevels: 1,
        ArraySize: 1,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..source_desc
    };
    let mut staging = None;
    // SAFETY: valid device; descriptor and out-pointer reference live locals.
    unsafe {
        device
            .raw()
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
    }?;
    let staging: ID3D11Texture2D = staging.expect("CreateTexture2D succeeded");

    let context = device.immediate_context()?;
    let region = D3D11_BOX {
        left: x,
        top: y,
        front: 0,
        right: x + 1,
        bottom: y + 1,
        back: 1,
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: same device and format; callers pass a pixel inside the
    // texture. Map blocks until the copy has completed; the 4 bytes read are
    // the single BGRA8 pixel of the mapped 1x1 staging texture.
    unsafe {
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, 0, Some(&region));
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pixel = std::ptr::read(mapped.pData.cast::<[u8; 4]>());
        context.Unmap(&staging, 0);
        Ok(pixel)
    }
}
