//! Summary statistics over stage durations.
//!
//! Percentiles use the nearest-rank method (no interpolation): the p-th
//! percentile is the smallest sample such that at least p% of the samples are
//! less than or equal to it. Keep this definition stable so that reports from
//! different runs stay comparable.

use serde::Serialize;

use crate::clock::{QpcDuration, QpcFrequency, ticks_to_micros};

/// Statistics of a non-empty set of durations, in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Summary {
    pub count: u64,
    pub mean_us: f64,
    pub min_us: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
}

impl Summary {
    /// Summarizes `samples` in any order; `None` when there are no samples.
    pub fn from_durations(samples: &[QpcDuration], frequency: QpcFrequency) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }

        let mut sorted: Vec<u64> = samples.iter().map(|duration| duration.ticks()).collect();
        sorted.sort_unstable();

        let total: u128 = sorted.iter().map(|&ticks| u128::from(ticks)).sum();
        let mean_ticks = total as f64 / sorted.len() as f64;
        let micros = |ticks: u64| ticks_to_micros(ticks as f64, frequency);

        Some(Self {
            count: sorted.len() as u64,
            mean_us: ticks_to_micros(mean_ticks, frequency),
            min_us: micros(sorted[0]),
            p50_us: micros(nearest_rank(&sorted, 50)),
            p95_us: micros(nearest_rank(&sorted, 95)),
            p99_us: micros(nearest_rank(&sorted, 99)),
            max_us: micros(sorted[sorted.len() - 1]),
        })
    }
}

/// Nearest-rank percentile of an ascending, non-empty slice.
fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    debug_assert!(!sorted.is_empty());
    debug_assert!((1..=100).contains(&percentile));
    // rank = ceil(percentile / 100 * n), 1-based, computed in integers.
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One tick per microsecond, so tick values read directly as microseconds.
    const ONE_MHZ: QpcFrequency = match QpcFrequency::new(1_000_000) {
        Some(frequency) => frequency,
        None => unreachable!(),
    };

    fn durations(values: impl IntoIterator<Item = u64>) -> Vec<QpcDuration> {
        values.into_iter().map(QpcDuration::from_ticks).collect()
    }

    #[test]
    fn empty_input_has_no_summary() {
        assert_eq!(Summary::from_durations(&[], ONE_MHZ), None);
    }

    #[test]
    fn one_to_hundred() {
        let summary = Summary::from_durations(&durations(1..=100), ONE_MHZ).unwrap();
        assert_eq!(
            summary,
            Summary {
                count: 100,
                mean_us: 50.5,
                min_us: 1.0,
                p50_us: 50.0,
                p95_us: 95.0,
                p99_us: 99.0,
                max_us: 100.0,
            }
        );
    }

    #[test]
    fn single_sample_is_every_statistic() {
        let summary = Summary::from_durations(&durations([42]), ONE_MHZ).unwrap();
        assert_eq!(summary.count, 1);
        for value in [
            summary.mean_us,
            summary.min_us,
            summary.p50_us,
            summary.p95_us,
            summary.p99_us,
            summary.max_us,
        ] {
            assert_eq!(value, 42.0);
        }
    }

    #[test]
    fn order_of_samples_does_not_matter() {
        let ascending = Summary::from_durations(&durations(1..=100), ONE_MHZ);
        let shuffled: Vec<u64> = (1..=100).map(|value| (value * 37) % 100 + 1).collect();
        let shuffled = Summary::from_durations(&durations(shuffled), ONE_MHZ);
        assert_eq!(ascending, shuffled);
    }

    #[test]
    fn tail_percentiles_catch_rare_spikes() {
        // 990 frames at 1 ms and 10 spikes at 50 ms: p99 must not hide the spikes.
        let samples =
            durations(std::iter::repeat_n(1_000, 990).chain(std::iter::repeat_n(50_000, 10)));
        let summary = Summary::from_durations(&samples, ONE_MHZ).unwrap();
        assert_eq!(summary.p50_us, 1_000.0);
        assert_eq!(summary.p95_us, 1_000.0);
        assert_eq!(summary.p99_us, 1_000.0);
        assert_eq!(summary.max_us, 50_000.0);
        assert_eq!(summary.mean_us, 1_490.0);

        // One more spike pushes p99 onto the spikes.
        let mut samples = samples;
        samples.push(QpcDuration::from_ticks(50_000));
        let summary = Summary::from_durations(&samples, ONE_MHZ).unwrap();
        assert_eq!(summary.p99_us, 50_000.0);
    }

    #[test]
    fn nearest_rank_small_sets() {
        assert_eq!(nearest_rank(&[7], 1), 7);
        assert_eq!(nearest_rank(&[1, 2], 50), 1);
        assert_eq!(nearest_rank(&[1, 2], 51), 2);
        assert_eq!(nearest_rank(&[1, 2, 3, 4], 100), 4);
    }

    #[test]
    fn conversion_uses_frequency() {
        let ten_mhz = QpcFrequency::new(10_000_000).unwrap();
        let summary = Summary::from_durations(&durations([10, 30]), ten_mhz).unwrap();
        assert_eq!(summary.min_us, 1.0);
        assert_eq!(summary.max_us, 3.0);
        assert_eq!(summary.mean_us, 2.0);
    }
}
