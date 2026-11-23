//! End-to-end runs of `bench probe`. They need a GPU and an interactive
//! desktop session, so they are ignored by default; run them with
//! `scripts/gpu-tests.ps1`.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn temp_json(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nitra-bench-{name}-{}.json", std::process::id()))
}

fn bench(args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_bench"))
        .args(args)
        .output()
        .expect("bench runs");
    assert!(
        output.status.success(),
        "bench {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn probe_writes_complete_json() {
    let path = temp_json("probe");
    bench(&["probe", "--json", path.to_str().unwrap()]);
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    fs::remove_file(&path).unwrap();

    assert_eq!(json["errors"], serde_json::json!([]), "{json:#}");
    assert_eq!(json["wgc"]["supported"], true);

    let adapters = json["adapters"].as_array().unwrap();
    let hardware: Vec<_> = adapters
        .iter()
        .filter(|adapter| adapter["is_software"] == false)
        .collect();
    assert!(!hardware.is_empty(), "{json:#}");
    for adapter in hardware {
        assert!(adapter["d3d11"].is_object(), "{adapter:#}");
        assert!(adapter["d3d11"]["decode"].is_object(), "{adapter:#}");
    }
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn repeated_probe_does_not_leak_handles() {
    let path = temp_json("repeat");
    let output = bench(&["probe", "--repeat", "20", "--json", path.to_str().unwrap()]);
    fs::remove_file(&path).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Leak check (20 runs"), "{stdout}");

    // A per-run leak shows up as roughly +19 handles; allow small noise.
    let growth: i64 = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("growth:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|handles| handles.parse().ok())
        .unwrap_or_else(|| panic!("no growth line in:\n{stdout}"));
    assert!(
        growth <= 3,
        "handle growth {growth} over 19 runs:\n{stdout}"
    );
}
