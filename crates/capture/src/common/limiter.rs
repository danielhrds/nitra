//! Caps the rate of delivered frames without delaying them.
//!
//! Frames are delivered as soon as they arrive, unless they arrive too early
//! for the target rate. Each admission moves the next due time forward by
//! one period, so the average rate converges to the target; a slack window
//! lets slightly early frames through so that a source running at the
//! target rate is not thinned out by jitter.

use telemetry::{QpcFrequency, QpcTimestamp};

/// Fraction of a period a frame may arrive early and still be admitted.
const SLACK_FRACTION: f64 = 0.25;

#[derive(Clone, Debug)]
pub struct RateLimiter {
    period_ticks: i64,
    slack_ticks: i64,
    next_due: Option<i64>,
}

impl RateLimiter {
    /// # Panics
    /// If `fps` is zero.
    pub fn new(fps: u32, frequency: QpcFrequency) -> Self {
        assert!(fps > 0, "fps must be positive");
        let period = frequency.ticks_per_second() as f64 / f64::from(fps);
        Self {
            period_ticks: period.round() as i64,
            slack_ticks: (period * SLACK_FRACTION).round() as i64,
            next_due: None,
        }
    }

    /// Decides whether a frame arriving at `now` is delivered.
    pub fn admit(&mut self, now: QpcTimestamp) -> bool {
        let now = now.ticks();
        let next_due = match self.next_due {
            None => now + self.period_ticks,
            Some(due) if now < due - self.slack_ticks => return false,
            // Keep the grid while on time; after a late frame, restart from
            // it (minus the slack) instead of bursting to catch up.
            Some(due) => (due + self.period_ticks).max(now + self.period_ticks - self.slack_ticks),
        };
        self.next_due = Some(next_due);
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
    fn no_burst_after_a_gap() {
        let mut limiter = limiter(100); // 10 ms period, 2.5 ms slack
        assert!(limiter.admit(QpcTimestamp::from_ticks(0)));
        // A 95 ms gap, then frames every 1 ms: the late frame restarts the
        // schedule instead of letting the missed periods through at once.
        assert!(limiter.admit(QpcTimestamp::from_ticks(95_000)));
        let early: Vec<bool> = (96..100)
            .map(|ms| limiter.admit(QpcTimestamp::from_ticks(ms * 1_000)))
            .collect();
        assert!(early.iter().all(|admitted| !admitted), "{early:?}");
        // Next admission: one period minus twice the slack after the late frame.
        assert!(limiter.admit(QpcTimestamp::from_ticks(100_000)));
        assert!(!limiter.admit(QpcTimestamp::from_ticks(101_000)));
    }

    #[test]
    #[should_panic(expected = "fps must be positive")]
    fn zero_fps_is_rejected() {
        let _ = limiter(0);
    }
}
