//! Writes a session report from deterministic synthetic stage timings.
//!
//! Simulates 60 seconds at 60 fps with a fixed per-stage cost, deterministic
//! jitter and a periodic spike, so the report shape can be inspected without
//! any capture or encoder. The numbers are made up; only the format is real.
//!
//! Usage: `cargo run -p telemetry --example synthetic_report [output_dir]`
//! (default: `ZIZ_LAB_DIR`, or the `lab` folder next to the repository).

use std::path::PathBuf;

use telemetry::{QpcDuration, Session, Stage, StageRecorder, default_output_dir, qpc_frequency};

const FRAMES: u32 = 60 * 60;

/// Base cost and spike cost per stage, in microseconds.
const STAGE_COSTS_US: [(Stage, f64, f64); 7] = [
    (Stage::Capture, 400.0, 2_000.0),
    (Stage::Convert, 250.0, 1_200.0),
    (Stage::Submit, 120.0, 800.0),
    (Stage::EncodeOutput, 3_500.0, 9_000.0),
    (Stage::Network, 1_000.0, 15_000.0),
    (Stage::Decode, 2_000.0, 6_000.0),
    (Stage::Present, 300.0, 16_000.0),
];

/// Small deterministic generator (64-bit LCG) so every run produces the same samples.
struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1_u64 << 53) as f64
    }
}

fn main() -> std::io::Result<()> {
    let output_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_output_dir);

    let frequency = qpc_frequency();
    let session = Session::start("synthetic", &[("source", "synthetic")]);
    let mut recorder = StageRecorder::new();
    let mut random = Lcg(0x5EED);

    for frame in 0..FRAMES {
        let is_spike = frame % 300 == 299;
        for (stage, base_us, spike_us) in STAGE_COSTS_US {
            let jitter = 1.0 + 0.3 * (random.next_unit() - 0.5);
            let cost_us = if is_spike { spike_us } else { base_us * jitter };
            recorder.record_duration(stage, QpcDuration::from_micros(cost_us, frequency));
        }
    }

    let report = session.finish(&recorder);
    let paths = report.write_files(&output_dir)?;
    println!("{}", report.to_csv());
    println!("json: {}", paths.json.display());
    println!("csv:  {}", paths.csv.display());
    Ok(())
}
