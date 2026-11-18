//! Fixed-rate frame pacing on absolute deadlines.
//!
//! Slot `n` is due at `start + n * period`, so lateness never accumulates.
//! When a whole slot has already passed, it is skipped (and counted) instead
//! of rendering a burst of frames to catch up.

use std::time::Duration;

use telemetry::{QpcDuration, QpcFrequency, QpcTimestamp, qpc_frequency, qpc_now};

/// A slot to render in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// Index of this slot since the start.
    pub index: u64,
    pub deadline: QpcTimestamp,
    /// Slots skipped right before this one because they were already over.
    pub skipped: u64,
}

#[derive(Clone, Debug)]
pub struct Pacer {
    start: QpcTimestamp,
    frequency: QpcFrequency,
    fps: u32,
    next_index: u64,
}

impl Pacer {
    /// Starts pacing at `fps` from `start`; slot 0 is due at `start`.
    ///
    /// # Panics
    /// If `fps` is zero.
    pub fn new(fps: u32, start: QpcTimestamp, frequency: QpcFrequency) -> Self {
        assert!(fps > 0, "fps must be positive");
        Self {
            start,
            frequency,
            fps,
            next_index: 0,
        }
    }

    /// Starts now on the real clock.
    pub fn start_now(fps: u32) -> Self {
        Self::new(fps, qpc_now(), qpc_frequency())
    }

    /// Deadline of slot `index`, rounded to the nearest tick.
    pub fn deadline(&self, index: u64) -> QpcTimestamp {
        let numerator = u128::from(index) * u128::from(self.frequency.ticks_per_second());
        let offset = (numerator + u128::from(self.fps) / 2) / u128::from(self.fps);
        QpcTimestamp::from_ticks(self.start.ticks() + offset as i64)
    }

    /// Returns the next slot to render at time `now`, skipping slots whose
    /// successor is already due (they can no longer be rendered on time).
    pub fn next_slot(&mut self, now: QpcTimestamp) -> Slot {
        let mut skipped = 0;
        while self.deadline(self.next_index + 1) <= now {
            self.next_index += 1;
            skipped += 1;
        }
        let slot = Slot {
            index: self.next_index,
            deadline: self.deadline(self.next_index),
            skipped,
        };
        self.next_index += 1;
        slot
    }

    /// Sleeps until `slot` is due; returns immediately if it already is.
    pub fn wait(&self, slot: &Slot) {
        let remaining = slot.deadline.duration_since(qpc_now());
        if remaining > QpcDuration::ZERO {
            let micros = remaining.as_micros(self.frequency);
            std::thread::sleep(Duration::from_secs_f64(micros / 1_000_000.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 tick per microsecond keeps the numbers readable.
    fn pacer(fps: u32) -> Pacer {
        Pacer::new(
            fps,
            QpcTimestamp::from_ticks(1_000),
            QpcFrequency::new(1_000_000).unwrap(),
        )
    }

    fn at(ticks: i64) -> QpcTimestamp {
        QpcTimestamp::from_ticks(ticks)
    }

    #[test]
    fn deadlines_do_not_drift() {
        let pacer = pacer(60);
        assert_eq!(pacer.deadline(0), at(1_000));
        assert_eq!(pacer.deadline(1), at(1_000 + 16_667));
        assert_eq!(pacer.deadline(2), at(1_000 + 33_333));
        // After one simulated hour the error is still below one tick.
        assert_eq!(pacer.deadline(60 * 3600), at(1_000 + 3_600_000_000));
    }

    #[test]
    fn on_time_slots_are_consecutive() {
        let mut pacer = pacer(100);
        for index in 0..5 {
            let now = at(1_000 + index * 10_000 - 500);
            let slot = pacer.next_slot(now);
            assert_eq!(slot.index, index as u64);
            assert_eq!(slot.skipped, 0);
            assert_eq!(slot.deadline, at(1_000 + index * 10_000));
        }
    }

    #[test]
    fn slightly_late_slot_is_rendered_not_skipped() {
        let mut pacer = pacer(100);
        // Slot 0 due at 1_000, slot 1 at 11_000; at 9_000 slot 0 is late but renderable.
        let slot = pacer.next_slot(at(9_000));
        assert_eq!((slot.index, slot.skipped), (0, 0));
    }

    #[test]
    fn fully_missed_slots_are_skipped_without_burst() {
        let mut pacer = pacer(100);
        pacer.next_slot(at(1_000));
        // A 35 ms stall: slots 1 and 2 are over, slot 3 (31_000) is the latest due.
        let slot = pacer.next_slot(at(36_000));
        assert_eq!((slot.index, slot.skipped), (3, 2));
        assert_eq!(slot.deadline, at(31_000));
        // Next one is back on schedule.
        let slot = pacer.next_slot(at(40_000));
        assert_eq!((slot.index, slot.skipped), (4, 0));
    }

    #[test]
    #[should_panic(expected = "fps must be positive")]
    fn zero_fps_is_rejected() {
        let _ = pacer(0);
    }
}
