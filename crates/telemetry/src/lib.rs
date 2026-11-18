//! Telemetry for the media pipeline: a monotonic clock, per-stage timing
//! and session reports.
//!
//! Intentionally empty for now; the clock and stage recording arrive later.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds_and_runs_tests() {
        assert_eq!(env!("CARGO_PKG_NAME"), "telemetry");
    }
}
