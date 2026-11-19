//! Caps the rate of delivered frames without delaying them.
//!
//! Time is divided into fixed slots of one period, and the first frame that
//! arrives in a slot is delivered at once; later frames in the same slot
//! are dropped. The slot grid is centered on the first frame, so a source
//! running at the target rate keeps every frame despite up to half a period
//! of jitter, and a source slightly faster than the target (e.g. WGC asked
//! for one frame per period, which a high refresh rate monitor turns into a
//! few frames more) still fills every slot. A minimum spacing of half a
//! period keeps two frames from being delivered back to back across a slot
//! boundary (e.g. after a gap).

use telemetry::{QpcFrequency, QpcTimestamp};

/// Minimum time between two delivered frames, as a fraction of the period.
const MIN_SPACING_FRACTION: f64 = 0.5;

#[derive(Clone, Debug)]
pub struct RateLimiter {
    period_ticks: i64,
    min_spacing_ticks: i64,
    /// Start of slot 0: half a period before the first delivered frame.
    origin: Option<i64>,
    last_slot: i64,
    last_admitted: i64,
}

impl RateLimiter {
    /// # Panics
    /// If `fps` is zero.
    pub fn new(fps: u32, frequency: QpcFrequency) -> Self {
        assert!(fps > 0, "fps must be positive");
        let period = frequency.ticks_per_second() as f64 / f64::from(fps);
        Self {
            period_ticks: period.round() as i64,
            min_spacing_ticks: (period * MIN_SPACING_FRACTION).round() as i64,
            origin: None,
            last_slot: 0,
            last_admitted: 0,
        }
    }

    /// Decides whether a frame arriving at `now` is delivered.
    pub fn admit(&mut self, now: QpcTimestamp) -> bool {
        let now = now.ticks();
        let Some(origin) = self.origin else {
            self.origin = Some(now - self.period_ticks / 2);
            self.last_slot = 0;
            self.last_admitted = now;
            return true;
        };
        let slot = (now - origin).div_euclid(self.period_ticks);
        if slot <= self.last_slot || now - self.last_admitted < self.min_spacing_ticks {
            return false;
        }
        self.last_slot = slot;
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
            assert!(
                delivered.len() <= target + 1,
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
            assert!(
                (target * 99 / 100..=target + 1).contains(&delivered.len()),
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
        let mut limiter = limiter(100); // 10 ms slots centered on the first frame
        assert!(limiter.admit(QpcTimestamp::from_ticks(0)));
        // A gap, then frames every 1 ms: one frame per slot (boundaries at
        // 85, 95, 105, 115 ms), never two closer than half a period.
        let admitted: Vec<i64> = (90..120)
            .map(|ms| ms * 1_000)
            .filter(|&time| limiter.admit(QpcTimestamp::from_ticks(time)))
            .collect();
        assert_eq!(admitted, [90_000, 95_000, 105_000, 115_000]);
    }

    #[test]
    #[should_panic(expected = "fps must be positive")]
    fn zero_fps_is_rejected() {
        let _ = limiter(0);
    }
}
