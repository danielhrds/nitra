//! When each frame should be shown, from the time it was captured: a short
//! hold that evens out frames arriving unevenly (the network, the decoder,
//! an uneven source), at the cost of a little delay.
//!
//! Each frame carries its media time (90 kHz, the sender's clock). Its
//! "lateness" is the local arrival time minus the media time: a constant
//! plus the jitter. The smallest lateness over the last couple of seconds
//! is the base (the clocks' offset plus the fastest trip), recomputed as it
//! slides so the two PCs' clocks drifting apart cannot build up a delay. A
//! frame is due at media time + base + hold: frames that came early wait,
//! frames that came late go at once.

use std::collections::VecDeque;

/// How the viewer trades delay for smoothness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Smoothing {
    /// Show each frame as soon as it arrives (no hold).
    Minimum,
    /// Always hold [`MAX_HOLD_FRAMES`] frames.
    Smooth,
    /// Hold as much as the recent jitter needs, up to [`MAX_HOLD_FRAMES`]:
    /// nothing on a steady link.
    #[default]
    Auto,
}

impl Smoothing {
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Minimum => 0,
            Self::Smooth => 1,
            Self::Auto => 2,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Minimum,
            1 => Self::Smooth,
            _ => Self::Auto,
        }
    }
}

/// The most a frame is held, in frame periods (~33 ms at 60 fps).
pub const MAX_HOLD_FRAMES: f64 = 2.0;
/// Arrivals the base and the jitter are taken from (~2 s at 60 fps).
const WINDOW: usize = 120;
/// Media clock rate (RTP video).
const MEDIA_HZ: f64 = 90_000.0;
/// Frame period until the media times give one (60 fps).
const DEFAULT_PERIOD_TICKS_90K: f64 = MEDIA_HZ / 60.0;
/// The automatic hold comes down this fast once the jitter is gone (ms per
/// second of frames): rising at once, falling slowly.
const AUTO_DECAY_MS_PER_S: f64 = 2.0;
/// The automatic hold covers this share of recent arrivals.
const AUTO_PERCENTILE: f64 = 0.95;

/// Schedules frames by their media time. Times are QPC ticks.
#[derive(Debug)]
pub struct Pacer {
    ticks_per_second: f64,
    /// (media time in QPC ticks, lateness in QPC ticks), oldest first.
    arrivals: VecDeque<(i64, i64)>,
    /// Media time of the last arrival (90 kHz), for the frame period.
    last_media: Option<u64>,
    /// Recent media-time steps (90 kHz), for the frame period.
    steps: VecDeque<u64>,
    /// The automatic hold now, in QPC ticks.
    auto_hold: f64,
    /// Arrival time of the last frame, for the decay.
    last_arrival: Option<i64>,
}

impl Pacer {
    pub fn new(ticks_per_second: i64) -> Self {
        Self {
            ticks_per_second: ticks_per_second as f64,
            arrivals: VecDeque::with_capacity(WINDOW + 1),
            last_media: None,
            steps: VecDeque::with_capacity(WINDOW + 1),
            auto_hold: 0.0,
            last_arrival: None,
        }
    }

    fn media_ticks(&self, media_90k: u64) -> i64 {
        (media_90k as f64 * self.ticks_per_second / MEDIA_HZ) as i64
    }

    /// A frame with media time `media_90k` arrived at `arrived` (QPC).
    pub fn arrived(&mut self, media_90k: u64, arrived: i64) {
        if let Some(last) = self.last_media
            && media_90k > last
        {
            self.steps.push_back(media_90k - last);
            if self.steps.len() > WINDOW {
                self.steps.pop_front();
            }
        }
        self.last_media = Some(media_90k);
        let media = self.media_ticks(media_90k);
        self.arrivals.push_back((media, arrived - media));
        if self.arrivals.len() > WINDOW {
            self.arrivals.pop_front();
        }
        // The automatic hold: up at once to the jitter's 95th percentile,
        // down slowly, never past the cap.
        let elapsed = self.last_arrival.map_or(0.0, |last| {
            (arrived - last).max(0) as f64 / self.ticks_per_second
        });
        self.last_arrival = Some(arrived);
        let decayed =
            self.auto_hold - AUTO_DECAY_MS_PER_S / 1000.0 * elapsed * self.ticks_per_second;
        self.auto_hold = decayed
            .max(self.jitter_percentile(AUTO_PERCENTILE))
            .min(self.max_hold());
    }

    /// The frame period from the media times (median step).
    fn period_ticks(&self) -> f64 {
        let mut steps: Vec<u64> = self.steps.iter().copied().collect();
        if steps.is_empty() {
            return DEFAULT_PERIOD_TICKS_90K * self.ticks_per_second / MEDIA_HZ;
        }
        steps.sort_unstable();
        steps[steps.len() / 2] as f64 * self.ticks_per_second / MEDIA_HZ
    }

    fn max_hold(&self) -> f64 {
        MAX_HOLD_FRAMES * self.period_ticks()
    }

    fn base(&self) -> Option<i64> {
        self.arrivals.iter().map(|&(_, late)| late).min()
    }

    fn jitter_percentile(&self, q: f64) -> f64 {
        let Some(base) = self.base() else {
            return 0.0;
        };
        let mut jitter: Vec<i64> = self.arrivals.iter().map(|&(_, late)| late - base).collect();
        jitter.sort_unstable();
        jitter[((jitter.len() - 1) as f64 * q).round() as usize] as f64
    }

    /// How long frames are held in `mode` now, in QPC ticks.
    pub fn hold(&self, mode: Smoothing) -> i64 {
        match mode {
            Smoothing::Minimum => 0,
            Smoothing::Smooth => self.max_hold() as i64,
            Smoothing::Auto => self.auto_hold as i64,
        }
    }

    /// When the frame with media time `media_90k` should be shown (QPC), or
    /// `None` before any arrival.
    pub fn due(&self, media_90k: u64, mode: Smoothing) -> Option<i64> {
        Some(self.media_ticks(media_90k) + self.base()? + self.hold(mode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 tick per microsecond.
    const TPS: i64 = 1_000_000;
    /// 60 fps in 90 kHz and (rounded) in microseconds.
    const STEP_90K: u64 = 1500;
    const STEP_US: i64 = 16_667;

    /// When frame `i` was sent, in microseconds (exact, like the pacer).
    fn sent(i: u64) -> i64 {
        (i * STEP_90K) as i64 * 1_000_000 / 90_000
    }

    fn feed(pacer: &mut Pacer, frames: u64, jitter_us: impl Fn(u64) -> i64) {
        for i in 0..frames {
            let media = i * STEP_90K;
            pacer.arrived(media, 5_000 + sent(i) + jitter_us(i));
        }
    }

    #[test]
    fn steady_frames_are_due_as_they_arrive_in_auto_and_minimum() {
        let mut pacer = Pacer::new(TPS);
        feed(&mut pacer, 120, |_| 0);
        assert_eq!(pacer.hold(Smoothing::Auto), 0);
        assert_eq!(pacer.hold(Smoothing::Minimum), 0);
        // The next frame is due when it would arrive on time.
        let due = pacer.due(120 * STEP_90K, Smoothing::Auto).unwrap();
        assert!((due - (5_000 + sent(120))).abs() <= 1, "{due}");
    }

    #[test]
    fn smooth_holds_two_frames() {
        let mut pacer = Pacer::new(TPS);
        feed(&mut pacer, 60, |_| 0);
        let hold = pacer.hold(Smoothing::Smooth);
        assert!((hold - 2 * STEP_US).abs() <= 2, "{hold}");
    }

    #[test]
    fn auto_rises_with_jitter_up_to_the_cap_and_falls_slowly() {
        let mut pacer = Pacer::new(TPS);
        // Every fourth frame 12 ms late.
        feed(&mut pacer, 120, |i| if i % 4 == 0 { 12_000 } else { 0 });
        let hold = pacer.hold(Smoothing::Auto);
        assert!((hold - 12_000).abs() <= 1, "{hold}");
        // Jitter beyond two frames: capped.
        let mut wild = Pacer::new(TPS);
        feed(&mut wild, 120, |i| if i % 3 == 0 { 80_000 } else { 0 });
        let hold = wild.hold(Smoothing::Auto);
        assert!((hold - 2 * STEP_US).abs() <= 2, "{hold}");
        // Steady again for 3 s: the window forgets the jitter after 2 s, then
        // the hold comes down ~2 ms a second, not at once.
        let start = 120;
        for i in start..start + 180 {
            pacer.arrived(i * STEP_90K, 5_000 + sent(i));
        }
        let hold = pacer.hold(Smoothing::Auto);
        assert!(hold > 8_000 && hold < 11_500, "{hold}");
    }

    #[test]
    fn a_late_frame_is_due_in_the_past_and_an_early_one_waits() {
        let mut pacer = Pacer::new(TPS);
        feed(&mut pacer, 120, |i| if i % 4 == 0 { 12_000 } else { 0 });
        let on_time = 5_000 + sent(120);
        let due = pacer.due(120 * STEP_90K, Smoothing::Auto).unwrap();
        // Held by the jitter (12 ms) after the on-time arrival.
        assert!((due - (on_time + 12_000)).abs() <= 2, "{due}");
    }

    #[test]
    fn clocks_drifting_apart_do_not_build_up_a_delay() {
        let mut pacer = Pacer::new(TPS);
        // The receiver's clock runs 0.1% fast: lateness grows ~1 ms a second.
        for i in 0..600_u64 {
            let at = sent(i);
            pacer.arrived(i * STEP_90K, 5_000 + at + at / 1000);
        }
        let i = 600_u64;
        let at = sent(i);
        let arrival = 5_000 + at + at / 1000;
        let due = pacer.due(i * STEP_90K, Smoothing::Auto).unwrap();
        // Due within ~2 s of drift (2 ms) of its arrival, not 10 s of it.
        assert!((arrival - due).abs() <= 2_500, "{}", arrival - due);
    }

    #[test]
    fn modes_survive_a_round_trip() {
        for mode in [Smoothing::Minimum, Smoothing::Smooth, Smoothing::Auto] {
            assert_eq!(Smoothing::from_u8(mode.to_u8()), mode);
        }
    }
}
