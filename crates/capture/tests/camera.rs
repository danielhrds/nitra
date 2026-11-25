//! Cameras on this PC. Listing opens none; reading one turns it on, so that
//! test is ignored and runs only for the camera named in
//! `NITRA_TEST_CAMERA` (never "the first one"):
//!
//! ```powershell
//! $env:NITRA_TEST_CAMERA = "Iriun Webcam"
//! cargo test --release -p capture --test camera -- --ignored --nocapture
//! ```
//!
//! `NITRA_TEST_CAMERA_BMP` (a new file's path) saves the last frame.

use std::time::{Duration, Instant};

use capture::camera::{CameraRequest, list_cameras, start_camera};
use capture::{CaptureEnd, CaptureOptions, Taken};
use gpu::DebugLayer;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

#[test]
fn cameras_are_listed_without_opening_them() {
    let cameras = list_cameras().unwrap();
    println!("{} camera(s): {cameras:?}", cameras.len());
    assert!(cameras.iter().all(|c| !c.name.is_empty()));
}

/// The process's CPU time (all threads, Media Foundation's included).
fn cpu_time() -> Duration {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: pseudo-handle of this process; the out-pointers are live locals.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .unwrap();
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

#[test]
#[ignore = "turns on the camera named in NITRA_TEST_CAMERA"]
fn a_named_camera_delivers_frames() {
    let Ok(name) = std::env::var("NITRA_TEST_CAMERA") else {
        panic!("set NITRA_TEST_CAMERA to a camera's name");
    };
    let adapters = gpu::enumerate_adapters().unwrap();
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    // No debug layer: this test also measures the cost.
    let device = gpu::create_device(adapter, DebugLayer::Off).unwrap();
    let request = CameraRequest::default();
    let options = CaptureOptions {
        fps: request.fps,
        ..CaptureOptions::default()
    };
    let opening = Instant::now();
    let (capture, mode) = start_camera(&device, &name, request, options).unwrap();
    println!("opened {name:?} as {mode:?} in {:?}", opening.elapsed());

    // The first frame can take a while (the camera starts up); measure after.
    let first = Instant::now();
    let mut last = match capture.next(Duration::from_secs(10)) {
        Taken::Value(frame) => frame,
        Taken::Ended(end) => panic!("ended: {end:?}"),
        Taken::Timeout => panic!("no frame in 10 s: {:?}", capture.counters()),
    };
    println!("first frame after {:?}", first.elapsed());

    let (started, cpu_before, counted_before) =
        (Instant::now(), cpu_time(), capture.counters().delivered);
    while started.elapsed() < Duration::from_secs(10) {
        match capture.next(Duration::from_secs(2)) {
            Taken::Value(frame) => last = frame,
            Taken::Ended(end) => panic!("ended: {end:?}"),
            Taken::Timeout => panic!("no frame in 2 s: {:?}", capture.counters()),
        }
    }
    let (wall, cpu) = (started.elapsed(), cpu_time() - cpu_before);
    let delivered = capture.counters().delivered - counted_before;
    let core = 100.0 * cpu.as_secs_f64() / wall.as_secs_f64();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!(
        "{:.1} frames/s delivered; process CPU {core:.2}% of a core ({:.3}% of {cores})",
        delivered as f64 / wall.as_secs_f64(),
        core / cores as f64,
    );
    assert_eq!((last.width, last.height), (mode.width, mode.height));
    if let Ok(path) = std::env::var("NITRA_TEST_CAMERA_BMP") {
        capture::synthetic::dump_bgra_bmp(&device, &last.texture, path.as_ref()).unwrap();
        println!("last frame saved to {path}");
    }
    drop(last);

    let report = capture.stop();
    assert_eq!(report.end, CaptureEnd::Stopped);
    let frequency = telemetry::qpc_frequency();
    let mut cost: Vec<f64> = report
        .cost
        .iter()
        .map(|c| c.as_micros(frequency) / 1e3)
        .collect();
    cost.sort_by(f64::total_cmp);
    let at = |p: f64| cost[((cost.len() - 1) as f64 * p) as usize];
    println!(
        "capture thread per frame: p50 {:.3} ms, p95 {:.3} ms, max {:.3} ms; {:?}",
        at(0.5),
        at(0.95),
        at(1.0),
        report.counters
    );
}
