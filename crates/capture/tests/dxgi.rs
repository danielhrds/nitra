//! DXGI Desktop Duplication against a real desktop session. Ignored by
//! default; run with `scripts/gpu-tests.ps1` (one test at a time: the tests
//! put a topmost window on the primary monitor and read the screen).

use std::time::{Duration, Instant};

use capture::test_window::TestWindow;
use capture::{Capture, CaptureEnd, CaptureOptions, Taken};
use gpu::{D3d11Device, DebugLayer, GpuFrame, Output};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

/// Device and the first output of the first hardware adapter.
fn device_and_output() -> (D3d11Device, Output) {
    capture::dxgi::enable_per_monitor_dpi_awareness();
    let adapters = gpu::enumerate_adapters().unwrap();
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    let device = gpu::create_device(adapter, DebugLayer::IfAvailable).unwrap();
    let output = adapter.outputs.first().expect("an output").clone();
    (device, output)
}

fn next_frame(capture: &Capture, timeout: Duration) -> GpuFrame {
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

/// Center of the window's client area, in screen coordinates.
fn client_center_on_screen(window: &TestWindow) -> (i32, i32) {
    let mut client = RECT::default();
    // SAFETY: valid window handle; out-pointers reference live locals.
    unsafe { GetClientRect(window.hwnd(), &mut client) }.unwrap();
    let mut center = POINT {
        x: (client.left + client.right) / 2,
        y: (client.top + client.bottom) / 2,
    };
    // SAFETY: as above.
    let _ = unsafe { ClientToScreen(window.hwnd(), &mut center) };
    (center.x, center.y)
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn duplication_shows_the_window_at_its_screen_position() {
    let (device, output) = device_and_output();
    let window = TestWindow::open("nitra dxgi color", 640, 480, [220, 60, 120], false).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let capture = capture::dxgi::start(&device, &output, CaptureOptions::default()).unwrap();

    let frame = next_frame(&capture, Duration::from_secs(5));
    let desktop = output.info.desktop;
    assert_eq!(
        (frame.width as i32, frame.height as i32),
        (desktop.right - desktop.left, desktop.bottom - desktop.top)
    );
    let (x, y) = client_center_on_screen(&window);
    let (x, y) = ((x - desktop.left) as u32, (y - desktop.top) as u32);
    let [b, g, r, _] = gpu::read_pixel(&device, &frame.texture, x, y).unwrap();
    let close = |actual: u8, expected: u8| actual.abs_diff(expected) <= 3;
    assert!(
        close(r, 220) && close(g, 60) && close(b, 120),
        "rgb = {r}, {g}, {b} at ({x}, {y})"
    );

    drop(frame);
    let report = capture.stop();
    assert_eq!(report.end, CaptureEnd::Stopped);
    assert_eq!(report.backend, "dxgi");
    assert_no_debug_problems(&device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn sustained_duplication_does_not_leak_handles() {
    let (device, output) = device_and_output();
    let window = TestWindow::open("nitra dxgi soak", 800, 600, [60, 140, 90], true).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let capture = capture::dxgi::start(&device, &output, CaptureOptions::default()).unwrap();

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
    drop(window);
    assert!(
        after <= before + 3,
        "handles grew from {before} to {after} over {delivered} frames"
    );
    let counters = report.counters;
    assert!(
        delivered >= 200,
        "only {delivered} frames in 10 s: {counters:?}"
    );
    assert_eq!(counters.pool_dropped, 0, "{counters:?}");
    assert_eq!(counters.access_lost, 0, "{counters:?}");
    assert_no_debug_problems(&device);
}
