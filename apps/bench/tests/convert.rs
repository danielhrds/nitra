//! End-to-end runs of `bench convert`. They need a GPU, so they are ignored
//! by default; run them with `scripts/gpu-tests.ps1`.

use std::process::Command;

#[test]
#[ignore = "requires a GPU"]
fn frame_stamps_survive_both_conversion_methods() {
    for method in ["vp", "shader"] {
        let output = Command::new(env!("CARGO_BIN_EXE_bench"))
            .args([
                "convert",
                "--method",
                method,
                "--fps",
                "30",
                "--seconds",
                "1",
                "--validate",
            ])
            .output()
            .expect("bench runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{method}: stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("validation: 30/30"), "{method}: {stdout}");
        assert!(stdout.contains("pool drops 0"), "{method}: {stdout}");
    }
}
