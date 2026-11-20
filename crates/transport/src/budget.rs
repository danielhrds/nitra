//! Per-viewer send budget: keeps one slow viewer from growing the host's
//! queues or holding back the others.
//!
//! str0m queues every packet written and its pacer drains the queue at about
//! the bandwidth estimate, with no bound. The budget models that backlog
//! (bytes written minus bytes the estimate lets out) and skips access units
//! for a viewer whose backlog would exceed [`MAX_BACKLOG`]. A skipped delta
//! frame breaks the decoding chain, so that viewer then waits for the next
//! keyframe that fits.

use std::time::{Duration, Instant};

/// Most media time a viewer may have waiting in its send queue.
pub const MAX_BACKLOG: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Send,
    /// Not sent; `fell_behind` is true on the first skip of a run.
    Skip {
        fell_behind: bool,
    },
}

#[derive(Clone, Debug)]
pub struct SendBudget {
    estimate_bps: u64,
    backlog_bytes: f64,
    updated: Instant,
    waiting_keyframe: bool,
}

impl SendBudget {
    /// A new viewer starts waiting for a keyframe: delta frames before it
    /// are useless to its decoder.
    pub fn new(estimate_bps: u64, now: Instant) -> Self {
        Self {
            estimate_bps,
            backlog_bytes: 0.0,
            updated: now,
            waiting_keyframe: true,
        }
    }

    pub fn set_estimate(&mut self, bps: u64, now: Instant) {
        self.drain(now);
        self.estimate_bps = bps;
    }

    pub fn estimate_bps(&self) -> u64 {
        self.estimate_bps
    }

    pub fn waiting_keyframe(&self) -> bool {
        self.waiting_keyframe
    }

    /// Decides whether an access unit of `bytes` goes to this viewer, and
    /// accounts for it when it does.
    pub fn offer(&mut self, bytes: usize, keyframe: bool, now: Instant) -> Decision {
        self.drain(now);
        if self.waiting_keyframe && !keyframe {
            return Decision::Skip { fell_behind: false };
        }
        if self.backlog() > MAX_BACKLOG {
            let fell_behind = !self.waiting_keyframe;
            self.waiting_keyframe = true;
            return Decision::Skip { fell_behind };
        }
        self.waiting_keyframe = false;
        self.backlog_bytes += bytes as f64;
        Decision::Send
    }

    /// Media time the queued bytes need at the current estimate.
    pub fn backlog(&self) -> Duration {
        if self.estimate_bps == 0 {
            return if self.backlog_bytes > 0.0 {
                Duration::MAX
            } else {
                Duration::ZERO
            };
        }
        Duration::from_secs_f64(self.backlog_bytes * 8.0 / self.estimate_bps as f64)
    }

    fn drain(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.updated = now;
        let sent = elapsed * self.estimate_bps as f64 / 8.0;
        self.backlog_bytes = (self.backlog_bytes - sent).max(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_new_viewer_starts_at_a_keyframe() {
        let t = Instant::now();
        let mut budget = SendBudget::new(10_000_000, t);
        assert_eq!(
            budget.offer(10_000, false, t),
            Decision::Skip { fell_behind: false }
        );
        assert_eq!(budget.offer(100_000, true, t), Decision::Send);
        assert_eq!(budget.offer(10_000, false, t + 16 * MS), Decision::Send);
    }

    #[test]
    fn a_fast_viewer_gets_every_frame() {
        let t = Instant::now();
        let mut budget = SendBudget::new(20_000_000, t);
        // 10 Mbps stream at 60 fps with a 200 KB keyframe each second.
        for i in 0..600_u32 {
            let keyframe = i % 60 == 0;
            let bytes = if keyframe { 200_000 } else { 20_000 };
            assert_eq!(
                budget.offer(bytes, keyframe, t + i * 16 * MS),
                Decision::Send,
                "frame {i}"
            );
        }
    }

    #[test]
    fn a_slow_viewer_falls_behind_once_and_resumes_at_a_keyframe() {
        let t = Instant::now();
        // 2 Mbps estimate, ~10 Mbps stream: the backlog grows ~130 ms/s per frame.
        let mut budget = SendBudget::new(2_000_000, t);
        assert_eq!(budget.offer(100_000, true, t), Decision::Send); // 400 ms queued
        assert_eq!(
            budget.offer(20_000, false, t + 16 * MS),
            Decision::Skip { fell_behind: true }
        );
        // Deltas keep being skipped (no second "fell behind"), even once drained.
        assert_eq!(
            budget.offer(20_000, false, t + 33 * MS),
            Decision::Skip { fell_behind: false }
        );
        assert_eq!(
            budget.offer(20_000, false, t + 500 * MS),
            Decision::Skip { fell_behind: false }
        );
        assert!(budget.waiting_keyframe());
        // The queue drained (400 ms of data at 2 Mbps): the next keyframe goes out.
        assert_eq!(budget.offer(100_000, true, t + 600 * MS), Decision::Send);
        assert!(!budget.waiting_keyframe());
    }

    #[test]
    fn the_backlog_stays_bounded_whatever_is_offered() {
        let t = Instant::now();
        let mut budget = SendBudget::new(1_000_000, t);
        for i in 0..1000_u32 {
            budget.offer(50_000, i % 10 == 0, t + i * 16 * MS);
            // At most one access unit above the limit.
            assert!(budget.backlog() <= MAX_BACKLOG + Duration::from_millis(400));
        }
    }

    #[test]
    fn a_lower_estimate_counts_from_now() {
        let t = Instant::now();
        let mut budget = SendBudget::new(10_000_000, t);
        budget.offer(125_000, true, t); // 100 ms at 10 Mbps
        budget.set_estimate(1_000_000, t + 50 * MS); // 62.5 KB left = 500 ms at 1 Mbps
        assert_eq!(
            budget.offer(10_000, false, t + 50 * MS),
            Decision::Skip { fell_behind: true }
        );
    }

    #[test]
    fn no_estimate_blocks_only_while_bytes_wait() {
        let t = Instant::now();
        let mut budget = SendBudget::new(0, t);
        assert_eq!(budget.offer(1_000, true, t), Decision::Send);
        assert_eq!(
            budget.offer(1_000, false, t + MS),
            Decision::Skip { fell_behind: true }
        );
    }
}
