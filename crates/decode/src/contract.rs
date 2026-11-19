//! The decoder contract shared by decoder backends.

use std::fmt;

use telemetry::QpcTimestamp;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::IMFSample;

/// A decoded picture on the GPU: NV12, in one array slice of a texture the
/// decoder owns. The slice stays reserved for this frame (the decoder will
/// not write it) until the frame is dropped, so frames must be released
/// promptly or the decoder runs out of output surfaces.
pub struct DecodedFrame {
    pub texture: ID3D11Texture2D,
    /// Array slice of `texture` holding the picture.
    pub slice: u32,
    /// Picture size (the visible area, top-left aligned in the texture).
    pub width: u32,
    pub height: u32,
    pub frame_id: u64,
    /// The source frame's timestamp, carried from `submit`.
    pub captured_at: QpcTimestamp,
    pub submitted_at: QpcTimestamp,
    pub decoded_at: QpcTimestamp,
    /// Keeps the decoder's output surface reserved while the frame lives.
    pub(crate) _sample: IMFSample,
}

impl fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("slice", &self.slice)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("frame_id", &self.frame_id)
            .finish_non_exhaustive()
    }
}

/// Result of offering an access unit to the decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeSubmit {
    Accepted,
    /// The decoder has output to hand back first: poll, then submit again.
    Busy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    InvalidInput(String),
    Unsupported(String),
    Windows(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::InvalidInput(message) => write!(f, "invalid input: {message}"),
            DecodeError::Unsupported(message) => write!(f, "unsupported: {message}"),
            DecodeError::Windows(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<windows::core::Error> for DecodeError {
    fn from(error: windows::core::Error) -> Self {
        DecodeError::Windows(error.to_string())
    }
}

/// A hardware H.264 decoder producing NV12 `DecodedFrame`s on the GPU.
///
/// `submit` and `poll` never block on the decoder.
pub trait VideoDecoder {
    /// Short name for reports, e.g. `media-foundation`.
    fn name(&self) -> &'static str;

    /// Offers one access unit (Annex B, start codes included) with the
    /// identity of the frame it encodes.
    fn submit(
        &mut self,
        access_unit: &[u8],
        frame_id: u64,
        captured_at: QpcTimestamp,
    ) -> Result<DecodeSubmit, DecodeError>;

    /// The next decoded frame, if any.
    fn poll(&mut self) -> Result<Option<DecodedFrame>, DecodeError>;

    /// Finishes every submitted access unit and returns the frames.
    fn drain(&mut self) -> Result<Vec<DecodedFrame>, DecodeError>;
}

/// Identity of submitted access units not yet decoded, oldest first, keyed
/// by the sample time given to the decoder. Bounded: the oldest entry is
/// forgotten if the decoder never returns it (e.g. a damaged access unit).
#[derive(Debug)]
pub(crate) struct Pending {
    entries: std::collections::VecDeque<(i64, PendingFrame)>,
    capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingFrame {
    pub frame_id: u64,
    pub captured_at: QpcTimestamp,
    pub submitted_at: QpcTimestamp,
}

impl Pending {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub(crate) fn push(&mut self, key: i64, frame: PendingFrame) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((key, frame));
    }

    /// The entry for `key`, dropping older ones the decoder skipped.
    pub(crate) fn take(&mut self, key: i64) -> Option<PendingFrame> {
        let position = self.entries.iter().position(|(entry, _)| *entry == key)?;
        self.entries.drain(..position);
        self.entries.pop_front().map(|(_, frame)| frame)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(frame_id: u64) -> PendingFrame {
        PendingFrame {
            frame_id,
            captured_at: QpcTimestamp::from_ticks(0),
            submitted_at: QpcTimestamp::from_ticks(0),
        }
    }

    #[test]
    fn pending_matches_keys_and_skips_lost_entries() {
        let mut pending = Pending::new(3);
        for key in 0..3 {
            pending.push(key, frame(key as u64 + 100));
        }
        assert_eq!(pending.take(0).map(|f| f.frame_id), Some(100));
        // Key 1 never comes back; taking 2 forgets it.
        assert_eq!(pending.take(2).map(|f| f.frame_id), Some(102));
        assert_eq!(pending.len(), 0);
        assert_eq!(pending.take(7), None);
    }

    #[test]
    fn pending_is_bounded() {
        let mut pending = Pending::new(2);
        for key in 0..5 {
            pending.push(key, frame(key as u64));
        }
        assert_eq!(pending.len(), 2);
        assert_eq!(pending.take(0), None);
        assert_eq!(pending.take(3).map(|f| f.frame_id), Some(3));
    }
}
