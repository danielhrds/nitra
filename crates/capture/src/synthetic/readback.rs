//! GPU-to-CPU copies for tests and diagnostics only. Every function here
//! stalls until the GPU finishes, so none of them belongs in the media path.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use gpu::D3d11Device;
use telemetry::frame_code::{self, FrameStamp};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11DeviceContext, ID3D11Texture2D,
};

use super::Result;

/// A mapped staging texture; unmapped on drop.
struct Mapped<'a> {
    context: &'a ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    data: D3D11_MAPPED_SUBRESOURCE,
    width: u32,
    height: u32,
}

impl Mapped<'_> {
    /// BGRA bytes of row `y`.
    fn row(&self, y: u32) -> &[u8] {
        assert!(y < self.height);
        // SAFETY: while mapped, pData points to `height` rows of `RowPitch`
        // bytes each, and every row holds at least `width * 4` bytes of
        // BGRA8 pixels.
        unsafe {
            std::slice::from_raw_parts(
                self.data
                    .pData
                    .cast::<u8>()
                    .add(y as usize * self.data.RowPitch as usize),
                self.width as usize * 4,
            )
        }
    }
}

impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        // SAFETY: the texture was mapped by `map_region` on this context.
        unsafe { self.context.Unmap(&self.texture, 0) };
    }
}

/// Copies the `width` x `height` region at (0, 0) of `source` (BGRA8) into a
/// new staging texture and maps it for reading.
fn map_region<'a>(
    device: &D3d11Device,
    context: &'a ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<Mapped<'a>> {
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture; the out-pointer references a live local.
    unsafe { source.GetDesc(&mut source_desc) };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
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

    let region = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: width,
        bottom: height,
        back: 1,
    };
    let mut data = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: both textures share format and device; the box lies inside the
    // source (callers pass at most its size). Map blocks until the copy is done.
    unsafe {
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, source, 0, Some(&region));
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut data))?;
    }
    Ok(Mapped {
        context,
        texture: staging,
        data,
        width,
        height,
    })
}

pub(super) fn read_stamp(
    device: &D3d11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> Result<FrameStamp> {
    let mapped = map_region(
        device,
        context,
        texture,
        frame_code::CODE_WIDTH,
        frame_code::CODE_HEIGHT,
    )?;
    let stamp = frame_code::read_frame_stamp(|x, y| {
        let pixel = &mapped.row(y)[x as usize * 4..x as usize * 4 + 3];
        // B, G, R averaged: the stamp is pure black or white.
        ((u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2])) / 3) as u8
    })?;
    Ok(stamp)
}

pub(super) fn dump_bmp(
    device: &D3d11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    path: &Path,
) -> Result<()> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture; the out-pointer references a live local.
    unsafe { texture.GetDesc(&mut desc) };
    let mapped = map_region(device, context, texture, desc.Width, desc.Height)?;

    let mut file = BufWriter::new(File::create_new(path)?);
    file.write_all(&bmp_header(desc.Width, desc.Height))?;
    for y in 0..desc.Height {
        file.write_all(mapped.row(y))?;
    }
    file.flush()?;
    Ok(())
}

/// Header of an uncompressed, top-down, 32-bit BGRA BMP.
fn bmp_header(width: u32, height: u32) -> [u8; 54] {
    let pixel_bytes = width * height * 4;
    let mut header = [0_u8; 54];
    header[0..2].copy_from_slice(b"BM");
    header[2..6].copy_from_slice(&(54 + pixel_bytes).to_le_bytes());
    header[10..14].copy_from_slice(&54_u32.to_le_bytes());
    header[14..18].copy_from_slice(&40_u32.to_le_bytes());
    header[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    // Negative height: rows are stored top to bottom.
    header[22..26].copy_from_slice(&(-(height as i32)).to_le_bytes());
    header[26..28].copy_from_slice(&1_u16.to_le_bytes());
    header[28..30].copy_from_slice(&32_u16.to_le_bytes());
    header[34..38].copy_from_slice(&pixel_bytes.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmp_header_fields() {
        let header = bmp_header(1920, 1080);
        assert_eq!(&header[0..2], b"BM");
        let read_u32 =
            |offset: usize| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
        let read_i32 =
            |offset: usize| i32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
        assert_eq!(read_u32(2), 54 + 1920 * 1080 * 4);
        assert_eq!(read_u32(10), 54);
        assert_eq!(read_i32(18), 1920);
        assert_eq!(read_i32(22), -1080);
        assert_eq!(u16::from_le_bytes([header[28], header[29]]), 32);
    }
}
