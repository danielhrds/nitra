//! Caps the rate of delivered frames without delaying them.
//!
//! A credit balance: credit builds up at the target rate (one frame per
//! period) up to [`MAX_CREDIT`] frames, and a delivered frame spends one. On
//! average no more than the target rate gets through, so a faster source is
//! thinned to it; a source at the target rate whose frames come unevenly (a
//! late frame, then a few early ones catching up, as games and windows do)
//! loses none, because the late frame left credit for the early ones. A
//! slot grid dropped real frames in those catch-up runs (measured: ~1.4 per
//! second of a 60 fps source, seen as small stutters). Credit builds 0.2%
//! faster than the target, so a source whose clock is a little fast (a game
//! or monitor at 60.02 Hz; 59.94 Hz video is 0.1% off) is not thinned by a
//! frame a minute. A minimum spacing keeps frames from coming back to back.

use telemetry::{QpcFrequency, QpcTimestamp};

/// Minimum time between two delivered frames, as a fraction of the period
/// (a frame right after a late one may come ~0.45 of a period later).
const MIN_SPACING_FRACTION: f64 = 0.4;
/// Credit builds this much faster than the target rate.
const RATE_SLACK: f64 = 1.002;
/// The most credit kept (in frames): a late frame can be made up for, but a
/// long pause does not let a burst through.
const MAX_CREDIT: f64 = 2.0;
/// Rounding of arrival times (whole ticks) must not cost a frame.
const CREDIT_SLACK: f64 = 0.01;

#[derive(Clone, Debug)]
pub struct RateLimiter {
    period_ticks: f64,
    min_spacing_ticks: i64,
    /// Frames that may be delivered now (fractional).
    credit: f64,
    last_seen: Option<i64>,
    last_admitted: i64,
}

impl RateLimiter {
    /// # Panics
    /// If `fps` is zero.
    pub fn new(fps: u32, frequency: QpcFrequency) -> Self {
        assert!(fps > 0, "fps must be positive");
        let period = frequency.ticks_per_second() as f64 / f64::from(fps);
        Self {
            period_ticks: period / RATE_SLACK,
            min_spacing_ticks: (period * MIN_SPACING_FRACTION).round() as i64,
            credit: MAX_CREDIT,
            last_seen: None,
            last_admitted: i64::MIN / 2,
        }
    }

    /// Decides whether a frame arriving at `now` is delivered.
    pub fn admit(&mut self, now: QpcTimestamp) -> bool {
        let now = now.ticks();
        if let Some(last) = self.last_seen {
            let earned = (now - last).max(0) as f64 / self.period_ticks;
            self.credit = (self.credit + earned).min(MAX_CREDIT);
        }
        self.last_seen = Some(now);
        if self.credit < 1.0 - CREDIT_SLACK || now - self.last_admitted < self.min_spacing_ticks {
            return false;
        }
        self.credit -= 1.0;
        self.last_admitted = now;
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 1 tick per microsecond.
    fn limiter(fps: u32) -> RateLimiter {
        RateLimiter::new(fps, QpcFrequency::new(1_000_000).unwrap())
    }

    /// Feeds arrivals every `interval_us` (plus optional jitter) for one
    /// second and returns how many were admitted.
    fn admitted(
        limiter: &mut RateLimiter,
        interval_us: f64,
        jitter_us: impl Fn(u32) -> f64,
    ) -> u32 {
        let mut count = 0;
        let mut index = 0;
        loop {
            let time = index as f64 * interval_us + jitter_us(index as u32);
            if time >= 1_000_000.0 {
                return count;
            }
            if limiter.admit(QpcTimestamp::from_ticks(time.round() as i64)) {
                count += 1;
            }
            index += 1;
        }
    }

    /// Arrival times (µs) of a WGC capture on a monitor refreshing at
    /// `refresh_hz`: WGC keeps a fixed cadence of `min_interval_us` and
    /// sends each frame on the first refresh at or after its slot in that
    /// cadence, plus a small delivery delay. Calibrated on the game session:
    /// factor 0.95 at 60 fps on the 320 Hz monitor measured 63.4 frames/s
    /// (this model: 63.2), factor 0.5 measured 114/s (model: up to 120).
    fn wgc_arrivals(refresh_hz: f64, min_interval_us: f64, seconds: f64) -> Vec<i64> {
        let refresh = 1_000_000.0 / refresh_hz;
        let mut arrivals: Vec<i64> = Vec::new();
        let mut index = 0_u32;
        loop {
            let nominal = f64::from(index) * min_interval_us;
            if nominal >= seconds * 1_000_000.0 {
                return arrivals;
            }
            let vblank = (nominal / refresh).ceil() * refresh;
            // Deterministic 0-400 µs delivery delay after the refresh.
            let delay = f64::from((index * 7919) % 401);
            let time = (vblank + delay).round() as i64;
            if arrivals.last().is_none_or(|&last| time > last) {
                arrivals.push(time);
            }
            index += 1;
        }
    }
    fn admit_all(limiter: &mut RateLimiter, arrivals: &[i64]) -> Vec<i64> {
        arrivals
            .iter()
            .copied()
            .filter(|&time| limiter.admit(QpcTimestamp::from_ticks(time)))
            .collect()
    }

    #[test]
    fn fast_source_is_thinned_to_target() {
        // A 320 Hz monitor captured at 60 fps.
        let count = admitted(&mut limiter(60), 1_000_000.0 / 320.0, |_| 0.0);
        assert!((59..=61).contains(&count), "{count}");
    }

    #[test]
    fn source_at_target_rate_with_jitter_loses_nothing() {
        // 60 Hz content with +-2 ms of arrival jitter.
        let jitter = |index: u32| {
            if index.is_multiple_of(2) {
                2_000.0
            } else {
                -2_000.0
            }
        };
        let count = admitted(&mut limiter(60), 1_000_000.0 / 60.0, jitter);
        assert_eq!(count, 60);
    }

    #[test]
    fn slower_source_passes_through() {
        assert_eq!(admitted(&mut limiter(60), 1_000_000.0 / 30.0, |_| 0.0), 30);
    }

    #[test]
    fn source_slightly_faster_than_target_fills_every_slot() {
        // WGC asked for one frame per period (0.95 of it) on a 320 Hz
        // monitor sends about 64/s at 60 fps and 32/s at 30 fps; the old
        // spacing-based limiter delivered only ~57.6/s from this in game.
        for fps in [60_u32, 30] {
            let period = 1_000_000.0 / f64::from(fps);
            let arrivals = wgc_arrivals(320.0, 0.95 * period, 10.0);
            let delivered = admit_all(&mut limiter(fps), &arrivals);
            let target = 10 * fps as usize;
            assert!(
                delivered.len() * 100 >= target * 99,
                "{fps} fps: {} of {target} delivered from {} arrivals",
                delivered.len(),
                arrivals.len()
            );
            // The starting credit (2 frames) and the 0.2% slack: +2 in 10 s.
            assert!(
                delivered.len() <= target + 3,
                "{fps} fps: {}",
                delivered.len()
            );
        }
    }

    #[test]
    fn twice_the_target_rate_is_thinned_exactly() {
        // The current WGC setting: up to twice the target rate.
        for fps in [60_u32, 30] {
            let period = 1_000_000.0 / f64::from(fps);
            let arrivals = wgc_arrivals(320.0, 0.5 * period, 10.0);
            let delivered = admit_all(&mut limiter(fps), &arrivals);
            let target = 10 * fps as usize;
            // The grid may follow a fast source by up to 0.2% (+1 frame per
            // 500): at most ~0.3% over the target here, 60-61 per second.
            assert!(
                (target * 99 / 100..=target + target / 300 + 1).contains(&delivered.len()),
                "{fps} fps: {} of {target}",
                delivered.len()
            );
        }
    }

    #[test]
    fn delivered_frames_are_at_least_half_a_period_apart() {
        let arrivals = wgc_arrivals(320.0, 0.95 * 1_000_000.0 / 60.0, 10.0);
        let delivered = admit_all(&mut limiter(60), &arrivals);
        let closest = delivered
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .min()
            .unwrap();
        assert!(closest >= 8_333, "closest frames {closest} µs apart");
    }

    #[test]
    fn no_burst_after_a_gap() {
        let mut limiter = limiter(100); // 10 ms period
        assert!(limiter.admit(QpcTimestamp::from_ticks(0)));
        // A gap, then frames every 1 ms: the saved credit (2 frames at most)
        // lets one extra frame through, then the target rate; never two
        // closer than the minimum spacing (4 ms here).
        let admitted: Vec<i64> = (90..140)
            .map(|ms| ms * 1_000)
            .filter(|&time| limiter.admit(QpcTimestamp::from_ticks(time)))
            .collect();
        assert_eq!(
            admitted,
            [90_000, 94_000, 100_000, 110_000, 120_000, 130_000]
        );
    }

    /// Arrivals of a source rendering at `fps` (its own clock) on a
    /// `refresh_hz` monitor: each frame shows on the first refresh after it
    /// is due, plus a 0-400 µs delivery delay, plus `late(n)` µs.
    fn vsynced_source(
        fps: f64,
        refresh_hz: f64,
        seconds: f64,
        late: impl Fn(u32) -> f64,
    ) -> Vec<i64> {
        let (period, refresh) = (1_000_000.0 / fps, 1_000_000.0 / refresh_hz);
        (0..(seconds * fps) as u32)
            .map(|n| {
                let due = f64::from(n) * period;
                let shown = (due / refresh).ceil() * refresh;
                (shown + f64::from((n * 7919) % 401) + late(n)).round() as i64
            })
            .collect()
    }

    /// The most frames delivered in any whole second (from the first frame).
    fn per_second(delivered: &[i64]) -> (usize, usize) {
        let first = delivered[0];
        let seconds = ((delivered[delivered.len() - 1] - first) / 1_000_000) as usize;
        let mut counts = vec![0_usize; seconds.max(1)];
        for &time in delivered {
            let second = ((time - first) / 1_000_000) as usize;
            if second < counts.len() {
                counts[second] += 1;
            }
        }
        (*counts.iter().min().unwrap(), *counts.iter().max().unwrap())
    }

    #[test]
    fn a_source_whose_clock_differs_a_little_keeps_every_frame() {
        // A game or monitor at 59.98 or 60.02 Hz, 2 minutes, on the 320 Hz
        // monitor. A fixed grid drifted onto slot edges (dropping frames and
        // leaving slots empty); the following grid keeps them all.
        for fps in [59.94, 59.98, 60.0, 60.02, 60.06] {
            let arrivals = vsynced_source(fps, 320.0, 120.0, |_| 0.0);
            let delivered = admit_all(&mut limiter(60), &arrivals);
            assert_eq!(delivered.len(), arrivals.len(), "{fps} Hz");
        }
    }

    #[test]
    fn a_source_at_the_target_rate_gives_exactly_that_rate() {
        let arrivals = vsynced_source(60.0, 320.0, 30.0, |_| 0.0);
        let delivered = admit_all(&mut limiter(60), &arrivals);
        assert_eq!(per_second(&delivered), (60, 60));
    }

    #[test]
    fn a_late_frame_is_made_up_for() {
        // A game without vsync: one frame in 37 comes 9 ms late and the next
        // one on time, 7.7 ms after it. The late frame's credit covers it.
        let arrivals = vsynced_source(
            60.0,
            1_000_000.0, // no rounding to a refresh
            30.0,
            |n| if n % 37 == 36 { 9_000.0 } else { 0.0 },
        );
        let delivered = admit_all(&mut limiter(60), &arrivals);
        assert_eq!(delivered.len(), arrivals.len());
    }

    #[test]
    fn a_faster_source_is_held_to_the_target() {
        // 63/s (WGC on the 320 Hz monitor) and a 144 fps game.
        for fps in [63.0, 144.0] {
            let arrivals = vsynced_source(fps, 320.0, 30.0, |_| 0.0);
            let delivered = admit_all(&mut limiter(60), &arrivals);
            let (low, high) = per_second(&delivered);
            assert!(low >= 59 && high <= 61, "{fps}: {low}..{high}");
        }
    }

    #[test]
    fn a_slower_source_keeps_every_frame() {
        let arrivals = vsynced_source(45.0, 320.0, 30.0, |_| 0.0);
        let delivered = admit_all(&mut limiter(60), &arrivals);
        assert_eq!(delivered.len(), arrivals.len());
    }

    #[test]
    #[should_panic(expected = "fps must be positive")]
    fn zero_fps_is_rejected() {
        let _ = limiter(0);
    }
}
