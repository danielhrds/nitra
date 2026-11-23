//! End-to-end run of `bench loopback`: synthetic stamped frames captured,
//! encoded, decoded and shown in one process, with latency read from the
//! stamps. Needs a GPU and a desktop session, so it is ignored by default;
//! run it with `scripts/gpu-tests.ps1`.

use std::fs;
use std::process::Command;

#[test]
#[ignore = "requires a GPU with hardware H.264 encode and decode, and a desktop session"]
fn loopback_shows_stamped_frames_end_to_end() {
    let summary =
        std::env::temp_dir().join(format!("nitra-bench-loopback-{}.json", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_bench"))
        .args([
            "loopback",
            "--width",
            "1280",
            "--height",
            "720",
            "--seconds",
            "4",
        ])
        .arg("--summary-json")
        .arg(&summary)
        .output()
        .expect("bench runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(&summary).expect("summary written");
    let _ = fs::remove_file(&summary);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let number = |pointer: &str| {
        json.pointer(pointer)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or_else(|| panic!("{pointer} missing in {text}"))
    };

    // Frames flowed through every stage without piling up.
    let access_units = number("/viewer/access_units");
    assert!(access_units >= 120.0, "{access_units} access units in 4 s");
    assert_eq!(number("/viewer/decoded"), access_units, "{text}");
    assert_eq!(number("/host/pipeline/output_full_dropped"), 0.0, "{text}");
    assert_eq!(number("/host/pipeline/busy"), 0.0, "{text}");
    assert!(number("/viewer/presenter/presented") >= 100.0, "{text}");

    // Stamps readable at both measuring points, latency in a sane range.
    assert!(number("/decoded_stamps/read") >= 10.0, "{text}");
    assert_eq!(number("/decoded_stamps/unreadable"), 0.0, "{text}");
    assert!(number("/screen_stamps/read") >= 20.0, "{text}");
    assert!(
        number("/screen_stamps/unreadable") * 10.0 <= number("/screen_stamps/read"),
        "{text}"
    );
    let to_decoded = number("/latency_to_decoded_ms/p50_us");
    let to_screen = number("/latency_to_screen_ms/p50_us");
    assert!(to_decoded > 0.0 && to_decoded < to_screen, "{text}");
    assert!(to_screen < 100.0, "render -> screen p50 {to_screen} ms");

    // DXGI frame statistics place most viewer frames at their refresh.
    let presented = number("/viewer/presenter/presented");
    assert!(
        number("/viewer/display/matched") * 10.0 >= presented * 8.0,
        "{text}"
    );
    assert_eq!(number("/viewer/display/implausible"), 0.0, "{text}");
    let to_displayed = number("/latency_to_displayed_ms/p50_us");
    assert!(
        to_decoded < to_displayed && to_displayed < 100.0,
        "render -> displayed p50 {to_displayed} ms; {text}"
    );
}
