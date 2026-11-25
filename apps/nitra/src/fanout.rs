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

/// Share of a viewer's bandwidth estimate the encoder is given. The pacer
/// sends ~10% above the estimate and keyframes come in bursts: at 100% a
/// link at its limit filled its queue, dropped keyframes and asked for new
/// ones (41-75 a minute at 6 Mbps for a 10 Mbps stream, measured).
pub const ENCODER_HEADROOM: f64 = 0.85;

/// The bitrate rises only after the target stayed higher this long...
const RISE_AFTER: Duration = Duration::from_secs(2);
/// ...and by at most this share at a time. It falls at once.
const RISE_STEP: f64 = 0.15;
/// Changes smaller than this share are not worth reconfiguring the encoder.
const MIN_CHANGE: f64 = 0.10;

/// One viewer as the bitrate policy sees it.
#[derive(Clone, Copy, Debug)]
pub struct ViewerRate {
    pub estimate_bps: u64,
    /// Fell behind within [`BEHIND_HOLD`].
    pub behind: bool,
}

/// The encoder bitrate for these viewers: among those keeping up (all of
/// them, if none is), the slowest that can still take at least half of what
/// the fastest one takes ([`ENCODER_HEADROOM`] of each estimate, capped at
/// `configured`), never below `min`. `None` without viewers.
pub fn encoder_target(viewers: &[ViewerRate], configured: u32, min: u32) -> Option<u32> {
    let keeping_up = viewers.iter().any(|v| !v.behind);
    let capped = viewers
        .iter()
        .filter(|v| !keeping_up || !v.behind)
        .map(|v| ((v.estimate_bps as f64 * ENCODER_HEADROOM) as u64).min(u64::from(configured)));
    let fastest = capped.clone().max()?;
    let floor = (fastest as f64 * HEALTHY_FRACTION) as u64;
    let target = capped.filter(|&e| e >= floor).min().unwrap_or(fastest);
    Some((target as u32).max(min))
}

/// Steadies the encoder bitrate: it follows a lower target at once and a
/// higher one only once it has held for [`RISE_AFTER`], in steps of at most
/// [`RISE_STEP`]. An estimate swinging around the link's limit reconfigured
/// the encoder ~75 times a minute (measured).
pub struct BitrateFollower {
    current: u32,
    /// Since when the target has been higher than `current`.
    higher_since: Option<Instant>,
}

impl BitrateFollower {
    pub fn new(start: u32) -> Self {
        Self {
            current: start,
            higher_since: None,
        }
    }

    /// The new encoder bitrate for `target`, if it should change now.
    pub fn update(&mut self, target: u32, now: Instant) -> Option<u32> {
        let current = f64::from(self.current);
        if f64::from(target) < current * (1.0 - MIN_CHANGE) {
            self.higher_since = None;
            self.current = target;
            return Some(target);
        }
        if f64::from(target) <= current * (1.0 + MIN_CHANGE) {
            self.higher_since = None;
            return None;
        }
        let since = *self.higher_since.get_or_insert(now);
        if now.saturating_duration_since(since) < RISE_AFTER {
            return None;
        }
        let step = (current * (1.0 + RISE_STEP)).round() as u32;
        self.current = target.min(step);
        // The next step waits again.
        self.higher_since = Some(now);
        Some(self.current)
    }
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

    /// A request is waiting for the end of the interval.
    pub fn is_pending(&self) -> bool {
        self.pending
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
        assert_eq!(target(&[6 * MBPS], &[]), Some(5_100_000));
        assert_eq!(target(&[30 * MBPS], &[]), Some(10_000_000));
        assert_eq!(target(&[100_000], &[]), Some(500_000));
        // Even when it is behind: nobody else to protect.
        assert_eq!(target(&[3 * MBPS], &[0]), Some(2_550_000));
    }

    #[test]
    fn a_somewhat_slower_viewer_still_lowers_the_bitrate() {
        assert_eq!(
            target(&[20 * MBPS, 7 * MBPS, 15 * MBPS], &[]),
            Some(5_950_000)
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
        assert_eq!(target(&[3 * MBPS, 2 * MBPS, MBPS], &[]), Some(1_700_000));
        assert_eq!(target(&[3 * MBPS, 2 * MBPS], &[0, 1]), Some(1_700_000));
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
    fn the_follower_falls_at_once_and_rises_in_steady_steps() {
        let t = Instant::now();
        let s = Duration::from_secs(1);
        let mut follower = BitrateFollower::new(10_000_000);
        // A good link: the target stays at the configured rate, no change.
        assert_eq!(follower.update(10_000_000, t), None);
        // Small swings are ignored.
        assert_eq!(follower.update(9_400_000, t + s), None);
        // A drop is followed at once.
        assert_eq!(follower.update(5_000_000, t + 2 * s), Some(5_000_000));
        // A rise waits two seconds, then goes up at most 15% a step.
        assert_eq!(follower.update(10_000_000, t + 3 * s), None);
        assert_eq!(follower.update(10_000_000, t + 4 * s), None);
        assert_eq!(follower.update(10_000_000, t + 5 * s), Some(5_750_000));
        assert_eq!(follower.update(10_000_000, t + 6 * s), None);
        assert_eq!(follower.update(10_000_000, t + 7 * s), Some(6_612_500));
        // A target that falls back meanwhile restarts the wait.
        assert_eq!(follower.update(6_700_000, t + 8 * s), None);
        assert_eq!(follower.update(10_000_000, t + 9 * s), None);
        assert_eq!(follower.update(10_000_000, t + 10 * s), None);
        assert_eq!(follower.update(10_000_000, t + 11 * s), Some(7_604_375));
        // Never above the target.
        let mut near = BitrateFollower::new(9_000_000);
        assert_eq!(near.update(10_000_000, t), None);
        assert_eq!(near.update(10_000_000, t + 2 * s), Some(10_000_000));
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
