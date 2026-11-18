//! Per-session reports exported as JSON and CSV.
//!
//! Reports carry only numbers and compile-time labels (`&'static str`), so no
//! runtime value such as a key, token, address or user path can end up in one.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::clock::{QpcFrequency, QpcTimestamp, qpc_frequency, qpc_now};
use crate::stage::{Stage, StageRecorder};
use crate::stats::Summary;

/// Bumped whenever a field is renamed, removed or changes meaning.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Environment variable that overrides the default report directory.
pub const OUTPUT_DIR_ENV: &str = "ZIZ_LAB_DIR";

const CSV_HEADER: &str = "stage,count,dropped_samples,mean_us,min_us,p50_us,p95_us,p99_us,max_us";

/// A measurement session in progress: remembers when it started.
#[derive(Debug)]
pub struct Session {
    label: &'static str,
    backends: BTreeMap<&'static str, &'static str>,
    started_unix_ms: u64,
    started_qpc: QpcTimestamp,
}

impl Session {
    /// Starts a session now.
    ///
    /// `label` names the scenario and becomes part of the file name, so it
    /// must be ASCII letters, digits, `-` or `_`. `backends` maps a component
    /// to the implementation in use, e.g. `("capture", "wgc")`.
    ///
    /// # Panics
    /// If `label` is empty or contains other characters.
    pub fn start(label: &'static str, backends: &[(&'static str, &'static str)]) -> Self {
        assert!(
            is_valid_label(label),
            "session label must be non-empty ASCII alphanumerics, '-' or '_': {label:?}"
        );
        let started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since_epoch| since_epoch.as_millis() as u64);
        Self {
            label,
            backends: backends.iter().copied().collect(),
            started_unix_ms,
            started_qpc: qpc_now(),
        }
    }

    /// Ends the session and summarizes everything in `recorder`.
    pub fn finish(&self, recorder: &StageRecorder) -> SessionReport {
        let frequency = qpc_frequency();
        let elapsed = qpc_now().duration_since(self.started_qpc);
        SessionReport::build(
            self,
            recorder,
            elapsed.as_micros(frequency) / 1_000.0,
            frequency,
        )
    }
}

fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Summary of one session, ready to be written to disk.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionReport {
    pub schema_version: u32,
    pub app_version: &'static str,
    pub build_profile: &'static str,
    pub label: &'static str,
    pub backends: BTreeMap<&'static str, &'static str>,
    pub started_unix_ms: u64,
    pub duration_ms: f64,
    pub qpc_frequency: u64,
    /// True when any stage dropped samples because its capacity was reached.
    pub truncated: bool,
    pub sample_capacity_per_stage: usize,
    pub stages: Vec<StageReport>,
}

/// Statistics for one stage; `summary` is `None` when nothing was recorded.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StageReport {
    pub stage: Stage,
    pub dropped_samples: u64,
    pub summary: Option<Summary>,
}

/// Where a report was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportPaths {
    pub json: PathBuf,
    pub csv: PathBuf,
}

impl SessionReport {
    fn build(
        session: &Session,
        recorder: &StageRecorder,
        duration_ms: f64,
        frequency: QpcFrequency,
    ) -> Self {
        let stages: Vec<StageReport> = Stage::ALL
            .iter()
            .map(|&stage| StageReport {
                stage,
                dropped_samples: recorder.dropped(stage),
                summary: Summary::from_durations(recorder.samples(stage), frequency),
            })
            .collect();

        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            app_version: env!("CARGO_PKG_VERSION"),
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            label: session.label,
            backends: session.backends.clone(),
            started_unix_ms: session.started_unix_ms,
            duration_ms,
            qpc_frequency: frequency.ticks_per_second(),
            truncated: stages.iter().any(|stage| stage.dropped_samples > 0),
            sample_capacity_per_stage: recorder.capacity_per_stage(),
            stages,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("report contains only serializable values")
    }

    /// One row per stage; statistic columns are empty for stages without samples.
    pub fn to_csv(&self) -> String {
        let mut csv = String::from(CSV_HEADER);
        csv.push('\n');
        for stage in &self.stages {
            match &stage.summary {
                Some(summary) => writeln!(
                    csv,
                    "{},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
                    stage.stage.name(),
                    summary.count,
                    stage.dropped_samples,
                    summary.mean_us,
                    summary.min_us,
                    summary.p50_us,
                    summary.p95_us,
                    summary.p99_us,
                    summary.max_us,
                ),
                None => writeln!(
                    csv,
                    "{},0,{},,,,,,",
                    stage.stage.name(),
                    stage.dropped_samples
                ),
            }
            .expect("writing to a String cannot fail");
        }
        csv
    }

    /// Writes `session-<start ms>-<label>.json` and `.csv` into `dir`,
    /// creating it if needed. Never overwrites an existing report.
    pub fn write_files(&self, dir: &Path) -> io::Result<ReportPaths> {
        fs::create_dir_all(dir)?;
        let stem = format!("session-{}-{}", self.started_unix_ms, self.label);
        let paths = ReportPaths {
            json: dir.join(format!("{stem}.json")),
            csv: dir.join(format!("{stem}.csv")),
        };
        File::create_new(&paths.json)?.write_all(self.to_json().as_bytes())?;
        File::create_new(&paths.csv)?.write_all(self.to_csv().as_bytes())?;
        tracing::info!(json = %paths.json.display(), csv = %paths.csv.display(), "session report written");
        Ok(paths)
    }
}

/// Report directory: `ZIZ_LAB_DIR` if set, otherwise the `lab` folder next to
/// the repository (a development default resolved at compile time).
pub fn default_output_dir() -> PathBuf {
    match std::env::var_os(OUTPUT_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => repository_parent_dir().join("lab"),
    }
}

/// The folder that contains the repository, derived from this crate's
/// manifest path (`<parent>/<repo>/crates/telemetry`).
fn repository_parent_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate lives at <parent>/<repo>/crates/telemetry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::QpcDuration;

    fn synthetic_recorder(frequency: QpcFrequency) -> StageRecorder {
        let mut recorder = StageRecorder::with_capacity(1_000);
        // Every stage except Network gets 100 samples of 1..=100 microseconds.
        for stage in Stage::ALL
            .into_iter()
            .filter(|&stage| stage != Stage::Network)
        {
            for micros in 1..=100 {
                recorder.record_duration(
                    stage,
                    QpcDuration::from_micros(f64::from(micros), frequency),
                );
            }
        }
        recorder
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ziz-telemetry-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn labels_are_restricted_to_file_name_safe_ascii() {
        assert!(is_valid_label("synthetic"));
        assert!(is_valid_label("wgc-1080p_60"));
        assert!(!is_valid_label(""));
        assert!(!is_valid_label("a b"));
        assert!(!is_valid_label("../escape"));
        assert!(!is_valid_label("ção"));
    }

    #[test]
    #[should_panic(expected = "session label")]
    fn invalid_label_panics() {
        let _ = Session::start("not valid", &[]);
    }

    #[test]
    fn report_summarizes_every_stage() {
        let frequency = qpc_frequency();
        let session = Session::start("unit", &[("capture", "synthetic")]);
        let report = session.finish(&synthetic_recorder(frequency));

        assert_eq!(report.schema_version, REPORT_SCHEMA_VERSION);
        assert_eq!(report.label, "unit");
        assert_eq!(report.backends.get("capture"), Some(&"synthetic"));
        assert_eq!(report.qpc_frequency, frequency.ticks_per_second());
        assert!(!report.truncated);
        assert!(report.duration_ms >= 0.0);

        let names: Vec<_> = report.stages.iter().map(|stage| stage.stage).collect();
        assert_eq!(names, Stage::ALL);
        let capture = report.stages[0].summary.unwrap();
        assert_eq!(capture.count, 100);
        assert!((capture.p95_us - 95.0).abs() < 0.01, "{}", capture.p95_us);
        let network = &report.stages[Stage::ALL
            .iter()
            .position(|&s| s == Stage::Network)
            .unwrap()];
        assert_eq!(network.summary, None);
    }

    #[test]
    fn truncation_is_reported() {
        let mut recorder = StageRecorder::with_capacity(1);
        recorder.record_duration(Stage::Present, QpcDuration::from_ticks(1));
        recorder.record_duration(Stage::Present, QpcDuration::from_ticks(2));
        let report = Session::start("truncated", &[]).finish(&recorder);
        assert!(report.truncated);
        assert_eq!(report.sample_capacity_per_stage, 1);
    }

    #[test]
    fn csv_has_header_and_one_row_per_stage() {
        let frequency = qpc_frequency();
        let report = Session::start("csv", &[]).finish(&synthetic_recorder(frequency));
        let csv = report.to_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 1 + Stage::ALL.len());
        assert_eq!(lines[0], CSV_HEADER);
        assert!(lines[1].starts_with("capture,100,0,"), "{}", lines[1]);
        assert_eq!(lines[5], "network,0,0,,,,,,");
        for line in &lines {
            assert_eq!(line.split(',').count(), 9, "{line}");
        }
    }

    #[test]
    fn files_round_trip_through_json() {
        let frequency = qpc_frequency();
        let report = Session::start("roundtrip", &[("encoder", "synthetic")])
            .finish(&synthetic_recorder(frequency));
        let dir = unique_temp_dir("roundtrip");

        let paths = report.write_files(&dir).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&paths.json).unwrap()).unwrap();
        let csv = fs::read_to_string(&paths.csv).unwrap();

        // Writing the same report again must not overwrite it.
        let second_write = report.write_files(&dir);
        fs::remove_dir_all(&dir).unwrap();

        assert_eq!(json["schema_version"], REPORT_SCHEMA_VERSION);
        assert_eq!(json["label"], "roundtrip");
        assert_eq!(json["backends"]["encoder"], "synthetic");
        assert_eq!(json["truncated"], false);
        assert_eq!(json["stages"].as_array().unwrap().len(), Stage::ALL.len());
        assert_eq!(json["stages"][0]["stage"], "capture");
        assert_eq!(json["stages"][0]["summary"]["count"], 100);
        assert_eq!(json["stages"][3]["stage"], "encode_output");
        assert!(json["stages"][4]["summary"].is_null());
        assert_eq!(csv, report.to_csv());
        assert_eq!(
            second_write.unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn output_dir_defaults_next_to_repository() {
        // Only checks the fallback shape; the environment override is not
        // exercised because tests run in parallel and share the environment.
        if std::env::var_os(OUTPUT_DIR_ENV).is_none() {
            let dir = default_output_dir();
            assert_eq!(dir.file_name(), Some("lab".as_ref()));
            assert!(
                Path::new(env!("CARGO_MANIFEST_DIR")).starts_with(dir.parent().unwrap()),
                "{}",
                dir.display()
            );
        }
    }
}
