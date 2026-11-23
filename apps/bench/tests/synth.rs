//! End-to-end runs of `bench synth`. They need a GPU, so they are ignored by
//! default; run them with `scripts/gpu-tests.ps1`. Reports go to a temporary
//! folder through `NITRA_LAB_DIR`, not to the lab folder.

use std::fs;
use std::process::Command;

#[test]
#[ignore = "requires a GPU"]
fn validated_synth_run_matches_every_stamp() {
    let out_dir = std::env::temp_dir().join(format!("nitra-bench-synth-{}", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_bench"))
        .args([
            "synth",
            "--fps",
            "30",
            "--seconds",
            "1",
            "--mode",
            "motion",
            "--validate",
            "--dump-frame",
            "3",
        ])
        .env("NITRA_LAB_DIR", &out_dir)
        .output()
        .expect("bench runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let written: Vec<_> = fs::read_dir(&out_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let _ = fs::remove_dir_all(&out_dir);

    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("validation: 30/30"), "{stdout}");
    assert!(stdout.contains("0 skipped"), "{stdout}");
    assert!(
        written
            .iter()
            .any(|name| name.ends_with("-synth-motion.json")),
        "{written:?}"
    );
    assert!(
        written.iter().any(|name| name.ends_with(".bmp")),
        "{written:?}"
    );
}
