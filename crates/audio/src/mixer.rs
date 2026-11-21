//! Mixes several captured streams into one stream of 20 ms frames on a
//! common timeline. Each packet carries the QPC time of its first sample;
//! packets are placed by that time (snapped to the stream's own previous
//! packet when close, so rounding never leaves one-sample gaps), summed, and
//! a frame is handed out once it is `latency` old, so streams that deliver a
//! little later still make it in. A frame no stream touched is not handed
//! out (silence everywhere stays a gap, as with a single capture). Memory is
//! bounded: packets far in the future or already handed out are dropped.

use std::collections::{BTreeMap, HashMap};

use telemetry::{QpcFrequency, QpcTimestamp};

use crate::frames::AudioFrame;
use crate::opus::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

/// A packet within this of where its stream's previous one ended continues
/// it (timing noise), instead of being placed by its own time.
const SNAP_SAMPLES: i64 = SAMPLE_RATE as i64 / 500; // 2 ms
/// Frames further than this ahead of the newest handed-out one are dropped.
const MAX_AHEAD_FRAMES: u64 = 50; // 1 s

pub struct Mixer {
    frequency: QpcFrequency,
    /// Time of sample 0 of the timeline.
    epoch: QpcTimestamp,
    latency_samples: i64,
    /// Frame index -> summed interleaved samples.
    pending: BTreeMap<u64, Vec<f32>>,
    /// Index of the next frame to hand out.
    next: u64,
    /// Where each stream's last packet ended (per-channel sample index).
    ends: HashMap<u64, i64>,
    /// Samples dropped because they came after their frame was handed out.
    pub late_samples: u64,
}

impl Mixer {
    pub fn new(frequency: QpcFrequency, epoch: QpcTimestamp, latency_ms: u32) -> Self {
        Self {
            frequency,
            epoch,
            latency_samples: i64::from(SAMPLE_RATE) * i64::from(latency_ms) / 1000,
            pending: BTreeMap::new(),
            next: 0,
            ends: HashMap::new(),
            late_samples: 0,
        }
    }

    /// The timeline position (per-channel samples) of `at`.
    fn position(&self, at: QpcTimestamp) -> i64 {
        let ticks = i128::from(at.ticks() - self.epoch.ticks());
        (ticks * i128::from(SAMPLE_RATE) / i128::from(self.frequency.ticks_per_second())) as i64
    }

    /// Adds interleaved `samples` from `stream` whose first one played at
    /// `at`.
    pub fn push(&mut self, stream: u64, samples: &[f32], at: QpcTimestamp, discontinuity: bool) {
        let count = (samples.len() / CHANNELS) as i64;
        let mut start = self.position(at);
        if let Some(&end) = self.ends.get(&stream)
            && !discontinuity
            && (start - end).abs() <= SNAP_SAMPLES
        {
            start = end;
        }
        self.ends.insert(stream, start + count);
        let frame_len = FRAME_SAMPLES as i64;
        let first_kept = self.next as i64 * frame_len;
        let last_allowed = (self.next + MAX_AHEAD_FRAMES) as i64 * frame_len;
        for i in 0..count {
            let position = start + i;
            if position < first_kept {
                self.late_samples += 1;
                continue;
            }
            if position >= last_allowed {
                break;
            }
            let frame = (position / frame_len) as u64;
            let offset = (position % frame_len) as usize * CHANNELS;
            let buffer = self
                .pending
                .entry(frame)
                .or_insert_with(|| vec![0.0; FRAME_SAMPLES * CHANNELS]);
            let source = i as usize * CHANNELS;
            for c in 0..CHANNELS {
                buffer[offset + c] += samples[source + c];
            }
        }
    }

    /// Frames that are at least `latency` old at `now`, in order, clamped to
    /// [-1, 1]. Untouched frames in between are skipped.
    pub fn ready(&mut self, now: QpcTimestamp) -> Vec<AudioFrame> {
        let complete_before =
            (self.position(now) - self.latency_samples).max(0) / FRAME_SAMPLES as i64;
        let mut out = Vec::new();
        while (self.next as i64) < complete_before {
            if let Some(mut samples) = self.pending.remove(&self.next) {
                for s in &mut samples {
                    *s = s.clamp(-1.0, 1.0);
                }
                out.push(AudioFrame {
                    samples,
                    captured_at: self.time_of(self.next as i64 * FRAME_SAMPLES as i64),
                });
            }
            self.next += 1;
        }
        out
    }

    /// Forgets a stream that ended (its next packet is placed by its time).
    pub fn remove_stream(&mut self, stream: u64) {
        self.ends.remove(&stream);
    }

    fn time_of(&self, position: i64) -> QpcTimestamp {
        let ticks = i128::from(position) * i128::from(self.frequency.ticks_per_second())
            / i128::from(SAMPLE_RATE);
        QpcTimestamp::from_ticks(self.epoch.ticks() + ticks as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 MHz QPC; the epoch at 0.
    fn mixer(latency_ms: u32) -> Mixer {
        Mixer::new(
            QpcFrequency::new(10_000_000).unwrap(),
            QpcTimestamp::from_ticks(0),
            latency_ms,
        )
    }

    fn at_ms(ms: f64) -> QpcTimestamp {
        QpcTimestamp::from_ticks((ms * 10_000.0) as i64)
    }

    fn packet(per_channel: usize, value: f32) -> Vec<f32> {
        vec![value; per_channel * CHANNELS]
    }

    #[test]
    fn one_stream_comes_out_as_stamped_frames() {
        let mut m = mixer(40);
        for i in 0..4 {
            m.push(1, &packet(480, 0.25), at_ms(10.0 * f64::from(i)), false);
        }
        assert!(m.ready(at_ms(50.0)).is_empty(), "not 40 ms old yet");
        let frames = m.ready(at_ms(80.0));
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].captured_at, at_ms(0.0));
        assert_eq!(frames[1].captured_at, at_ms(20.0));
        assert!(frames.iter().all(|f| f.samples.iter().all(|&s| s == 0.25)));
    }

    #[test]
    fn two_streams_are_summed_and_clamped() {
        let mut m = mixer(0);
        m.push(1, &packet(960, 0.75), at_ms(0.0), false);
        m.push(2, &packet(960, 0.5), at_ms(0.0), false);
        m.push(3, &packet(960, -0.25), at_ms(0.0), false);
        let frames = m.ready(at_ms(20.0));
        assert_eq!(frames.len(), 1);
        // 0.75 + 0.5 - 0.25 = 1.0; and 0.75 + 0.5 alone would clamp.
        assert!(frames[0].samples.iter().all(|&s| (s - 1.0).abs() < 1e-6));
        let mut loud = mixer(0);
        loud.push(1, &packet(960, 0.75), at_ms(0.0), false);
        loud.push(2, &packet(960, 0.5), at_ms(0.0), false);
        assert!(loud.ready(at_ms(20.0))[0].samples.iter().all(|&s| s == 1.0));
    }

    #[test]
    fn streams_offset_in_time_line_up_by_their_stamps() {
        let mut m = mixer(0);
        // Stream 2 starts 10 ms (480 samples) after stream 1.
        m.push(1, &packet(960, 0.1), at_ms(0.0), false);
        m.push(2, &packet(480, 0.2), at_ms(10.0), false);
        let frame = &m.ready(at_ms(20.0))[0];
        assert!((frame.samples[0] - 0.1).abs() < 1e-6);
        assert!((frame.samples[479 * CHANNELS] - 0.1).abs() < 1e-6);
        assert!((frame.samples[480 * CHANNELS] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn rounding_noise_does_not_leave_gaps() {
        let mut m = mixer(0);
        // 10 ms packets stamped up to 0.3 ms off: every sample still counts.
        for (i, noise) in [0.0, -0.3, 0.3, -0.3].into_iter().enumerate() {
            m.push(1, &packet(480, 0.5), at_ms(10.0 * i as f64 + noise), false);
        }
        let frames = m.ready(at_ms(40.3));
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.samples.iter().all(|&s| s == 0.5)));
    }

    #[test]
    fn untouched_frames_stay_gaps() {
        let mut m = mixer(0);
        m.push(1, &packet(960, 0.5), at_ms(0.0), false);
        m.push(1, &packet(960, 0.5), at_ms(100.0), false);
        let frames = m.ready(at_ms(120.0));
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].captured_at, at_ms(100.0));
    }

    #[test]
    fn late_packets_are_dropped_and_counted() {
        let mut m = mixer(0);
        m.push(1, &packet(960, 0.5), at_ms(0.0), false);
        assert_eq!(m.ready(at_ms(20.0)).len(), 1);
        m.push(2, &packet(960, 0.5), at_ms(0.0), false);
        assert_eq!(m.late_samples, 960);
        assert!(m.ready(at_ms(40.0)).is_empty());
    }

    #[test]
    fn memory_is_bounded_for_packets_far_ahead() {
        let mut m = mixer(0);
        m.push(1, &packet(960, 0.5), at_ms(60_000.0), false);
        assert!(m.pending.is_empty());
    }
}
