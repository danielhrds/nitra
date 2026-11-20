//! Shared helpers for the transport tests. The access units are synthetic
//! NAL units (str0m only parses NAL headers), with a large "keyframe" every
//! 30 frames.

#![allow(dead_code)] // each test file uses a different subset

use transport::{HostConfig, HostSession};

pub const FPS: u64 = 60;

pub fn localhost() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

pub fn is_keyframe(index: u64) -> bool {
    index.is_multiple_of(30)
}

/// Annex B access unit `index`: an IDR slice (~120 KB) every 30 frames,
/// otherwise a small slice (~7 Mbps in all at 60 fps); bytes avoid
/// start-code patterns.
pub fn access_unit(index: u64) -> Vec<u8> {
    let (nal_type, size) = if is_keyframe(index) {
        (5_u8, 120_000)
    } else {
        (1_u8, 8_000 + (index as usize % 7) * 900)
    };
    let mut unit = vec![0, 0, 0, 1, 0x60 | nal_type];
    unit.extend((0..size).map(|i| 0x10 + ((i as u64 * 31 + index) % 0xE0) as u8));
    unit
}

pub fn host() -> HostSession {
    HostSession::bind(
        localhost(),
        HostConfig {
            start_bitrate_bps: 12_000_000,
            max_bitrate_bps: 20_000_000,
        },
    )
    .unwrap()
}
