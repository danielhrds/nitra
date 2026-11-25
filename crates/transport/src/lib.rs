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

/// A connection's quality, from str0m's peer statistics (every
/// [`STATS_INTERVAL`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinkStats {
    /// Share of packets lost on the way (0..1) over the last interval,
    /// before retransmissions recover them: the network's loss. For a viewer
    /// what it receives; for a host what one viewer reported. None until known.
    pub loss: Option<f32>,
    /// Round trip time, when measured.
    pub rtt: Option<std::time::Duration>,
    /// Media bytes received (viewer) or sent (host) so far.
    pub media_bytes: u64,
    /// Which way the media goes, once datagrams flow.
    pub route: Option<Route>,
}

/// Which way a connection goes, judged from the other side's address (str0m
/// reports the addresses of the chosen pair, not the candidate types).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// A private, link-local or loopback address: the same network (or PC).
    Lan,
    /// A public IPv4 address: straight across the internet, through NAT.
    Internet,
    /// A public IPv6 address: across the internet without NAT.
    Ipv6,
}

impl Route {
    pub fn of(remote: std::net::SocketAddr) -> Self {
        use std::net::IpAddr;
        let ip = match remote.ip() {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 => v4,
        };
        match ip {
            IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() || v4.is_loopback() => {
                Self::Lan
            }
            IpAddr::V4(_) => Self::Internet,
            // Unique local (fc00::/7) and link-local (fe80::/10).
            IpAddr::V6(v6)
                if v6.is_loopback()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 =>
            {
                Self::Lan
            }
            IpAddr::V6(_) => Self::Ipv6,
        }
    }
}

/// How often the link statistics refresh.
pub const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// H.264 video and Opus audio; peer statistics every [`STATS_INTERVAL`].
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
        // Video waits for retransmissions over this many packets before it
        // gives up on a frame with a hole (a keyframe request and a freeze).
        // str0m's 30 is less than two 1080p frames at 10 Mbps.
        .set_reordering_size_video(VIDEO_REORDER_PACKETS)
        .set_stats_interval(Some(STATS_INTERVAL))
}

/// Payload types of the one H.264 format (and its retransmissions).
const H264_PT: u8 = 114;
const H264_RTX_PT: u8 = 115;

/// See [`rtc_config`].
const AUDIO_REORDER_WAIT: std::time::Duration = std::time::Duration::from_millis(40);
/// See [`rtc_config`]: ~1 s of 1080p60 at 10 Mbps, as many as the NACK window
/// in `vendor/str0m` tracks (with 30, freezes at 2-5 % loss stayed, measured).
const VIDEO_REORDER_PACKETS: usize = 1000;

#[cfg(test)]
mod tests {
    use super::Route;

    fn route(addr: &str) -> Route {
        Route::of(addr.parse().unwrap())
    }

    #[test]
    fn private_addresses_are_the_local_network() {
        for addr in [
            "192.168.1.20:5000",
            "10.0.0.2:5000",
            "172.20.0.5:5000",
            "169.254.3.4:5000",
            "127.0.0.1:5000",
            "[fe80::1]:5000",
            "[fd12:3456::1]:5000",
            "[::ffff:192.168.0.9]:5000",
        ] {
            assert_eq!(route(addr), Route::Lan, "{addr}");
        }
    }

    #[test]
    fn public_addresses_cross_the_internet() {
        assert_eq!(route("203.0.113.7:40000"), Route::Internet);
        assert_eq!(route("172.32.0.1:40000"), Route::Internet);
        assert_eq!(route("[::ffff:203.0.113.7]:40000"), Route::Internet);
        assert_eq!(route("[2001:db8::7]:40000"), Route::Ipv6);
    }
}
