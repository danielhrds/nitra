//! WGC capture against a real desktop session. Ignored by default; run with
//! `scripts/gpu-tests.ps1`. Test windows briefly appear on screen.

use std::time::{Duration, Instant};

use capture::test_window::TestWindow;
use capture::wgc::{CaptureEnd, CaptureOptions, CaptureTarget, Taken, WgcCapture};
use gpu::{D3d11Device, DebugLayer, GpuFrame};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

fn hardware_device() -> D3d11Device {
    let adapters = gpu::enumerate_adapters().unwrap();
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    gpu::create_device(adapter, DebugLayer::IfAvailable).unwrap()
}

fn next_frame(capture: &WgcCapture, timeout: Duration) -> GpuFrame {
    match capture.next(timeout) {
        Taken::Value(frame) => frame,
        Taken::Ended(end) => panic!("capture ended: {end:?}"),
        Taken::Timeout => panic!("no frame within {timeout:?}: {:?}", capture.counters()),
    }
}

fn assert_no_debug_problems(device: &D3d11Device) {
    let problems: Vec<_> = device
        .take_debug_messages()
        .into_iter()
        .filter(|message| message.severity.is_problem())
        .collect();
    assert!(problems.is_empty(), "{problems:#?}");
}

fn handle_count() -> u32 {
    let mut count = 0;
    // SAFETY: pseudo-handle of the current process and a live out-pointer.
    unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }.unwrap();
    count
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn window_capture_sees_the_window_color() {
    let device = hardware_device();
    let window = TestWindow::open("ziz wgc color", 640, 480, [32, 160, 224], false).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let capture = WgcCapture::start(
        &device,
        CaptureTarget::Window(window.hwnd()),
        CaptureOptions::default(),
    )
    .unwrap();

    let frame = next_frame(&capture, Duration::from_secs(5));
    let [b, g, r, _] =
        gpu::read_pixel(&device, &frame.texture, frame.width / 2, frame.height / 2).unwrap();
    let close = |actual: u8, expected: u8| actual.abs_diff(expected) <= 3;
    assert!(
        close(r, 32) && close(g, 160) && close(b, 224),
        "rgb = {r}, {g}, {b}"
    );

    drop(frame);
    let report = capture.stop();
    assert_eq!(report.end, CaptureEnd::Stopped);
    assert!(report.counters.delivered >= 1);
    assert_no_debug_problems(&device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn window_resize_changes_frame_size() {
    let device = hardware_device();
    let window = TestWindow::open("ziz wgc resize", 640, 480, [200, 80, 40], true).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let capture = WgcCapture::start(
        &device,
        CaptureTarget::Window(window.hwnd()),
        CaptureOptions::default(),
    )
    .unwrap();

    let first = next_frame(&capture, Duration::from_secs(5));
    let first_size = (first.width, first.height);
    drop(first);
    window.resize(960, 640).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let new_size = loop {
        assert!(
            Instant::now() < deadline,
            "size never changed: {:?}",
            capture.counters()
        );
        let frame = next_frame(&capture, Duration::from_secs(5));
        if (frame.width, frame.height) != first_size {
            break (frame.width, frame.height);
        }
    };
    assert!(
        new_size.0 > first_size.0 && new_size.1 > first_size.1,
        "{first_size:?} -> {new_size:?}"
    );
    let report = capture.stop();
    assert!(report.counters.resizes >= 1, "{:?}", report.counters);
    assert_no_debug_problems(&device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn closing_the_window_ends_the_capture() {
    let device = hardware_device();
    let window = TestWindow::open("ziz wgc close", 480, 360, [40, 200, 80], true).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let capture = WgcCapture::start(
        &device,
        CaptureTarget::Window(window.hwnd()),
        CaptureOptions::default(),
    )
    .unwrap();
    drop(next_frame(&capture, Duration::from_secs(5)));

    window.close();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "capture did not end");
        match capture.next(Duration::from_millis(500)) {
            Taken::Ended(end) => {
                assert_eq!(end, CaptureEnd::SourceClosed);
                break;
            }
            Taken::Value(_) | Taken::Timeout => {}
        }
    }
    assert_eq!(capture.stop().end, CaptureEnd::SourceClosed);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn monitor_capture_delivers_frames() {
    let device = hardware_device();
    let adapters = gpu::enumerate_adapters().unwrap();
    let output = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .and_then(|adapter| adapter.outputs.first())
        .expect("an output on the hardware adapter")
        .clone();
    let capture = WgcCapture::start(
        &device,
        CaptureTarget::Monitor(output.monitor()),
        CaptureOptions::default(),
    )
    .unwrap();
    // WGC sends a first frame on start even for a static desktop.
    let frame = next_frame(&capture, Duration::from_secs(5));
    let desktop = output.info.desktop;
    assert_eq!(
        (frame.width as i32, frame.height as i32),
        (desktop.right - desktop.left, desktop.bottom - desktop.top)
    );
    drop(frame);
    capture.stop();
    assert_no_debug_problems(&device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn sustained_capture_does_not_leak_handles() {
    let device = hardware_device();
    let window = TestWindow::open("ziz wgc soak", 800, 600, [90, 90, 160], true).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let capture = WgcCapture::start(
        &device,
        CaptureTarget::Window(window.hwnd()),
        CaptureOptions::default(),
    )
    .unwrap();

    for _ in 0..60 {
        drop(next_frame(&capture, Duration::from_secs(5)));
    }
    let before = handle_count();
    let mut delivered = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        drop(next_frame(&capture, Duration::from_secs(5)));
        delivered += 1;
    }
    let after = handle_count();
    let report = capture.stop();
    assert!(
        after <= before + 3,
        "handles grew from {before} to {after} over {delivered} frames"
    );
    // The GDI test window repaints at roughly 40-64 Hz (timer granularity);
    // what matters is that capture keeps up with whatever arrives.
    let counters = report.counters;
    assert!(
        delivered >= 200,
        "only {delivered} frames in 10 s: {counters:?}"
    );
    assert_eq!(counters.pool_dropped, 0, "{counters:?}");
    assert!(
        counters.delivered + counters.rate_dropped + counters.superseded >= counters.arrived,
        "{counters:?}"
    );
    assert!(
        counters.rate_dropped + counters.superseded <= counters.arrived / 50,
        "too many frames dropped below the target rate: {counters:?}"
    );
    assert_no_debug_problems(&device);
}
