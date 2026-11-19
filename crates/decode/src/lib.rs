//! Hardware video decoding behind one contract ([`VideoDecoder`]): Media
//! Foundation's H.264 decoder with DXVA on a D3D11 device (NV12 textures
//! out, no CPU copies), plus read-only capability probes.

mod contract;
pub mod media_foundation;
pub mod probe;

pub use contract::{DecodeError, DecodeSubmit, DecodedFrame, VideoDecoder};
