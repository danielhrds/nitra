//! Telemetry for the media pipeline: a QPC-based monotonic clock, bounded
//! per-stage duration recording and per-session JSON/CSV reports.
//!
//! Typical use: each pipeline thread owns a [`StageRecorder`] and records
//! `(stage, start, end)` with [`qpc_now`]; at the end the recorders are
//! merged and [`Session::finish`] produces a [`SessionReport`].

mod clock;
pub mod frame_code;
mod report;
mod stage;
mod stats;

pub use clock::{QpcDuration, QpcFrequency, QpcTimestamp, qpc_frequency, qpc_now};
pub use report::{
    OUTPUT_DIR_ENV, REPORT_SCHEMA_VERSION, ReportPaths, Session, SessionReport, StageReport,
    default_output_dir,
};
pub use stage::{DEFAULT_SAMPLE_CAPACITY, Stage, StageRecorder};
pub use stats::Summary;
