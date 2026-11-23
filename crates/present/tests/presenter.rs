//! The video presenter on a real GPU, with a real window. The picture on
//! screen is checked by capturing the presenter's own window with WGC and
//! reading the frame stamp from the capture. Ignored by default; run with
//! `scripts/gpu-tests.ps1`.

use std::time::{Duration, Instant};

use capture::synthetic::{PatternMode, SyntheticConfig, SyntheticSource, read_bgra_stamp};
use capture::wgc::CaptureTarget;
use capture::{CaptureOptions, Taken};
use gpu::convert::{ConvertConfig, Converter, ShaderConverter};
use gpu::{D3d11Device, DebugLayer};
use present::{FrameMeta, Presented, PresenterOptions, VideoPresenter};
use telemetry::qpc_now;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_TOPMOST, SW_MINIMIZE, SW_RESTORE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowPos, ShowWindowAsync,
};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

struct Rig {
    device: D3d11Device,
    source: SyntheticSource,
    converter: ShaderConverter,
}

fn rig() -> Rig {
    // Window sizes in physical pixels (1:1 with the frames), whatever the
    // display scaling; fails harmlessly if already set.
    // SAFETY: process-wide setting with a constant argument.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let adapter = gpu::enumerate_adapters()
        .unwrap()
        .into_iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    let device = gpu::create_device(&adapter, DebugLayer::IfAvailable).unwrap();
    let source = SyntheticSource::new(
        &device,
        SyntheticConfig {
            width: WIDTH,
            height: HEIGHT,
            mode: PatternMode::Motion,
        },
    )
    .unwrap();
    let converter = ShaderConverter::new(
        &device,
        ConvertConfig {
            input_width: WIDTH,
            input_height: HEIGHT,
            output_width: WIDTH,
            output_height: HEIGHT,
            pool_size: 6,
        },
    )
    .unwrap();
    Rig {
        device,
        source,
        converter,
    }
}

impl Rig {
    fn presenter(&self, borderless: bool, overlay: bool) -> VideoPresenter {
        VideoPresenter::open(
            &self.device,
            PresenterOptions {
                title: "nitra presenter test".to_owned(),
                width: WIDTH,
                height: HEIGHT,
                borderless,
                overlay,
                parent: None,
                position: None,
                background: false,
                skip_when_hidden: false,
            },
        )
        .unwrap()
    }

    /// Renders, converts and presents the next synthetic frame; returns its id.
    fn present_next(&mut self, presenter: &mut VideoPresenter) -> (u64, Presented) {
        let frame = self.source.render_next().unwrap();
        let nv12 = self
            .converter
            .convert(&frame.texture)
            .unwrap()
            .expect("free NV12 texture");
        let id = u64::from(frame.stamp.frame_id);
        let result = presenter
            .present(
                &nv12,
                0,
                WIDTH,
                HEIGHT,
                FrameMeta {
                    frame_id: id,
                    captured_at: frame.stamp.timestamp,
                    decoded_at: qpc_now(),
                },
            )
            .unwrap();
        (id, result)
    }

    /// Presents frames at about 60 fps for `duration`, calling `during`
    /// after each one.
    fn play(
        &mut self,
        presenter: &mut VideoPresenter,
        duration: Duration,
        mut during: impl FnMut(&mut VideoPresenter, u64),
    ) {
        let started = Instant::now();
        let mut next = started;
        while started.elapsed() < duration {
            let (id, _) = self.present_next(presenter);
            during(presenter, id);
            next += Duration::from_micros(16_667);
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
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

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn shows_the_presented_frames_on_screen() {
    let mut rig = rig();
    let mut presenter = rig.presenter(true, false);
    rig.play(&mut presenter, Duration::from_millis(300), |_, _| {});
    let capture = capture::wgc::start(
        &rig.device,
        CaptureTarget::Window(presenter.hwnd()),
        CaptureOptions {
            fps: 240,
            cursor: false,
            ..CaptureOptions::default()
        },
    )
    .unwrap();
    let mut seen: Vec<(u64, u32)> = Vec::new();
    let mut unreadable = 0;
    let device = rig.device.clone();
    rig.play(
        &mut presenter,
        Duration::from_millis(1500),
        |_, last_presented| {
            while let Taken::Value(captured) = capture.next(Duration::ZERO) {
                match read_bgra_stamp(&device, &captured.texture) {
                    Ok(stamp) => seen.push((last_presented, stamp.frame_id)),
                    Err(_) => unreadable += 1,
                }
            }
        },
    );
    let report_capture = capture.stop();
    let report = presenter.close();
    eprintln!(
        "presenter {:?}; {} captured stamps, {unreadable} unreadable; capture {:?}",
        report.counters,
        seen.len(),
        report_capture.counters
    );
    assert!(report.error.is_none(), "{:?}", report.error);
    assert!(seen.len() >= 20, "only {} readable captures", seen.len());
    assert!(
        unreadable * 10 <= seen.len(),
        "{unreadable} unreadable captures"
    );
    // What is on screen is what was presented, a few frames behind at most.
    for &(presented, shown) in &seen {
        let shown = u64::from(shown);
        assert!(
            shown <= presented && presented - shown <= 6,
            "screen shows frame {shown} while {presented} was presented"
        );
    }
    assert!(
        seen.windows(2).all(|pair| pair[0].1 <= pair[1].1),
        "frames on screen went backwards"
    );
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn reports_when_frames_reach_the_screen() {
    let mut rig = rig();
    let mut presenter = rig.presenter(true, false);
    // A covered window is not shown, so its frames never reach the screen;
    // a window opened after others may not come to the front by itself.
    // SAFETY: SetWindowPos may target a window of another thread.
    unsafe {
        SetWindowPos(
            presenter.hwnd(),
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    }
    .unwrap();
    rig.play(&mut presenter, Duration::from_secs(2), |_, _| {});
    let report = presenter.close();
    let display = &report.display;
    eprintln!(
        "presenter {:?}; display {:?}, refresh period {:?} us",
        report.counters, display.counters, display.refresh_period_us
    );
    assert!(report.error.is_none(), "{:?}", report.error);
    let presented = report.counters.presented;
    assert!(presented > 60, "{:?}", report.counters);
    // Most frames are matched to the refresh they were shown at.
    assert!(
        display.counters.matched * 10 >= presented * 8,
        "{:?} of {presented} presented",
        display.counters
    );
    assert_eq!(display.counters.implausible, 0, "{:?}", display.counters);
    assert_eq!(display.frames.len() as u64, display.counters.matched);
    let frequency = telemetry::qpc_frequency();
    for frame in &display.frames {
        assert!(frame.displayed_at >= frame.presented_at, "{frame:?}");
        let ms = frame
            .displayed_at
            .duration_since(frame.presented_at)
            .as_micros(frequency)
            / 1000.0;
        assert!(ms < 100.0, "present -> screen {ms} ms ({frame:?})");
    }
    assert!(
        display
            .frames
            .windows(2)
            .all(|pair| pair[0].frame_id < pair[1].frame_id
                && pair[0].displayed_at < pair[1].displayed_at),
        "frames reached the screen out of order"
    );
    let period = display.refresh_period_us.expect("refresh period measured");
    assert!((1_000.0..=50_000.0).contains(&period), "period {period} us");
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn a_burst_replaces_frames_without_queueing() {
    let mut rig = rig();
    let mut presenter = rig.presenter(false, true);
    let mut results = Vec::new();
    for _ in 0..300 {
        results.push(rig.present_next(&mut presenter).1);
    }
    std::thread::sleep(Duration::from_millis(200));
    let report = presenter.close();
    eprintln!("presenter {:?}", report.counters);
    let counters = report.counters;
    assert!(report.error.is_none(), "{:?}", report.error);
    assert_eq!(counters.submitted, 300);
    assert!(!results.contains(&Presented::Closed));
    // Far fewer frames shown than submitted: the rest were replaced or
    // dropped, never queued up.
    assert!(counters.presented < 150, "{counters:?}");
    assert_eq!(
        counters.presented + counters.replaced + counters.pool_dropped,
        counters.submitted,
        "{counters:?}"
    );
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn a_minimized_window_skips_frames_until_restored() {
    let mut rig = rig();
    let mut presenter = VideoPresenter::open(
        &rig.device,
        PresenterOptions {
            title: "nitra presenter test".to_owned(),
            width: WIDTH,
            height: HEIGHT,
            borderless: false,
            overlay: false,
            parent: None,
            position: None,
            background: false,
            skip_when_hidden: true,
        },
    )
    .unwrap();
    let window = presenter.hwnd();
    rig.play(&mut presenter, Duration::from_millis(500), |_, _| {});
    let visible = presenter.counters();
    assert!(visible.presented > 10, "{visible:?}");
    assert_eq!(visible.hidden, 0, "{visible:?}");

    // SAFETY: the window belongs to the presenter's thread; the async
    // variant only posts the request.
    let _ = unsafe { ShowWindowAsync(window, SW_MINIMIZE) };
    rig.play(&mut presenter, Duration::from_secs(1), |_, _| {});
    let minimized = presenter.counters();
    eprintln!("minimized {minimized:?}");
    // A few frames go out before the first present reports the occlusion.
    assert!(
        minimized.presented - visible.presented < 15,
        "{minimized:?}"
    );
    assert!(minimized.hidden > 40, "{minimized:?}");

    // SAFETY: as above.
    let _ = unsafe { ShowWindowAsync(window, SW_RESTORE) };
    rig.play(&mut presenter, Duration::from_secs(1), |_, _| {});
    let report = presenter.close();
    let restored = report.counters;
    eprintln!("restored {restored:?}");
    assert!(report.error.is_none(), "{:?}", report.error);
    assert!(
        restored.presented - minimized.presented > 40,
        "{restored:?}"
    );
    assert!(restored.hidden - minimized.hidden < 15, "{restored:?}");
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn resizing_and_fullscreen_keep_presenting() {
    let mut rig = rig();
    let mut presenter = rig.presenter(false, true);
    let window = presenter.hwnd();
    let started = Instant::now();
    let mut step = 0;
    rig.play(&mut presenter, Duration::from_secs(3), |presenter, _| {
        let due = started.elapsed().as_millis() / 500;
        if due > step {
            step = due;
            match step {
                1 => resize(window, 800, 600),
                2 => resize(window, 1600, 900),
                3 | 4 => presenter.toggle_fullscreen(),
                _ => {}
            }
        }
    });
    let report = presenter.close();
    eprintln!("presenter {:?}", report.counters);
    assert!(report.error.is_none(), "{:?}", report.error);
    assert!(report.counters.presented > 100, "{:?}", report.counters);
    assert_no_debug_problems(&rig.device);
}

fn resize(window: windows::Win32::Foundation::HWND, width: i32, height: i32) {
    // SAFETY: SetWindowPos may target a window of another thread.
    let _ = unsafe {
        SetWindowPos(
            window,
            None,
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn opening_and_closing_does_not_leak_handles() {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let handles = || {
        let mut count = 0;
        // SAFETY: pseudo-handle of the current process and a live out-pointer.
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }.unwrap();
        count
    };
    let mut rig = rig();
    let cycle = |rig: &mut Rig| {
        let mut presenter = rig.presenter(false, true);
        rig.play(&mut presenter, Duration::from_millis(300), |_, _| {});
        let report = presenter.close();
        assert!(report.error.is_none(), "{:?}", report.error);
        assert!(report.counters.presented > 5, "{:?}", report.counters);
    };
    cycle(&mut rig);
    let before = handles();
    for _ in 0..5 {
        cycle(&mut rig);
    }
    let after = handles();
    assert!(after <= before + 5, "handles {before} -> {after}");
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU and a desktop session"]
fn a_resized_window_keeps_the_aspect_ratio() {
    let mut rig = rig();
    let mut presenter = rig.presenter(true, false);
    let window = presenter.hwnd();
    rig.play(&mut presenter, Duration::from_millis(300), |_, _| {});
    // 1000x700 is taller than 16:9: the 1000x562 picture sits between black
    // bars of 69 px at the top and the bottom.
    resize(window, 1000, 700);
    rig.play(&mut presenter, Duration::from_millis(400), |_, _| {});
    let capture = capture::wgc::start(
        &rig.device,
        CaptureTarget::Window(window),
        CaptureOptions {
            fps: 240,
            cursor: false,
            ..CaptureOptions::default()
        },
    )
    .unwrap();
    let device = rig.device.clone();
    let mut samples = None;
    rig.play(&mut presenter, Duration::from_millis(600), |_, _| {
        if samples.is_some() {
            return;
        }
        if let Taken::Value(captured) = capture.next(Duration::ZERO) {
            let pixel = |x, y| gpu::read_pixel(&device, &captured.texture, x, y).unwrap();
            samples = Some((
                (captured.width, captured.height),
                [pixel(500, 10), pixel(500, 690)],
                [pixel(500, 120), pixel(500, 350), pixel(500, 580)],
            ));
        }
    });
    drop(capture);
    let report = presenter.close();
    assert!(report.error.is_none(), "{:?}", report.error);
    let (size, bars, picture) = samples.expect("a capture of the window");
    assert_eq!(size, (1000, 700));
    for bar in bars {
        assert_eq!(&bar[..3], &[0, 0, 0], "bar pixel {bar:?}");
    }
    // The synthetic pattern is colorful wherever the picture is.
    assert!(
        picture.iter().any(|pixel| pixel[..3] != [0, 0, 0]),
        "picture pixels {picture:?}"
    );
    assert_no_debug_problems(&rig.device);
}
