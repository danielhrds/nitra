//! BGRA8 to NV12 conversion with optional scaling, on the GPU.
//!
//! Two implementations of [`Converter`] produce the same output so they can
//! be compared (decision D3): the D3D11 video processor
//! ([`VideoProcessorConverter`]) and our own pixel shaders
//! ([`ShaderConverter`]). Both write BT.709 limited-range YCbCr
//! ([`bt709_limited`] is the CPU reference) into textures from a fixed NV12
//! pool; when every output texture is still held, the frame is dropped and
//! counted instead of allocating.

mod color;
mod preview;
mod readback;
mod shader_converter;
mod video_processor;

use std::collections::HashMap;
use std::fmt;

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::core::Interface;

use crate::pool::Lease;

pub use color::bt709_limited;
pub use preview::{PreviewReader, PreviewWriter, SharedHandle};
pub use readback::{
    MAX_IMAGE_PIXELS, Nv12Image, read_nv12_image, read_nv12_luma, read_nv12_luma_region,
    read_nv12_samples,
};
pub use shader_converter::ShaderConverter;
pub use video_processor::VideoProcessorConverter;

/// Input and output sizes of a conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConvertConfig {
    pub input_width: u32,
    pub input_height: u32,
    /// Both output dimensions must be even (NV12 chroma is half size).
    pub output_width: u32,
    pub output_height: u32,
    /// NV12 textures in the output pool.
    pub pool_size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConvertError {
    InvalidSize(String),
    Unsupported(String),
    Windows(String),
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvertError::InvalidSize(message) => write!(f, "invalid conversion size: {message}"),
            ConvertError::Unsupported(message) => write!(f, "unsupported: {message}"),
            ConvertError::Windows(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ConvertError {}

impl From<windows::core::Error> for ConvertError {
    fn from(error: windows::core::Error) -> Self {
        ConvertError::Windows(error.to_string())
    }
}

impl ConvertConfig {
    /// Checks the sizes: non-zero, output even.
    pub fn validate(&self) -> Result<(), ConvertError> {
        let sizes = [
            self.input_width,
            self.input_height,
            self.output_width,
            self.output_height,
        ];
        if sizes.contains(&0) || self.pool_size == 0 {
            return Err(ConvertError::InvalidSize(format!("{self:?}")));
        }
        if !self.output_width.is_multiple_of(2) || !self.output_height.is_multiple_of(2) {
            return Err(ConvertError::InvalidSize(format!(
                "NV12 output must have even dimensions, got {}x{}",
                self.output_width, self.output_height
            )));
        }
        Ok(())
    }
}

/// Converts BGRA8 textures (the top-left `input_width` x `input_height`
/// region) into pooled NV12 textures of the output size.
pub trait Converter {
    /// Short name for reports, e.g. `video-processor`.
    fn method(&self) -> &'static str;

    fn config(&self) -> ConvertConfig;

    /// Records the conversion of `source` on the immediate context and
    /// returns the output texture, or `None` (counted) if every output
    /// texture is still in use. The GPU work runs asynchronously; later
    /// commands on the same context see the result.
    fn convert(
        &mut self,
        source: &ID3D11Texture2D,
    ) -> Result<Option<Lease<ID3D11Texture2D>>, ConvertError>;

    /// Frames dropped because the output pool was exhausted.
    fn pool_dropped(&self) -> u64;
}

/// Per-texture view cache. Views keep their texture alive, so a cached
/// address cannot be reused by another texture; the cache is cleared when it
/// grows past `limit` (e.g. after capture textures were recreated).
pub(crate) struct ViewCache<V> {
    views: HashMap<usize, V>,
    limit: usize,
}

impl<V: Clone> ViewCache<V> {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            views: HashMap::new(),
            limit,
        }
    }

    pub(crate) fn get_or_create<E>(
        &mut self,
        texture: &ID3D11Texture2D,
        create: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E> {
        let key = texture.as_raw() as usize;
        if let Some(view) = self.views.get(&key) {
            return Ok(view.clone());
        }
        if self.views.len() >= self.limit {
            self.views.clear();
        }
        let view = create()?;
        self.views.insert(key, view.clone());
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(output_width: u32, output_height: u32) -> ConvertConfig {
        ConvertConfig {
            input_width: 1920,
            input_height: 1080,
            output_width,
            output_height,
            pool_size: 3,
        }
    }

    #[test]
    fn output_must_be_even_and_non_zero() {
        assert!(config(1280, 720).validate().is_ok());
        assert!(config(1281, 720).validate().is_err());
        assert!(config(1280, 721).validate().is_err());
        assert!(config(0, 720).validate().is_err());
        let no_pool = ConvertConfig {
            pool_size: 0,
            ..config(1280, 720)
        };
        assert!(no_pool.validate().is_err());
    }
}
