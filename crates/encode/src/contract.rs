//! The encoder contract shared by every backend (Media Foundation, AMF).

use std::fmt;

use gpu::GpuFrame;
use telemetry::QpcTimestamp;

/// Encoder settings. Low latency: no B-frames, one frame in, one access
/// unit out, constant bitrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Frames between automatic IDR frames; 0 leaves it to the encoder and
    /// relies on `request_keyframe`.
    pub keyframe_interval: u32,
    /// Input frames the encoder may hold at once; more are refused (`Busy`).
    pub max_in_flight: usize,
}

impl EncoderConfig {
    pub fn validate(&self) -> Result<(), EncodeError> {
        let even = |value: u32| value > 0 && value.is_multiple_of(2);
        if !even(self.width) || !even(self.height) {
            return Err(EncodeError::InvalidConfig(format!(
                "frame size must be even and non-zero, got {}x{}",
                self.width, self.height
            )));
        }
        if self.fps == 0 || self.bitrate_bps == 0 || self.max_in_flight == 0 {
            return Err(EncodeError::InvalidConfig(format!("{self:?}")));
        }
        Ok(())
    }
}

/// One encoded access unit (Annex B byte stream: compressed data, not pixels).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedChunk {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub frame_id: u64,
    /// The source frame's timestamp (see `GpuFrame::captured_at`).
    pub captured_at: QpcTimestamp,
    pub submitted_at: QpcTimestamp,
    /// When the encoder handed the access unit back.
    pub output_at: QpcTimestamp,
}

/// Result of offering a frame to the encoder.
#[derive(Debug)]
pub enum Submit {
    Accepted,
    /// The encoder cannot take a frame now; it is handed back unchanged.
    Busy(GpuFrame),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    InvalidConfig(String),
    NotFound(String),
    Unsupported(String),
    Windows(String),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::InvalidConfig(message) => write!(f, "invalid encoder config: {message}"),
            EncodeError::NotFound(message) => write!(f, "encoder not found: {message}"),
            EncodeError::Unsupported(message) => write!(f, "unsupported: {message}"),
            EncodeError::Windows(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for EncodeError {}

impl From<windows::core::Error> for EncodeError {
    fn from(error: windows::core::Error) -> Self {
        EncodeError::Windows(error.to_string())
    }
}

/// A hardware H.264 encoder taking NV12 `GpuFrame`s.
///
/// `submit` and `poll` never block on the encoder. Frames stay leased
/// (their textures reserved) until their output comes back.
pub trait VideoEncoder {
    /// Short name for reports, e.g. `media-foundation`.
    fn name(&self) -> &'static str;

    fn config(&self) -> EncoderConfig;

    fn submit(&mut self, frame: GpuFrame) -> Result<Submit, EncodeError>;

    /// The next finished access unit, if any.
    fn poll(&mut self) -> Result<Option<EncodedChunk>, EncodeError>;

    /// Makes the next submitted frame an IDR frame.
    fn request_keyframe(&mut self);

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), EncodeError>;

    /// Finishes every frame in flight and returns their access units.
    fn drain(&mut self) -> Result<Vec<EncodedChunk>, EncodeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EncoderConfig {
        EncoderConfig {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 10_000_000,
            keyframe_interval: 0,
            max_in_flight: 3,
        }
    }

    #[test]
    fn config_validation() {
        assert!(config().validate().is_ok());
        assert!(
            EncoderConfig {
                width: 1919,
                ..config()
            }
            .validate()
            .is_err()
        );
        assert!(EncoderConfig { fps: 0, ..config() }.validate().is_err());
        assert!(
            EncoderConfig {
                bitrate_bps: 0,
                ..config()
            }
            .validate()
            .is_err()
        );
        assert!(
            EncoderConfig {
                max_in_flight: 0,
                ..config()
            }
            .validate()
            .is_err()
        );
    }
}
