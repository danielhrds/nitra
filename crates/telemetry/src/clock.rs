//! Monotonic clock based on `QueryPerformanceCounter` (QPC).
//!
//! Raw QPC ticks are kept instead of `std::time::Instant` because Windows
//! capture APIs (WGC `SystemRelativeTime`, DXGI `LastPresentTime`) report
//! timestamps on the same QPC timeline, so they can be compared directly.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

const MICROS_PER_SECOND: f64 = 1_000_000.0;

/// A point in time on the QPC timeline, in raw ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QpcTimestamp(i64);

impl QpcTimestamp {
    pub const fn from_ticks(ticks: i64) -> Self {
        Self(ticks)
    }

    pub const fn ticks(self) -> i64 {
        self.0
    }

    /// Time elapsed from `earlier` to `self`; zero if `earlier` is later.
    pub fn duration_since(self, earlier: QpcTimestamp) -> QpcDuration {
        let ticks = self.0.saturating_sub(earlier.0).max(0);
        // `ticks` is non-negative here, so the conversion is lossless.
        QpcDuration(ticks.unsigned_abs())
    }
}

/// A non-negative span of QPC ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QpcDuration(u64);

impl QpcDuration {
    pub const ZERO: Self = Self(0);

    pub const fn from_ticks(ticks: u64) -> Self {
        Self(ticks)
    }

    /// Converts microseconds to ticks at `frequency`, rounding to the nearest tick.
    pub fn from_micros(micros: f64, frequency: QpcFrequency) -> Self {
        let ticks = (micros.max(0.0) * frequency.ticks_per_second() as f64 / MICROS_PER_SECOND)
            .round() as u64;
        Self(ticks)
    }

    pub const fn ticks(self) -> u64 {
        self.0
    }

    pub fn as_micros(self, frequency: QpcFrequency) -> f64 {
        ticks_to_micros(self.0 as f64, frequency)
    }
}

/// QPC ticks per second. Fixed at boot and never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QpcFrequency(u64);

impl QpcFrequency {
    /// Returns `None` for a zero frequency, which would make every conversion divide by zero.
    pub const fn new(ticks_per_second: u64) -> Option<Self> {
        if ticks_per_second == 0 {
            None
        } else {
            Some(Self(ticks_per_second))
        }
    }

    pub const fn ticks_per_second(self) -> u64 {
        self.0
    }
}

/// Converts a (possibly fractional) tick count to microseconds.
pub(crate) fn ticks_to_micros(ticks: f64, frequency: QpcFrequency) -> f64 {
    ticks * MICROS_PER_SECOND / frequency.0 as f64
}

/// Reads the current QPC value.
pub fn qpc_now() -> QpcTimestamp {
    let mut ticks = 0_i64;
    // SAFETY: the pointer refers to a live, writable, properly aligned local i64.
    unsafe { QueryPerformanceCounter(&mut ticks) }
        .expect("QueryPerformanceCounter never fails on Windows XP or later");
    QpcTimestamp(ticks)
}

/// The QPC frequency, queried once per process.
pub fn qpc_frequency() -> QpcFrequency {
    static FREQUENCY: OnceLock<QpcFrequency> = OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut ticks_per_second = 0_i64;
        // SAFETY: the pointer refers to a live, writable, properly aligned local i64.
        unsafe { QueryPerformanceFrequency(&mut ticks_per_second) }
            .expect("QueryPerformanceFrequency never fails on Windows XP or later");
        u64::try_from(ticks_per_second)
            .ok()
            .and_then(QpcFrequency::new)
            .expect("QPC frequency is positive on Windows XP or later")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEN_MHZ: QpcFrequency = match QpcFrequency::new(10_000_000) {
        Some(frequency) => frequency,
        None => unreachable!(),
    };

    #[test]
    fn zero_frequency_is_rejected() {
        assert_eq!(QpcFrequency::new(0), None);
    }

    #[test]
    fn ticks_convert_to_micros() {
        assert_eq!(QpcDuration::from_ticks(10).as_micros(TEN_MHZ), 1.0);
        assert_eq!(
            QpcDuration::from_ticks(166_667).as_micros(TEN_MHZ),
            16_666.7
        );
    }

    #[test]
    fn micros_convert_to_ticks() {
        assert_eq!(QpcDuration::from_micros(1.0, TEN_MHZ).ticks(), 10);
        assert_eq!(QpcDuration::from_micros(-5.0, TEN_MHZ), QpcDuration::ZERO);
    }

    #[test]
    fn duration_since_saturates_at_zero() {
        let earlier = QpcTimestamp::from_ticks(100);
        let later = QpcTimestamp::from_ticks(250);
        assert_eq!(later.duration_since(earlier).ticks(), 150);
        assert_eq!(earlier.duration_since(later), QpcDuration::ZERO);
    }

    #[test]
    fn clock_never_goes_backwards() {
        let mut previous = qpc_now();
        for _ in 0..10_000 {
            let current = qpc_now();
            assert!(current >= previous, "{current:?} < {previous:?}");
            previous = current;
        }
    }

    #[test]
    fn frequency_is_cached_and_positive() {
        let first = qpc_frequency();
        assert!(first.ticks_per_second() > 0);
        assert_eq!(first, qpc_frequency());
    }
}
