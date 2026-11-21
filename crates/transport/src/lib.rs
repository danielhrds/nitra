//! Media transport over WebRTC with str0m: a host session that
//! sends one encoded stream to several viewers, and a viewer session that
//! receives it as complete frames. Sans-IO: the sessions own a UDP socket
//! with a precise wait ([`net`]) and are driven by the caller's thread.
//!
//! Signaling (offer/answer) is not here: the caller moves the SDP strings.

pub mod budget;
pub mod gather;
mod host;
pub mod impair;
pub mod interfaces;
pub mod net;
pub mod stun;
mod viewer;

use std::fmt;

pub use host::{HostConfig, HostEvent, HostSession, PeerId, PeerStats};
pub use viewer::{AudioPacket, Frame, ViewerEvent, ViewerSession};

/// RTP media time units per second for video.
pub const VIDEO_CLOCK_HZ: u64 = 90_000;
/// RTP media time units per second for audio (Opus).
pub const AUDIO_CLOCK_HZ: u64 = 48_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportError(pub String);

impl TransportError {
    fn from_display(error: impl fmt::Display) -> Self {
        Self(error.to_string())
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportError {}

impl From<std::io::Error> for TransportError {
    fn from(error: std::io::Error) -> Self {
        Self::from_display(error)
    }
}

impl From<str0m::RtcError> for TransportError {
    fn from(error: str0m::RtcError) -> Self {
        Self::from_display(error)
    }
}

/// H.264 video and Opus audio; statistics off (the sessions report what they need).
fn rtc_config() -> str0m::RtcConfig {
    let mut config = str0m::Rtc::builder().clear_codecs();
    // Only what our encoders send: H.264 High (level 5.2 covers up to 4K60),
    // packetization mode 1, with retransmissions. str0m's default offers
    // seven H.264 variants, which made every offer ~2.5 KB longer (too long
    // for a manual invite).
    config
        .codec_config()
        .add_h264(H264_PT.into(), Some(H264_RTX_PT.into()), true, 0x64_00_34);
    config
        .enable_opus(true, false)
        // Audio waits at most this long for a missing earlier packet (a
        // retransmission), then moves on: Opus conceals the loss, and a
        // packet later than the playback queue (60 ms) is useless anyway.
        // str0m's default (1 s) delayed audio by up to ~300 ms at 5 % loss.
        .set_reordering_timeout_audio(Some(AUDIO_REORDER_WAIT))
}

/// Payload types of the one H.264 format (and its retransmissions).
const H264_PT: u8 = 114;
const H264_RTX_PT: u8 = 115;

/// See [`rtc_config`].
const AUDIO_REORDER_WAIT: std::time::Duration = std::time::Duration::from_millis(40);
