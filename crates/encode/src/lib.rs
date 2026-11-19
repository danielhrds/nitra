//! Hardware H.264 encoders behind one contract ([`VideoEncoder`]), plus
//! read-only capability probes and minimal Annex B inspection.

pub mod amf;
pub mod annexb;
mod contract;
pub mod media_foundation;
pub mod probe;
pub mod select;

pub use contract::{EncodeError, EncodedChunk, EncoderConfig, Submit, VideoEncoder};
pub use select::{EncoderKind, EncoderPreference, EncoderSelection, create_encoder};
