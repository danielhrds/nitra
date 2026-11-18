//! Visual frame stamp: a frame id and a QPC timestamp drawn into the image
//! as black and white blocks, so latency can be measured on the receiving
//! side from the decoded picture itself.
//!
//! Layout: 128 bits in 2 rows of 64 blocks of 16x16 px at the top-left
//! corner (1024x32 px). Blocks match H.264 macroblocks, which helps them
//! survive compression. The bits are `[frame_id, timestamp low, timestamp
//! high, crc32]` as little-endian u32 words; bit `i` is bit `i % 32` of word
//! `i / 32` and sits at column `i % 64`, row `i / 64`. White means 1.

use std::fmt;

use crate::clock::QpcTimestamp;

pub const BLOCK_SIZE: u32 = 16;
pub const BLOCKS_PER_ROW: u32 = 64;
pub const BLOCK_ROWS: u32 = 2;
pub const CODE_BITS: u32 = BLOCKS_PER_ROW * BLOCK_ROWS;
/// Size of the code region, anchored at pixel (0, 0).
pub const CODE_WIDTH: u32 = BLOCK_SIZE * BLOCKS_PER_ROW;
pub const CODE_HEIGHT: u32 = BLOCK_SIZE * BLOCK_ROWS;

/// Luma at or above this value reads as a 1 bit.
const LUMA_THRESHOLD: u32 = 128;
/// Only the central part of each block is sampled, away from block edges
/// where compression artifacts concentrate.
const SAMPLE_INSET: u32 = 4;

/// What one frame carries in its code region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStamp {
    pub frame_id: u32,
    /// When the frame was submitted, on the QPC timeline.
    pub timestamp: QpcTimestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameCodeError {
    /// The frame cannot hold the code region.
    FrameTooSmall { width: u32, height: u32 },
    /// The bits were read but do not match their checksum.
    ChecksumMismatch { stored: u32, computed: u32 },
}

impl fmt::Display for FrameCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameCodeError::FrameTooSmall { width, height } => write!(
                f,
                "frame {width}x{height} is smaller than the {CODE_WIDTH}x{CODE_HEIGHT} frame code"
            ),
            FrameCodeError::ChecksumMismatch { stored, computed } => write!(
                f,
                "frame code checksum mismatch (stored {stored:#010x}, computed {computed:#010x})"
            ),
        }
    }
}

impl std::error::Error for FrameCodeError {}

/// Checks that a `width` x `height` frame can hold the code region.
pub fn check_frame_size(width: u32, height: u32) -> Result<(), FrameCodeError> {
    if width < CODE_WIDTH || height < CODE_HEIGHT {
        Err(FrameCodeError::FrameTooSmall { width, height })
    } else {
        Ok(())
    }
}

impl FrameStamp {
    /// The 128 code bits as four words: frame id, timestamp low/high, CRC32.
    pub fn to_words(self) -> [u32; 4] {
        let timestamp = self.timestamp.ticks() as u64;
        let low = timestamp as u32;
        let high = (timestamp >> 32) as u32;
        [
            self.frame_id,
            low,
            high,
            payload_crc(self.frame_id, low, high),
        ]
    }

    /// Rebuilds a stamp from its words, verifying the checksum.
    pub fn from_words(words: [u32; 4]) -> Result<Self, FrameCodeError> {
        let [frame_id, low, high, stored] = words;
        let computed = payload_crc(frame_id, low, high);
        if stored != computed {
            return Err(FrameCodeError::ChecksumMismatch { stored, computed });
        }
        let timestamp = (u64::from(high) << 32) | u64::from(low);
        Ok(Self {
            frame_id,
            timestamp: QpcTimestamp::from_ticks(timestamp as i64),
        })
    }
}

/// Top-left pixel of the block that holds `bit`.
pub fn block_origin(bit: u32) -> (u32, u32) {
    debug_assert!(bit < CODE_BITS);
    (
        (bit % BLOCKS_PER_ROW) * BLOCK_SIZE,
        (bit / BLOCKS_PER_ROW) * BLOCK_SIZE,
    )
}

/// Value (0 or 1) of `bit` in `words`.
pub fn bit_value(words: &[u32; 4], bit: u32) -> u32 {
    (words[(bit / 32) as usize] >> (bit % 32)) & 1
}

/// Reads a stamp from an image, given the luma (0-255) of pixels inside the
/// code region. `luma(x, y)` is only called with `x < CODE_WIDTH` and
/// `y < CODE_HEIGHT`.
pub fn read_frame_stamp(luma: impl Fn(u32, u32) -> u8) -> Result<FrameStamp, FrameCodeError> {
    let mut words = [0_u32; 4];
    for bit in 0..CODE_BITS {
        let (left, top) = block_origin(bit);
        let samples = BLOCK_SIZE - 2 * SAMPLE_INSET;
        let mut total = 0_u32;
        for y in top + SAMPLE_INSET..top + BLOCK_SIZE - SAMPLE_INSET {
            for x in left + SAMPLE_INSET..left + BLOCK_SIZE - SAMPLE_INSET {
                total += u32::from(luma(x, y));
            }
        }
        if total >= LUMA_THRESHOLD * samples * samples {
            words[(bit / 32) as usize] |= 1 << (bit % 32);
        }
    }
    FrameStamp::from_words(words)
}

fn payload_crc(frame_id: u32, timestamp_low: u32, timestamp_high: u32) -> u32 {
    let mut bytes = [0_u8; 12];
    bytes[0..4].copy_from_slice(&frame_id.to_le_bytes());
    bytes[4..8].copy_from_slice(&timestamp_low.to_le_bytes());
    bytes[8..12].copy_from_slice(&timestamp_high.to_le_bytes());
    crc32(&bytes)
}

/// CRC-32 (IEEE 802.3, reflected, polynomial 0xEDB88320).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(frame_id: u32, ticks: i64) -> FrameStamp {
        FrameStamp {
            frame_id,
            timestamp: QpcTimestamp::from_ticks(ticks),
        }
    }

    /// Renders the code region on the CPU, the same way the shader does.
    fn render(words: [u32; 4]) -> Vec<u8> {
        let mut image = vec![0_u8; (CODE_WIDTH * CODE_HEIGHT) as usize];
        for bit in 0..CODE_BITS {
            let (left, top) = block_origin(bit);
            let value = if bit_value(&words, bit) == 1 { 255 } else { 0 };
            for y in top..top + BLOCK_SIZE {
                for x in left..left + BLOCK_SIZE {
                    image[(y * CODE_WIDTH + x) as usize] = value;
                }
            }
        }
        image
    }

    fn read(image: &[u8]) -> Result<FrameStamp, FrameCodeError> {
        read_frame_stamp(|x, y| image[(y * CODE_WIDTH + x) as usize])
    }

    #[test]
    fn crc32_matches_reference_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn words_round_trip() {
        for original in [
            stamp(0, 0),
            stamp(1, 123_456_789),
            stamp(u32::MAX, i64::MAX),
            stamp(42, -1),
        ] {
            assert_eq!(FrameStamp::from_words(original.to_words()), Ok(original));
        }
    }

    #[test]
    fn any_flipped_bit_is_detected() {
        let words = stamp(7, 9_876_543_210).to_words();
        for bit in 0..CODE_BITS {
            let mut corrupted = words;
            corrupted[(bit / 32) as usize] ^= 1 << (bit % 32);
            assert!(
                matches!(
                    FrameStamp::from_words(corrupted),
                    Err(FrameCodeError::ChecksumMismatch { .. })
                ),
                "bit {bit}"
            );
        }
    }

    #[test]
    fn image_round_trip() {
        let original = stamp(1234, 55_555_555_555);
        assert_eq!(read(&render(original.to_words())), Ok(original));
    }

    #[test]
    fn reading_tolerates_luma_noise() {
        let original = stamp(99, 31_415_926_535);
        let mut image = render(original.to_words());
        // Deterministic +-40 noise, as a lossy encoder might add.
        let mut state = 0x1234_5678_u32;
        for pixel in &mut image {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state >> 24) as i32 % 81 - 40;
            *pixel = (i32::from(*pixel) + noise).clamp(0, 255) as u8;
        }
        assert_eq!(read(&image), Ok(original));
    }

    #[test]
    fn blurred_block_edges_do_not_matter() {
        let original = stamp(5, 1_000);
        let mut image = render(original.to_words());
        // Smear the outer 4 px of every block to mid-gray.
        for y in 0..CODE_HEIGHT {
            for x in 0..CODE_WIDTH {
                let (bx, by) = (x % BLOCK_SIZE, y % BLOCK_SIZE);
                let near_edge = bx < SAMPLE_INSET
                    || by < SAMPLE_INSET
                    || bx >= BLOCK_SIZE - SAMPLE_INSET
                    || by >= BLOCK_SIZE - SAMPLE_INSET;
                if near_edge {
                    image[(y * CODE_WIDTH + x) as usize] = 128;
                }
            }
        }
        assert_eq!(read(&image), Ok(original));
    }

    #[test]
    fn layout_places_bits_row_major() {
        assert_eq!(block_origin(0), (0, 0));
        assert_eq!(block_origin(63), (1008, 0));
        assert_eq!(block_origin(64), (0, 16));
        assert_eq!(block_origin(127), (1008, 16));
        assert_eq!((CODE_WIDTH, CODE_HEIGHT), (1024, 32));
    }

    #[test]
    fn small_frames_are_rejected() {
        assert_eq!(check_frame_size(1920, 1080), Ok(()));
        assert_eq!(check_frame_size(1024, 32), Ok(()));
        assert_eq!(
            check_frame_size(800, 600),
            Err(FrameCodeError::FrameTooSmall {
                width: 800,
                height: 600
            })
        );
    }
}
