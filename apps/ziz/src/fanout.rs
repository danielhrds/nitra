//! Host policies for one encoder shared by every viewer: which bitrate the
//! encoder follows, and how often viewers may force a keyframe.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use transport::PeerId;

/// A viewer slower than this fraction of the fastest one no longer lowers
/// the encoder bitrate for everyone: the transport skips frames for it.
const HEALTHY_FRACTION: f64 = 0.5;

/// A viewer that fell behind this recently does not set the encoder bitrate
/// (its estimate swings with its losses).
pub const BEHIND_HOLD: Duration = Duration::from_secs(10);

/// One viewer as the bitrate policy sees it.
#[derive(Clone, Copy, Debug)]
pub struct ViewerRate {
    pub estimate_bps: u64,
    /// Fell behind within [`BEHIND_HOLD`].
    pub behind: bool,
}

/// The encoder bitrate for these viewers: among those keeping up (all of
/// them, if none is), the slowest that can still take at least half of what
/// the fastest one takes (estimates capped at `configured`), never below
/// `min`. `None` without viewers.
pub fn encoder_target(viewers: &[ViewerRate], configured: u32, min: u32) -> Option<u32> {
    let keeping_up = viewers.iter().any(|v| !v.behind);
    let capped = viewers
        .iter()
        .filter(|v| !keeping_up || !v.behind)
        .map(|v| v.estimate_bps.min(u64::from(configured)));
    let fastest = capped.clone().max()?;
    let floor = (fastest as f64 * HEALTHY_FRACTION) as u64;
    let target = capped.filter(|&e| e >= floor).min().unwrap_or(fastest);
    Some((target as u32).max(min))
}

/// Merges keyframe requests: at most one per `min_interval`; a request
/// inside the interval waits for its end instead of being lost.
pub struct KeyframeLimiter {
    min_interval: Duration,
    last: Option<Instant>,
    pending: bool,
}

impl KeyframeLimiter {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last: None,
            pending: false,
        }
    }

    pub fn request(&mut self) {
        self.pending = true;
    }

    /// True when a keyframe should be asked of the encoder now.
    pub fn poll(&mut self, now: Instant) -> bool {
        let ready = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.min_interval);
        if self.pending && ready {
            self.pending = false;
            self.last = Some(now);
            return true;
        }
        false
    }
}

/// Keyframes a viewer needs after joining: it asked (PLI, after a loss), or
/// the transport holds it until one fits its link. Every viewer receives a
/// forced keyframe, so one viewer is served right away, then less and less
/// often while it keeps needing them (1, 2, 4, 8 s apart), instead of
/// filling everyone's stream with keyframes for a link that cannot carry
/// it. After [`Recovery::QUIET`] without a need it starts over.
pub struct Recovery {
    peers: HashMap<PeerId, Backoff>,
}

struct Backoff {
    /// When this viewer may next force a keyframe.
    next: Instant,
    interval: Duration,
    last: Instant,
}

impl Recovery {
    const FIRST: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(8);
    const QUIET: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
        }
    }

    /// `needing`: the viewers that need a keyframe right now. True when one
    /// should be asked of the encoder.
    pub fn poll(&mut self, now: Instant, needing: &[PeerId]) -> bool {
        let mut due = false;
        for &peer in needing {
            let backoff = self.peers.entry(peer).or_insert(Backoff {
                next: now,
                interval: Self::FIRST,
                last: now,
            });
            if now < backoff.next {
                continue;
            }
            due = true;
            backoff.interval = if now.saturating_duration_since(backoff.last) > Self::QUIET {
                Self::FIRST
            } else if backoff.next == backoff.last {
                backoff.interval // first one: keep FIRST
            } else {
                (backoff.interval * 2).min(Self::MAX)
            };
            backoff.last = now;
            backoff.next = now + backoff.interval;
        }
        due
    }

    pub fn forget(&mut self, peer: PeerId) {
        self.peers.remove(&peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewer_that_keeps_waiting_is_asked_for_less_often() {
        let t = Instant::now();
        let s = Duration::from_secs(1);
        let mut recovery = Recovery::new();
        let slow = [PeerId(7)];
        let mut fired = Vec::new();
        for tenth in 0..400_u32 {
            let now = t + s * tenth / 10;
            if recovery.poll(now, &slow) {
                fired.push(tenth);
            }
        }
        // Right away, then +1, +2, +4, +8, +8, ... over 40 s.
        assert_eq!(fired, vec![0, 10, 30, 70, 150, 230, 310, 390]);
    }

    #[test]
    fn recovery_starts_over_after_a_quiet_while() {
        let t = Instant::now();
        let s = Duration::from_secs(1);
        let mut recovery = Recovery::new();
        let peer = [PeerId(1)];
        assert!(recovery.poll(t, &peer));
        assert!(!recovery.poll(t + s / 2, &peer)); // too soon
        assert!(recovery.poll(t + s, &peer)); // next allowed after 2 s
        assert!(!recovery.poll(t + 2 * s, &peer));
        assert!(!recovery.poll(t + 2 * s, &[]));
        // 20 s fine, then a loss: served at once, and 1 s apart again.
        assert!(recovery.poll(t + 23 * s, &peer));
        assert!(recovery.poll(t + 24 * s, &peer));
    }

    #[test]
    fn one_viewer_backing_off_does_not_delay_another() {
        let t = Instant::now();
        let s = Duration::from_secs(1);
        let mut recovery = Recovery::new();
        for i in 0..4 {
            recovery.poll(t + i * s, &[PeerId(1)]);
        }
        assert!(recovery.poll(t + 4 * s, &[PeerId(2)]));
    }

    const MBPS: u64 = 1_000_000;

    #[test]
    fn no_viewers_no_target() {
        assert_eq!(target(&[], &[]), None);
    }

    #[test]
    fn one_viewer_sets_the_bitrate() {
        assert_eq!(target(&[6 * MBPS], &[]), Some(6_000_000));
        assert_eq!(target(&[30 * MBPS], &[]), Some(10_000_000));
        assert_eq!(target(&[100_000], &[]), Some(500_000));
        // Even when it is behind: nobody else to protect.
        assert_eq!(target(&[3 * MBPS], &[0]), Some(3_000_000));
    }

    #[test]
    fn a_somewhat_slower_viewer_still_lowers_the_bitrate() {
        assert_eq!(
            target(&[20 * MBPS, 7 * MBPS, 15 * MBPS], &[]),
            Some(7_000_000)
        );
    }

    #[test]
    fn a_much_slower_viewer_is_left_out() {
        assert_eq!(
            target(&[20 * MBPS, 2 * MBPS, 15 * MBPS], &[]),
            Some(10_000_000)
        );
    }

    #[test]
    fn a_viewer_behind_is_left_out_whatever_its_estimate_says() {
        assert_eq!(
            target(&[20 * MBPS, 6 * MBPS, 15 * MBPS], &[1]),
            Some(10_000_000)
        );
    }

    #[test]
    fn every_viewer_slow_follows_the_slowest_healthy_one() {
        assert_eq!(target(&[3 * MBPS, 2 * MBPS, MBPS], &[]), Some(2_000_000));
        assert_eq!(target(&[3 * MBPS, 2 * MBPS], &[0, 1]), Some(2_000_000));
    }

    /// Estimates in bps; `behind`: indices of viewers that fell behind.
    fn target(estimates: &[u64], behind: &[usize]) -> Option<u32> {
        let viewers: Vec<ViewerRate> = estimates
            .iter()
            .enumerate()
            .map(|(i, &estimate_bps)| ViewerRate {
                estimate_bps,
                behind: behind.contains(&i),
            })
            .collect();
        encoder_target(&viewers, 10_000_000, 500_000)
    }

    #[test]
    fn keyframe_requests_are_merged() {
        let t = Instant::now();
        let ms = Duration::from_millis(1);
        let mut limiter = KeyframeLimiter::new(500 * ms);
        assert!(!limiter.poll(t));
        limiter.request();
        limiter.request();
        assert!(limiter.poll(t)); // first one goes out right away
        assert!(!limiter.poll(t + ms));
        limiter.request();
        assert!(!limiter.poll(t + 100 * ms)); // waits
        assert!(limiter.poll(t + 500 * ms)); // not lost
        assert!(!limiter.poll(t + 2000 * ms));
    }
}
