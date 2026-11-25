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

    let (started, cpu_before, counted_before, threads_before) = (
        Instant::now(),
        cpu_time(),
        capture.counters().delivered,
        thread_times(),
    );
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
    // Where it went: our capture thread, the test thread (taking frames),
    // and the rest (the system's camera threads, and for DirectShow the
    // camera's own filter, which runs in this process).
    let after = thread_times();
    let mut by_thread: Vec<(String, f64)> = after
        .iter()
        .map(|(id, (name, time))| {
            let before = threads_before.get(id).map_or(Duration::ZERO, |(_, t)| *t);
            (
                name.clone(),
                100.0 * (*time - before).as_secs_f64() / wall.as_secs_f64(),
            )
        })
        .filter(|(_, share)| *share > 0.01)
        .collect();
    by_thread.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (name, share) in by_thread {
        println!("  thread {name:?}: {share:.2}% of a core");
    }
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

/// Each thread of this process: its name and CPU time so far.
fn thread_times() -> std::collections::HashMap<u32, (String, Duration)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, GetThreadTimes, OpenThread,
        THREAD_QUERY_LIMITED_INFORMATION,
    };

    let mut threads = std::collections::HashMap::new();
    // SAFETY: a snapshot handle closed at the end; each thread handle is
    // opened for querying and closed after; out-pointers are live locals;
    // the description string is freed with LocalFree.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).unwrap();
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut more = Thread32First(snapshot, &mut entry).is_ok();
        while more {
            if entry.th32OwnerProcessID == GetCurrentProcessId()
                && let Ok(thread) =
                    OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, entry.th32ThreadID)
            {
                let (mut created, mut exited, mut kernel, mut user) = Default::default();
                if GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user).is_ok()
                {
                    let ticks = |t: FILETIME| {
                        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
                    };
                    let name = match GetThreadDescription(thread) {
                        Ok(text) => {
                            let name = text.to_string().unwrap_or_default();
                            let _ = windows::Win32::Foundation::LocalFree(Some(
                                windows::Win32::Foundation::HLOCAL(text.0.cast()),
                            ));
                            name
                        }
                        Err(_) => String::new(),
                    };
                    let name = if name.is_empty() {
                        format!("#{}", entry.th32ThreadID)
                    } else {
                        name
                    };
                    let time = Duration::from_nanos((ticks(kernel) + ticks(user)) * 100);
                    threads.insert(entry.th32ThreadID, (name, time));
                }
                let _ = CloseHandle(thread);
            }
            more = Thread32Next(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    threads
}
