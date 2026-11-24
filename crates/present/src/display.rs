//! When presented frames reach the screen, from DXGI frame statistics.
//!
//! After each `Present`, `GetLastPresentCount` gives that present's id;
//! `GetFrameStatistics` says which present id was on screen last and the
//! QPC time of the refresh (vblank) at which it appeared. Matching the two
//! gives, per frame, when its scan-out started: DWM composition is
//! included, the panel's own response time is not.
//!
//! The statistics only describe the latest frame shown, so they are read
//! often (on every wake of the render thread); a frame shown and replaced
//! between two reads, or never shown at all, is counted as `unknown`.

use std::collections::VecDeque;

use serde::Serialize;
use telemetry::{QpcTimestamp, qpc_frequency};

use crate::FrameMeta;

/// Presents waiting to be seen on screen (more means older ones are lost).
const MAX_PENDING: usize = 16;
/// Displayed frames kept: about 18 minutes at 60 fps.
const MAX_FRAMES: usize = 1 << 16;

/// A frame and the refresh at which it reached the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayedFrame {
    pub frame_id: u64,
    pub decoded_at: QpcTimestamp,
    /// Right after the `Present` call returned.
    pub presented_at: QpcTimestamp,
    /// The vblank at which the frame started to be scanned out.
    pub displayed_at: QpcTimestamp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DisplayCounters {
    /// New frames matched to the refresh they were shown at.
    pub matched: u64,
    /// Of `matched`: refresh time derived from a later vblank and the
    /// measured refresh period (the statistics' sync point was not the
    /// frame's own refresh).
    pub estimated: u64,
    /// New frames never matched: shown and replaced between two reads,
    /// not shown at all, or still pending at the end.
    pub unknown: u64,
    /// Matches dropped because they made no sense (shown before presented).
    pub implausible: u64,
    /// Frames matched beyond the storage bound (counted, not kept).
    pub frames_dropped: u64,
    /// `GetFrameStatistics` calls that failed (e.g. disjoint statistics).
    pub statistics_failed: u64,
}

#[derive(Clone, Debug, Default)]
pub struct DisplayReport {
    pub counters: DisplayCounters,
    pub frames: Vec<DisplayedFrame>,
    /// Refresh period measured from the statistics' vblank counter.
    pub refresh_period_us: Option<f64>,
}

/// Spacing kept between two reads of [`Cadence`]: a few seconds at 60 fps;
/// more is not kept (the owner reads it every second).
const MAX_INTERVALS: usize = 1024;

/// How new frames were spaced on screen since the owner last read it: the
/// time from one frame being taken for the screen to the next. Steady
/// 60 fps is ~16.7 ms each; a frame lost on the way shows the one before
/// it twice as long (~33 ms), which the eye sees as a hitch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cadence {
    pub intervals_us: Vec<u32>,
}

impl Cadence {
    pub(crate) fn extend(&mut self, intervals_us: &[u32]) {
        let room = MAX_INTERVALS.saturating_sub(self.intervals_us.len());
        self.intervals_us
            .extend_from_slice(&intervals_us[..intervals_us.len().min(room)]);
    }

    pub fn take(&mut self) -> Self {
        Self {
            intervals_us: std::mem::take(&mut self.intervals_us),
        }
    }

    /// Percentiles of the spacing and the share of long frames. `None`
    /// without spacing to describe.
    pub fn summary(&self) -> Option<CadenceSummary> {
        if self.intervals_us.is_empty() {
            return None;
        }
        let mut sorted = self.intervals_us.clone();
        sorted.sort_unstable();
        let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
        let median = at(0.5);
        let long = sorted
            .iter()
            .filter(|&&interval| f64::from(interval) > f64::from(median) * LONG_FACTOR)
            .count();
        Some(CadenceSummary {
            frames: sorted.len(),
            p50_ms: median as f32 / 1000.0,
            p95_ms: at(0.95) as f32 / 1000.0,
            max_ms: sorted[sorted.len() - 1] as f32 / 1000.0,
            long_share: long as f32 / sorted.len() as f32,
        })
    }
}

/// A frame is "long" past this many times the median spacing: a frame
/// missing in between (2x), not the refresh grid's own unevenness (a 60 fps
/// frame stays 13.9 or 20.8 ms on 144 Hz, 1.25x the median at most).
const LONG_FACTOR: f64 = 1.5;

/// A [`Cadence`] in a few numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CadenceSummary {
    /// Spacings described.
    pub frames: usize,
    pub p50_ms: f32,
    pub p95_ms: f32,
    pub max_ms: f32,
    /// Share of frames that stayed past 1.5x the median (0..1): hitches.
    pub long_share: f32,
}

/// Spacing of new frames on screen, from the swap chain's frame-latency
/// waitable object. With a frame latency of 1 it is signaled when the
/// compositor takes the frame presented last, at the refresh that shows it:
/// the time between two signals is how long a frame stayed on screen.
/// The frame statistics would be exact, but in a window they follow the
/// main display: on a second monitor at another refresh rate ~1% of the
/// frames matched (measured), on the main one ~90%. Against them, on the
/// main monitor, this gives the same median and 95th percentile (16.3 vs
/// 15.6 ms, 33.2 vs 34.4 ms) but also a few spacings too short: a signal
/// seen late, then the next on time.
#[derive(Debug, Default)]
pub(crate) struct ShownSpacing {
    /// A new frame was presented and its signal has not come yet.
    awaiting: bool,
    /// When the previous new frame was taken.
    last: Option<QpcTimestamp>,
    intervals_us: Vec<u32>,
}

impl ShownSpacing {
    /// After a `Present`: `fresh` for a new frame, else a redraw of the
    /// last one (which breaks the spacing: its signal is not a new frame).
    pub fn presented(&mut self, fresh: bool) {
        self.awaiting = fresh;
        if !fresh {
            self.last = None;
        }
    }

    /// The waitable was signaled. `on_time`: seen as it happened (not
    /// found later by polling), so `at` is its time.
    pub fn ready(&mut self, at: QpcTimestamp, on_time: bool) {
        if !std::mem::take(&mut self.awaiting) {
            return;
        }
        if !on_time {
            self.last = None;
            return;
        }
        if let Some(previous) = self.last
            && at > previous
            && self.intervals_us.len() < MAX_INTERVALS
        {
            let micros = at.duration_since(previous).as_micros(qpc_frequency());
            self.intervals_us.push(micros.round() as u32);
        }
        self.last = Some(at);
    }

    pub fn take(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.intervals_us)
    }
}

/// The fields of `DXGI_FRAME_STATISTICS` used here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Statistics {
    /// Id of the last present shown.
    pub present_count: u32,
    /// Vblank count at which that present was shown.
    pub present_refresh_count: u32,
    /// Vblank count at which `sync_qpc_time` was sampled.
    pub sync_refresh_count: u32,
    pub sync_qpc_time: i64,
}

struct Pending {
    present_id: u32,
    /// `None` for a redraw of the frame already shown.
    frame: Option<FrameMeta>,
    presented_at: QpcTimestamp,
}

/// Matches presents with frame statistics. Pure bookkeeping, no DXGI calls.
#[derive(Default)]
pub(crate) struct DisplayTracker {
    pending: VecDeque<Pending>,
    frames: Vec<DisplayedFrame>,
    counters: DisplayCounters,
    /// First vblank sample (count, QPC), the base of the period estimate.
    first_sync: Option<(u32, i64)>,
    period_ticks: Option<f64>,
}

/// `a <= b` for wrapping 32-bit present and refresh counters.
fn not_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

impl DisplayTracker {
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Records a present (`frame` is `None` for a redraw).
    pub fn presented(
        &mut self,
        present_id: u32,
        frame: Option<FrameMeta>,
        presented_at: QpcTimestamp,
    ) {
        if self.pending.len() == MAX_PENDING
            && let Some(lost) = self.pending.pop_front()
            && lost.frame.is_some()
        {
            self.counters.unknown += 1;
        }
        self.pending.push_back(Pending {
            present_id,
            frame,
            presented_at,
        });
    }

    pub fn statistics_failed(&mut self) {
        self.counters.statistics_failed += 1;
    }

    pub fn observe(&mut self, statistics: Statistics) {
        if statistics.sync_qpc_time <= 0 {
            // Nothing shown yet.
            return;
        }
        self.update_period(statistics);
        let lag = statistics
            .sync_refresh_count
            .wrapping_sub(statistics.present_refresh_count);
        let (displayed_at, estimated) = if lag == 0 {
            (statistics.sync_qpc_time, false)
        } else if let (Some(period), true) = (self.period_ticks, (lag as i32) > 0) {
            (
                statistics.sync_qpc_time - (f64::from(lag) * period).round() as i64,
                true,
            )
        } else {
            // Cannot place the frame's refresh in time yet; the presents stay
            // pending for a later sample.
            return;
        };
        while let Some(front) = self.pending.front() {
            if !not_after(front.present_id, statistics.present_count) {
                break;
            }
            let entry = self.pending.pop_front().expect("front exists");
            let Some(frame) = entry.frame else { continue };
            if entry.present_id != statistics.present_count {
                self.counters.unknown += 1;
                continue;
            }
            let displayed_at = QpcTimestamp::from_ticks(displayed_at);
            if displayed_at < entry.presented_at {
                self.counters.implausible += 1;
                continue;
            }
            self.counters.matched += 1;
            if estimated {
                self.counters.estimated += 1;
            }
            if self.frames.len() < MAX_FRAMES {
                self.frames.push(DisplayedFrame {
                    frame_id: frame.frame_id,
                    decoded_at: frame.decoded_at,
                    presented_at: entry.presented_at,
                    displayed_at,
                });
            } else {
                self.counters.frames_dropped += 1;
            }
        }
    }

    /// Refresh period from the first and the latest vblank samples (a long
    /// baseline, so QPC sampling jitter averages out).
    fn update_period(&mut self, statistics: Statistics) {
        let sample = (statistics.sync_refresh_count, statistics.sync_qpc_time);
        match self.first_sync {
            Some((count, time)) => {
                let refreshes = sample.0.wrapping_sub(count) as i32;
                if refreshes < 0 || sample.1 < time {
                    // Counter reset (mode change): start over.
                    self.first_sync = Some(sample);
                    self.period_ticks = None;
                } else if refreshes > 0 {
                    self.period_ticks = Some((sample.1 - time) as f64 / f64::from(refreshes));
                }
            }
            None => self.first_sync = Some(sample),
        }
    }

    pub fn finish(mut self) -> DisplayReport {
        self.counters.unknown += self
            .pending
            .iter()
            .filter(|entry| entry.frame.is_some())
            .count() as u64;
        let frequency = qpc_frequency().ticks_per_second() as f64;
        DisplayReport {
            counters: self.counters,
            frames: self.frames,
            refresh_period_us: self.period_ticks.map(|ticks| ticks * 1e6 / frequency),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(frame_id: u64) -> Option<FrameMeta> {
        Some(FrameMeta {
            frame_id,
            captured_at: QpcTimestamp::from_ticks(0),
            decoded_at: QpcTimestamp::from_ticks(frame_id as i64 * 1000),
        })
    }

    fn stats(present: u32, present_refresh: u32, sync_refresh: u32, time: i64) -> Statistics {
        Statistics {
            present_count: present,
            present_refresh_count: present_refresh,
            sync_refresh_count: sync_refresh,
            sync_qpc_time: time,
        }
    }

    fn at(ticks: i64) -> QpcTimestamp {
        QpcTimestamp::from_ticks(ticks)
    }

    #[test]
    fn matches_each_present_with_its_refresh() {
        let mut tracker = DisplayTracker::default();
        tracker.presented(1, meta(10), at(100));
        tracker.observe(stats(0, 0, 5, 50));
        assert!(tracker.has_pending());
        tracker.observe(stats(1, 6, 6, 150));
        tracker.presented(2, meta(11), at(200));
        tracker.observe(stats(2, 7, 7, 260));
        assert!(!tracker.has_pending());
        let report = tracker.finish();
        assert_eq!(report.counters.matched, 2);
        assert_eq!(report.counters.unknown, 0);
        assert_eq!(
            report
                .frames
                .iter()
                .map(|f| (f.frame_id, f.displayed_at))
                .collect::<Vec<_>>(),
            [(10, at(150)), (11, at(260))]
        );
    }

    #[test]
    fn shown_spacing_measures_between_signals_of_new_frames() {
        let ticks = |us: i64| at(us * qpc_frequency().ticks_per_second() as i64 / 1_000_000);
        let mut spacing = ShownSpacing::default();
        // A signal without a new frame before it is ignored.
        spacing.ready(ticks(0), true);
        for shown in [10_000, 26_667, 47_500] {
            spacing.presented(true);
            spacing.ready(ticks(shown), true);
        }
        assert_eq!(spacing.take(), [16_667, 20_833]);
        // A redraw, then a signal found late: neither gives a spacing, and
        // the next one starts over.
        spacing.presented(false);
        spacing.ready(ticks(50_000), true);
        spacing.presented(true);
        spacing.ready(ticks(60_000), false);
        spacing.presented(true);
        spacing.ready(ticks(70_000), true);
        assert!(spacing.take().is_empty());
        spacing.presented(true);
        spacing.ready(ticks(83_889), true);
        assert_eq!(spacing.take(), [13_889]);
    }

    #[test]
    fn summary_gives_percentiles_and_long_frames() {
        // 60 fps on 144 Hz (13.9 and 20.8 ms: not long), one frame missing
        // (34.7 ms: long).
        let cadence = Cadence {
            intervals_us: vec![13_889, 20_833, 13_889, 20_833, 34_722],
        };
        let summary = cadence.summary().unwrap();
        assert_eq!(summary.frames, 5);
        assert!((summary.p50_ms - 20.833).abs() < 0.01);
        assert!((summary.max_ms - 34.722).abs() < 0.01);
        assert!((summary.long_share - 0.2).abs() < 1e-6);
        assert_eq!(Cadence::default().summary(), None);
    }

    #[test]
    fn cadence_keeps_a_bounded_window() {
        let mut cadence = Cadence::default();
        cadence.extend(&vec![16_667; MAX_INTERVALS + 10]);
        assert_eq!(cadence.intervals_us.len(), MAX_INTERVALS);
        let taken = cadence.take();
        assert_eq!(taken.intervals_us.len(), MAX_INTERVALS);
        assert!(cadence.intervals_us.is_empty());
    }

    #[test]
    fn a_frame_skipped_between_reads_is_unknown() {
        let mut tracker = DisplayTracker::default();
        tracker.presented(1, meta(1), at(100));
        tracker.presented(2, meta(2), at(110));
        tracker.observe(stats(2, 9, 9, 200));
        let report = tracker.finish();
        assert_eq!(report.counters.matched, 1);
        assert_eq!(report.counters.unknown, 1);
        assert_eq!(report.frames[0].frame_id, 2);
    }

    #[test]
    fn redraws_are_not_counted() {
        let mut tracker = DisplayTracker::default();
        tracker.presented(1, None, at(100));
        tracker.presented(2, meta(5), at(110));
        tracker.observe(stats(1, 3, 3, 150));
        tracker.observe(stats(2, 4, 4, 200));
        let report = tracker.finish();
        assert_eq!(report.counters.matched, 1);
        assert_eq!(report.counters.unknown, 0);
    }

    #[test]
    fn a_later_sync_point_is_corrected_by_the_refresh_period() {
        let mut tracker = DisplayTracker::default();
        // Period: 100 ticks per refresh, from two samples.
        tracker.observe(stats(0, 0, 10, 1_000));
        tracker.observe(stats(0, 0, 12, 1_200));
        tracker.presented(1, meta(1), at(1_250));
        // Shown at refresh 13, sampled at refresh 15 (time 1500).
        tracker.observe(stats(1, 13, 15, 1_500));
        let report = tracker.finish();
        assert_eq!(report.counters.matched, 1);
        assert_eq!(report.counters.estimated, 1);
        assert_eq!(report.frames[0].displayed_at, at(1_300));
    }

    #[test]
    fn without_a_period_a_lagging_sample_waits() {
        let mut tracker = DisplayTracker::default();
        tracker.presented(1, meta(1), at(100));
        tracker.observe(stats(1, 3, 5, 500));
        assert!(tracker.has_pending());
    }

    #[test]
    fn shown_before_presented_is_rejected() {
        let mut tracker = DisplayTracker::default();
        tracker.presented(1, meta(1), at(500));
        tracker.observe(stats(1, 2, 2, 400));
        let report = tracker.finish();
        assert_eq!(report.counters.implausible, 1);
        assert_eq!(report.counters.matched, 0);
    }

    #[test]
    fn pending_presents_are_bounded() {
        let mut tracker = DisplayTracker::default();
        for id in 1..=(MAX_PENDING as u32 + 4) {
            tracker.presented(id, meta(u64::from(id)), at(i64::from(id)));
        }
        assert_eq!(tracker.pending.len(), MAX_PENDING);
        let report = tracker.finish();
        assert_eq!(report.counters.unknown, MAX_PENDING as u64 + 4);
    }

    #[test]
    fn counters_wrap_around() {
        assert!(not_after(u32::MAX, 0));
        assert!(!not_after(0, u32::MAX));
        let mut tracker = DisplayTracker::default();
        tracker.presented(u32::MAX, meta(1), at(100));
        tracker.presented(0, meta(2), at(110));
        tracker.observe(stats(0, 1, 1, 200));
        let report = tracker.finish();
        assert_eq!(report.counters.matched, 1);
        assert_eq!(report.counters.unknown, 1);
    }
}
