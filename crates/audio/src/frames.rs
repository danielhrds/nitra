//! Cuts captured audio into 20 ms frames stamped with the QPC time of their
//! first sample. Capture packets come in device-sized pieces; a gap or a
//! discontinuity starts a new timeline (the partial frame is padded with
//! silence), so stamps always say when the sound was played.

use telemetry::{QpcFrequency, QpcTimestamp};

use crate::opus::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

/// A larger mismatch between the expected and the reported time of a
/// packet is a gap (e.g. nothing was playing), not jitter.
const GAP_TOLERANCE_US: i64 = 2_000;

#[derive(Clone, Debug, PartialEq)]
pub struct AudioFrame {
    /// `FRAME_SAMPLES * CHANNELS` interleaved samples.
    pub samples: Vec<f32>,
    /// When the first sample was played (captured).
    pub captured_at: QpcTimestamp,
}

pub struct Framer {
    frequency: QpcFrequency,
    pending: Vec<f32>,
    /// Time of `pending[0]`.
    start: Option<QpcTimestamp>,
    /// Gaps and discontinuities seen.
    pub restarts: u64,
}

impl Framer {
    pub fn new(frequency: QpcFrequency) -> Self {
        Self {
            frequency,
            pending: Vec::with_capacity(FRAME_SAMPLES * CHANNELS * 2),
            start: None,
            restarts: 0,
        }
    }

    /// Adds interleaved `samples` whose first one played at `at`; returns
    /// the frames completed.
    pub fn push(
        &mut self,
        samples: &[f32],
        at: QpcTimestamp,
        discontinuity: bool,
    ) -> Vec<AudioFrame> {
        let mut out = Vec::new();
        if let Some(start) = self.start {
            let expected = self.offset(start, self.pending.len() / CHANNELS);
            let error_us = (at.ticks() - expected.ticks()) * 1_000_000
                / self.frequency.ticks_per_second() as i64;
            if discontinuity || error_us.abs() > GAP_TOLERANCE_US {
                self.restarts += 1;
                if !self.pending.is_empty() {
                    self.pending.resize(FRAME_SAMPLES * CHANNELS, 0.0);
                    out.push(self.take_frame(start));
                }
                self.pending.clear();
                self.start = None;
            }
        }
        let mut start = *self.start.get_or_insert(at);
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= FRAME_SAMPLES * CHANNELS {
            out.push(self.take_frame(start));
            start = self.offset(start, FRAME_SAMPLES);
        }
        self.start = Some(start);
        out
    }

    fn take_frame(&mut self, start: QpcTimestamp) -> AudioFrame {
        let rest = self.pending.split_off(FRAME_SAMPLES * CHANNELS);
        let samples = std::mem::replace(&mut self.pending, rest);
        AudioFrame {
            samples,
            captured_at: start,
        }
    }

    /// The time `samples` (per channel) after `start`.
    fn offset(&self, start: QpcTimestamp, samples: usize) -> QpcTimestamp {
        let ticks =
            samples as i64 * self.frequency.ticks_per_second() as i64 / i64::from(SAMPLE_RATE);
        QpcTimestamp::from_ticks(start.ticks() + ticks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 MHz, the usual QPC frequency.
    fn framer() -> Framer {
        Framer::new(QpcFrequency::new(10_000_000).unwrap())
    }

    fn at_ms(ms: f64) -> QpcTimestamp {
        QpcTimestamp::from_ticks((ms * 10_000.0) as i64)
    }

    fn samples(per_channel: usize, value: f32) -> Vec<f32> {
        vec![value; per_channel * CHANNELS]
    }

    #[test]
    fn packets_become_20_ms_frames_with_the_right_stamps() {
        let mut f = framer();
        // 10 ms packets (480 samples) back to back.
        let mut frames = Vec::new();
        for i in 0..10 {
            frames.extend(f.push(
                &samples(480, i as f32),
                at_ms(1000.0 + 10.0 * i as f64),
                false,
            ));
        }
        assert_eq!(frames.len(), 5);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.samples.len(), FRAME_SAMPLES * CHANNELS);
            assert_eq!(frame.captured_at, at_ms(1000.0 + 20.0 * i as f64));
            // First half from packet 2i, second from 2i + 1.
            assert_eq!(frame.samples[0], (2 * i) as f32);
            assert_eq!(*frame.samples.last().unwrap(), (2 * i + 1) as f32);
        }
        assert_eq!(f.restarts, 0);
    }

    #[test]
    fn small_timing_noise_is_not_a_gap() {
        let mut f = framer();
        let mut n = 0;
        for i in 0..20 {
            let noise = if i % 2 == 0 { 0.5 } else { -0.5 };
            n += f
                .push(&samples(480, 0.0), at_ms(10.0 * i as f64 + noise), false)
                .len();
        }
        assert_eq!(n, 10);
        assert_eq!(f.restarts, 0);
    }

    #[test]
    fn a_gap_pads_the_partial_frame_and_restarts_the_timeline() {
        let mut f = framer();
        assert!(f.push(&samples(480, 1.0), at_ms(0.0), false).is_empty());
        // Nothing for 500 ms, then sound again.
        let frames = f.push(&samples(960, 2.0), at_ms(500.0), false);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].captured_at, at_ms(0.0));
        assert_eq!(frames[0].samples[0], 1.0);
        assert_eq!(*frames[0].samples.last().unwrap(), 0.0); // padded
        assert_eq!(frames[1].captured_at, at_ms(500.0));
        assert_eq!(f.restarts, 1);
    }

    #[test]
    fn a_discontinuity_restarts_even_on_time() {
        let mut f = framer();
        f.push(&samples(480, 1.0), at_ms(0.0), false);
        let frames = f.push(&samples(480, 2.0), at_ms(10.0), true);
        assert_eq!(frames.len(), 1);
        assert_eq!(f.restarts, 1);
    }
}
