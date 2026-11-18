//! NV12 reads for tests and diagnostics only. They copy a whole frame to a
//! staging texture and wait for the GPU; never use them in the media path.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::core::Result;

use crate::device::D3d11Device;

/// A mapped NV12 subresource: `height` Y rows followed by `height / 2`
/// interleaved CbCr rows, `pitch` bytes apart, each at least `width` bytes.
struct MappedNv12 {
    base: *const u8,
    pitch: usize,
    width: u32,
    height: u32,
}

impl MappedNv12 {
    fn y_row(&self, y: u32) -> &[u8] {
        assert!(y < self.height);
        // SAFETY: the subresource stays mapped while `self` is borrowed
        // (see `with_mapped`), and row `y` lies inside the Y plane.
        unsafe {
            std::slice::from_raw_parts(self.base.add(y as usize * self.pitch), self.width as usize)
        }
    }

    fn uv_row(&self, y: u32) -> &[u8] {
        assert!(y < self.height / 2);
        // SAFETY: as above; chroma rows follow the `height` luma rows.
        unsafe {
            std::slice::from_raw_parts(
                self.base.add((self.height + y) as usize * self.pitch),
                self.width as usize,
            )
        }
    }
}

/// Maps a staging copy of an NV12 texture and runs `read` on it.
fn with_mapped<R>(
    device: &D3d11Device,
    texture: &ID3D11Texture2D,
    read: impl FnOnce(&MappedNv12) -> R,
) -> Result<R> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: valid texture; the out-pointer references a live local.
    unsafe { texture.GetDesc(&mut desc) };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
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
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: same size and format; Map waits for the copy to finish.
    unsafe {
        context.CopyResource(&staging, texture);
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    let view = MappedNv12 {
        base: mapped.pData.cast::<u8>(),
        pitch: mapped.RowPitch as usize,
        width: desc.Width,
        height: desc.Height,
    };
    let result = read(&view);
    // SAFETY: mapped above on this context; `view` is no longer used.
    unsafe { context.Unmap(&staging, 0) };
    Ok(result)
}

/// `[Y, Cb, Cr]` at each luma position `(x, y)` (chroma from `(x/2, y/2)`).
pub fn read_nv12_samples(
    device: &D3d11Device,
    texture: &ID3D11Texture2D,
    points: &[(u32, u32)],
) -> Result<Vec<[u8; 3]>> {
    with_mapped(device, texture, |mapped| {
        points
            .iter()
            .map(|&(x, y)| {
                let chroma = mapped.uv_row(y / 2);
                let cx = (x / 2 * 2) as usize;
                [mapped.y_row(y)[x as usize], chroma[cx], chroma[cx + 1]]
            })
            .collect()
    })
}

/// The luma of a `width` x `height` region at the top-left corner, row by
/// row (small regions only, e.g. a frame stamp).
pub fn read_nv12_luma(
    device: &D3d11Device,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    with_mapped(device, texture, |mapped| {
        assert!(width <= mapped.width && height <= mapped.height);
        (0..height)
            .flat_map(|y| mapped.y_row(y)[..width as usize].to_vec())
            .collect()
    })
}
