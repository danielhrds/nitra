//! Playback that follows the sender's clock. The sender's sound card (or
//! QPC, for synthetic audio) and the receiver's sound card tick at slightly
//! different rates (tens to hundreds of ppm), so a fixed queue slowly
//! empties (gaps) or fills (growing delay). The controller watches the
//! queue and nudges a resampling ratio around 1.0 to hold it at a target;
//! the resampler plays the input that much faster or slower.
//!
//! Linear interpolation is enough here: the ratio stays within ±0.5 %,
//! where the pitch change and the interpolation error are inaudible.
//!
//! ±0.5 % only removes small excesses quickly (a 50 ms surplus takes ~10 s).
//! After a stall, audio arrives late in a burst and the queue jumps; played
//! out slowly, it would leave the sound behind the picture for tens of
//! seconds. Beyond [`CATCH_UP_FRAMES`] above the target, the excess is
//! dropped at once instead (a short skip, like video jumping to the present).

use std::collections::VecDeque;

use crate::opus::CHANNELS;

/// The ratio never leaves 1 ± this.
pub const MAX_CORRECTION: f64 = 0.005;
/// More than this above the target (40 ms at 48 kHz) is dropped at once.
/// Normal arrivals saw the level by one 20 ms packet, well below it.
pub const CATCH_UP_FRAMES: usize = 48 * 40;

/// Holds a queue level (in frames) at a target by choosing a ratio.
#[derive(Clone, Debug)]
pub struct DriftController {
    target: f64,
    integral: f64,
    smoothed: Option<f64>,
}

impl DriftController {
    /// Proportional gain: 1 ms of excess level → +0.0001 (100 ppm)…
    const KP: f64 = 0.1 / 48_000.0 * 1000.0 / 1000.0;
    /// …and an integral term that removes a constant drift.
    const KI: f64 = Self::KP / 200.0;

    pub fn new(target_frames: usize) -> Self {
        Self {
            target: target_frames as f64,
            integral: 0.0,
            smoothed: None,
        }
    }

    /// The ratio for the next block, given the frames queued now. Called
    /// once per playback block (~10 ms).
    pub fn ratio(&mut self, queued_frames: usize) -> f64 {
        // Smooth the level: it jumps by a whole packet at every arrival.
        let level = queued_frames as f64;
        let smoothed = self.smoothed.map_or(level, |s| s + (level - s) * 0.05);
        self.smoothed = Some(smoothed);
        let error = smoothed - self.target;
        self.integral =
            (self.integral + error).clamp(-MAX_CORRECTION / Self::KI, MAX_CORRECTION / Self::KI);
        let correction =
            (Self::KP * error + Self::KI * self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        1.0 + correction
    }

    /// How many frames to drop now to be back at the target, if the level
    /// is more than [`CATCH_UP_FRAMES`] above it (0 otherwise). Resets the
    /// controller to the target, so it does not keep correcting a surplus
    /// that is gone (it relearns the drift within seconds).
    pub fn catch_up(&mut self, queued_frames: usize) -> usize {
        let target = self.target as usize;
        if queued_frames <= target + CATCH_UP_FRAMES {
            return 0;
        }
        self.smoothed = Some(self.target);
        self.integral = 0.0;
        queued_frames - target
    }
}

/// Plays interleaved stereo frames at `ratio` input frames per output frame.
#[derive(Clone, Debug, Default)]
pub struct Resampler {
    input: VecDeque<[f32; CHANNELS]>,
    /// Position between `input[0]` and `input[1]`, in [0, 1).
    phase: f64,
}

impl Resampler {
    pub fn push(&mut self, interleaved: &[f32]) {
        self.input
            .extend(interleaved.as_chunks::<CHANNELS>().0.iter().copied());
    }

    /// Frames queued (not yet played).
    pub fn queued(&self) -> usize {
        self.input.len()
    }

    /// Drops the oldest frames beyond `max` (the queue must not grow without bound).
    pub fn trim_to(&mut self, max: usize) -> usize {
        let excess = self.input.len().saturating_sub(max);
        self.drop_oldest(excess)
    }

    /// Drops up to `frames` of the oldest audio; returns how many.
    pub fn drop_oldest(&mut self, frames: usize) -> usize {
        let frames = frames.min(self.input.len());
        self.input.drain(..frames);
        frames
    }

    /// Writes up to `out.len() / CHANNELS` frames; returns how many it had
    /// input for (the rest of `out` is left untouched).
    pub fn pull(&mut self, out: &mut [f32], ratio: f64) -> usize {
        let mut written = 0;
        for frame in out.as_chunks_mut::<CHANNELS>().0 {
            if self.input.len() < 2 {
                break;
            }
            let (a, b) = (self.input[0], self.input[1]);
            let t = self.phase as f32;
            for c in 0..CHANNELS {
                frame[c] = a[c] + (b[c] - a[c]) * t;
            }
            written += 1;
            self.phase += ratio;
            while self.phase >= 1.0 && !self.input.is_empty() {
                self.input.pop_front();
                self.phase -= 1.0;
            }
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(frames: usize, start: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|i| [i as f32, -(i as f32)])
            .collect()
    }

    #[test]
    fn ratio_one_plays_the_input_unchanged() {
        let mut r = Resampler::default();
        r.push(&ramp(1000, 0));
        let mut out = vec![0.0; 2 * 500];
        assert_eq!(r.pull(&mut out, 1.0), 500);
        for (i, f) in out.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(*f, [i as f32, -(i as f32)]);
        }
        assert_eq!(r.queued(), 500);
    }

    #[test]
    fn a_higher_ratio_consumes_more_input() {
        let mut r = Resampler::default();
        r.push(&ramp(100_000, 0));
        let mut out = vec![0.0; 2 * 48_000];
        r.pull(&mut out, 1.001);
        // 48 000 output frames at 1.001 consume ~48 048 input frames.
        let consumed = 100_000 - r.queued();
        assert!((48_046..=48_050).contains(&consumed), "{consumed}");
        // Interpolated values stay on the ramp.
        let last = out[out.len() - 2];
        assert!((last - 47_999.0 * 1.001).abs() < 0.01, "{last}");
    }

    #[test]
    fn running_dry_writes_what_it_has() {
        let mut r = Resampler::default();
        r.push(&ramp(10, 0));
        let mut out = vec![7.0; 2 * 20];
        assert_eq!(r.pull(&mut out, 1.0), 9);
        assert_eq!(out[2 * 9], 7.0); // untouched
    }

    #[test]
    fn trimming_bounds_the_queue() {
        let mut r = Resampler::default();
        r.push(&ramp(1000, 0));
        assert_eq!(r.trim_to(100), 900);
        assert_eq!(r.queued(), 100);
    }

    /// A sender 300 ppm fast (or slow) into a playback device at 48 kHz:
    /// the controller holds the queue near its target for 10 minutes.
    #[test]
    fn the_controller_holds_the_level_against_drift() {
        for ppm in [300.0_f64, -300.0] {
            let target = 48 * 60; // 60 ms
            let mut controller = DriftController::new(target);
            let mut r = Resampler::default();
            let block = 480; // 10 ms device periods
            let mut out = vec![0.0; 2 * block];
            let mut sent = 0.0_f64;
            let (mut worst, mut sum, mut n) = (0_usize, 0.0_f64, 0.0_f64);
            r.push(&ramp(target, 0));
            for step in 0..60_000_u32 {
                // The sender delivers 20 ms packets at its own rate.
                let due = f64::from(step + 1) * block as f64 * (1.0 + ppm / 1e6);
                while sent + 960.0 <= due {
                    r.push(&ramp(960, 0));
                    sent += 960.0;
                }
                let level = r.queued();
                let ratio = controller.ratio(level);
                assert!((ratio - 1.0).abs() <= MAX_CORRECTION + 1e-12);
                let got = r.pull(&mut out, ratio);
                assert_eq!(got, block, "ran dry at step {step} ({ppm} ppm)");
                if step > 30_000 {
                    worst = worst.max(level.abs_diff(target));
                    sum += level as f64;
                    n += 1.0;
                }
            }
            // After settling: on target on average (the level saws by a
            // 20 ms packet at every arrival), never far off.
            let mean_off = (sum / n - target as f64).abs();
            assert!(
                mean_off < 48.0 * 5.0,
                "{ppm} ppm: mean off by {mean_off} frames"
            );
            assert!(worst < 48 * 30, "{ppm} ppm: off by {worst} frames");
        }
    }

    /// A stall: nothing for 300 ms, then the late audio arrives at once.
    /// The surplus is dropped in one go and playback holds the target
    /// again without running dry.
    #[test]
    fn a_burst_after_a_stall_is_caught_up_at_once() {
        let target = 48 * 60;
        let mut controller = DriftController::new(target);
        let mut r = Resampler::default();
        let block = 480;
        let mut out = vec![0.0; 2 * block];
        r.push(&ramp(target, 0));
        let (mut sent, mut catch_ups, mut dropped) = (0.0_f64, 0, 0);
        let mut stalled = Vec::new();
        for step in 0..3000_u32 {
            let due = f64::from(step + 1) * block as f64;
            // Steps 1000..1030 (300 ms): the network holds the packets back.
            if !(1000..1030).contains(&step) {
                while sent + 960.0 <= due {
                    r.push(&ramp(960, 0));
                    sent += 960.0;
                }
            }
            let excess = controller.catch_up(r.queued());
            if excess > 0 {
                catch_ups += 1;
                dropped += r.drop_oldest(excess);
            }
            let ratio = controller.ratio(r.queued());
            let level = r.queued();
            let got = r.pull(&mut out, ratio);
            if (1000..1030).contains(&step) {
                stalled.push(got);
            } else if step > 50 {
                assert_eq!(got, block, "ran dry at step {step}");
            }
            if step > 1040 {
                // Back near the target right after the catch-up.
                assert!(
                    level.abs_diff(target) < 48 * 30,
                    "step {step}: level {level}"
                );
            }
        }
        assert_eq!(catch_ups, 1, "one catch-up for one stall");
        // ~300 ms of surplus, less what played from the queue meanwhile.
        assert!(dropped > 48 * 200, "dropped only {dropped} frames");
        println!("stall: dropped {} ms in {catch_ups} catch-up", dropped / 48);
    }

    #[test]
    fn normal_arrivals_never_trigger_a_catch_up() {
        let target = 48 * 60;
        let mut controller = DriftController::new(target);
        // The level saws between target and target + one packet (20 ms).
        for i in 0..10_000 {
            assert_eq!(controller.catch_up(target + (i % 960)), 0);
        }
        assert_eq!(controller.catch_up(target + CATCH_UP_FRAMES), 0);
        assert_eq!(
            controller.catch_up(target + CATCH_UP_FRAMES + 1),
            CATCH_UP_FRAMES + 1
        );
    }
}
