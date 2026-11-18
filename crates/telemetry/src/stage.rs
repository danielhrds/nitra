//! Pipeline stages and a bounded per-thread recorder of stage durations.

use serde::Serialize;

use crate::clock::{QpcDuration, QpcTimestamp};

/// A measured stage of the media pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Capture,
    Convert,
    Submit,
    EncodeOutput,
    Network,
    Decode,
    Present,
}

impl Stage {
    /// All stages, in pipeline order.
    pub const ALL: [Stage; 7] = [
        Stage::Capture,
        Stage::Convert,
        Stage::Submit,
        Stage::EncodeOutput,
        Stage::Network,
        Stage::Decode,
        Stage::Present,
    ];

    /// Stable snake_case name, as used in reports.
    pub const fn name(self) -> &'static str {
        match self {
            Stage::Capture => "capture",
            Stage::Convert => "convert",
            Stage::Submit => "submit",
            Stage::EncodeOutput => "encode_output",
            Stage::Network => "network",
            Stage::Decode => "decode",
            Stage::Present => "present",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Default number of samples kept per stage: about 73 minutes at 60 fps,
/// using 2 MiB per recorded stage.
pub const DEFAULT_SAMPLE_CAPACITY: usize = 262_144;

/// Records stage durations with a fixed memory bound.
///
/// Meant to be owned by a single pipeline thread, so recording needs no
/// locking; recorders from different threads are combined with [`merge`].
/// Each stage reserves its full capacity on its first sample (one allocation
/// per stage per session) and never grows afterwards: samples beyond the
/// capacity are counted as dropped instead of stored.
///
/// [`merge`]: StageRecorder::merge
#[derive(Debug)]
pub struct StageRecorder {
    capacity_per_stage: usize,
    stages: [StageSamples; Stage::ALL.len()],
}

#[derive(Debug, Default)]
struct StageSamples {
    durations: Vec<QpcDuration>,
    dropped: u64,
}

impl StageRecorder {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_SAMPLE_CAPACITY)
    }

    pub fn with_capacity(capacity_per_stage: usize) -> Self {
        Self {
            capacity_per_stage,
            stages: Default::default(),
        }
    }

    pub fn capacity_per_stage(&self) -> usize {
        self.capacity_per_stage
    }

    /// Records the time between `start` and `end` for `stage`.
    pub fn record(&mut self, stage: Stage, start: QpcTimestamp, end: QpcTimestamp) {
        self.record_duration(stage, end.duration_since(start));
    }

    pub fn record_duration(&mut self, stage: Stage, duration: QpcDuration) {
        let capacity = self.capacity_per_stage;
        let samples = &mut self.stages[stage.index()];
        if samples.durations.len() >= capacity {
            samples.dropped += 1;
            return;
        }
        if samples.durations.capacity() == 0 {
            samples.durations.reserve_exact(capacity);
        }
        samples.durations.push(duration);
    }

    /// Moves all samples of `other` into `self`, respecting `self`'s capacity.
    pub fn merge(&mut self, other: StageRecorder) {
        for stage in Stage::ALL {
            let incoming = &other.stages[stage.index()];
            for &duration in &incoming.durations {
                self.record_duration(stage, duration);
            }
            self.stages[stage.index()].dropped += incoming.dropped;
        }
    }

    /// Stored samples for `stage`, in recording order.
    pub fn samples(&self, stage: Stage) -> &[QpcDuration] {
        &self.stages[stage.index()].durations
    }

    /// Samples for `stage` that were discarded because the capacity was reached.
    pub fn dropped(&self, stage: Stage) -> u64 {
        self.stages[stage.index()].dropped
    }
}

impl Default for StageRecorder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks(value: u64) -> QpcDuration {
        QpcDuration::from_ticks(value)
    }

    #[test]
    fn stage_names_are_unique_and_ordered() {
        let names: Vec<_> = Stage::ALL.iter().map(|stage| stage.name()).collect();
        assert_eq!(
            names,
            [
                "capture",
                "convert",
                "submit",
                "encode_output",
                "network",
                "decode",
                "present"
            ]
        );
        for (index, stage) in Stage::ALL.iter().enumerate() {
            assert_eq!(stage.index(), index);
        }
    }

    #[test]
    fn records_interval_between_timestamps() {
        let mut recorder = StageRecorder::with_capacity(4);
        recorder.record(
            Stage::Decode,
            QpcTimestamp::from_ticks(1_000),
            QpcTimestamp::from_ticks(1_250),
        );
        assert_eq!(recorder.samples(Stage::Decode), [ticks(250)]);
        assert!(recorder.samples(Stage::Capture).is_empty());
    }

    #[test]
    fn samples_beyond_capacity_are_dropped_not_stored() {
        let mut recorder = StageRecorder::with_capacity(3);
        for value in 1..=5 {
            recorder.record_duration(Stage::Capture, ticks(value));
        }
        assert_eq!(
            recorder.samples(Stage::Capture),
            [ticks(1), ticks(2), ticks(3)]
        );
        assert_eq!(recorder.dropped(Stage::Capture), 2);
        assert_eq!(recorder.dropped(Stage::Present), 0);
    }

    #[test]
    fn storage_never_grows_past_first_reservation() {
        let mut recorder = StageRecorder::with_capacity(16);
        recorder.record_duration(Stage::Submit, ticks(1));
        let reserved = recorder.samples(Stage::Submit).as_ptr();
        for value in 2..=40 {
            recorder.record_duration(Stage::Submit, ticks(value));
        }
        assert_eq!(recorder.samples(Stage::Submit).as_ptr(), reserved);
        assert_eq!(recorder.samples(Stage::Submit).len(), 16);
        assert_eq!(recorder.dropped(Stage::Submit), 24);
    }

    #[test]
    fn merge_combines_samples_and_drop_counts_within_capacity() {
        let mut capture_thread = StageRecorder::with_capacity(2);
        capture_thread.record_duration(Stage::Capture, ticks(10));
        capture_thread.record_duration(Stage::Capture, ticks(11));
        capture_thread.record_duration(Stage::Capture, ticks(12));

        let mut encode_thread = StageRecorder::with_capacity(2);
        encode_thread.record_duration(Stage::Capture, ticks(20));
        encode_thread.record_duration(Stage::Submit, ticks(30));

        capture_thread.merge(encode_thread);

        assert_eq!(
            capture_thread.samples(Stage::Capture),
            [ticks(10), ticks(11)]
        );
        // One dropped originally, plus the merged sample that did not fit.
        assert_eq!(capture_thread.dropped(Stage::Capture), 2);
        assert_eq!(capture_thread.samples(Stage::Submit), [ticks(30)]);
    }
}
