//! Raw NV12 file source for `bench encode --source raw` (encoder quality
//! comparisons on recorded game footage).
//!
//! Bench-only: each frame is read on the CPU and uploaded through a staging
//! texture so every encoder gets bit-identical input that can be compared
//! with the same file afterwards. The media path never does this.

use std::error::Error;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use gpu::{D3d11Device, Lease, TexturePool};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

/// Bytes of one tightly packed NV12 frame (Y plane, then interleaved CbCr
/// at half resolution), as `ffmpeg -pix_fmt nv12 -f rawvideo` writes it.
pub fn nv12_frame_bytes(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

pub struct RawSource {
    reader: BufReader<File>,
    width: u32,
    height: u32,
    frames: u64,
    buffer: Vec<u8>,
    staging: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    pool: TexturePool,
}

impl RawSource {
    pub fn open(
        device: &D3d11Device,
        path: &Path,
        width: u32,
        height: u32,
    ) -> Result<Self, Box<dyn Error>> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(format!("NV12 frames must have even sizes, got {width}x{height}").into());
        }
        let frame_bytes = nv12_frame_bytes(width, height);
        let file = File::open(path)?;
        let length = file.metadata()?.len();
        if length == 0 || !length.is_multiple_of(frame_bytes as u64) {
            return Err(format!(
                "{} is {length} bytes, not a whole number of {width}x{height} NV12 frames ({frame_bytes} bytes each)",
                path.display()
            )
            .into());
        }
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
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        // SAFETY: valid device; descriptor and out-pointer reference live locals.
        unsafe {
            device
                .raw()
                .CreateTexture2D(&desc, None, Some(&mut staging))
        }?;
        Ok(Self {
            reader: BufReader::with_capacity(frame_bytes * 2, file),
            width,
            height,
            frames: length / frame_bytes as u64,
            buffer: vec![0; frame_bytes],
            staging: staging.expect("CreateTexture2D succeeded"),
            context: device.immediate_context()?,
            // Enough for the encoder's frames in flight plus one being filled.
            pool: gpu::create_nv12_pool(device, width, height, 8)?,
        })
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The next frame on the GPU, or `None` at the end of the file. Fails if
    /// every pool texture is still held by the encoder.
    pub fn next(&mut self) -> Result<Option<Lease<ID3D11Texture2D>>, Box<dyn Error>> {
        match self.reader.read_exact(&mut self.buffer) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let texture = self
            .pool
            .try_acquire()
            .ok_or("raw source: no free NV12 texture")?;
        let (width, height) = (self.width as usize, self.height as usize);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: valid staging texture of this context's device, mapped for
        // writing only while the rows are copied. For NV12, the mapped
        // memory holds `height` luma rows then `height / 2` CbCr rows, all
        // `RowPitch` bytes apart (D3D11 planar layout); each copied row is
        // `width` bytes, which is at most `RowPitch`.
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData.cast::<u8>();
            for row in 0..height + height / 2 {
                let source = &self.buffer[row * width..(row + 1) * width];
                std::ptr::copy_nonoverlapping(source.as_ptr(), base.add(row * pitch), width);
            }
            self.context.Unmap(&self.staging, 0);
            self.context.CopyResource(&*texture, &self.staging);
        }
        Ok(Some(texture))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_frame_size() {
        assert_eq!(nv12_frame_bytes(1920, 1080), 3_110_400);
        assert_eq!(nv12_frame_bytes(2, 2), 6);
    }
}
