//! The resize probe on `VideoPresenter` (its own window and render thread),
//! for comparison with the Dear ImGui prototype's same-window video.

use std::time::{Duration, Instant};

use feed::resize_probe::ResizeProbe;
use feed::{Feed, FeedConfig, MediaThread};
use present::{FrameMeta, PresenterOptions, VideoPresenter};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos, WINDOW_EX_STYLE,
    WS_OVERLAPPEDWINDOW,
};

const SIZES: [(i32, i32); 6] = [
    (1280, 720),
    (600, 900),
    (1600, 500),
    (900, 900),
    (700, 1000),
    (1400, 600),
];

fn main() -> Result<(), String> {
    let seconds: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8.0);
    let _media = MediaThread::start()?;
    let adapters = gpu::enumerate_adapters().map_err(|e| e.to_string())?;
    let adapter = adapters
        .iter()
        .find(|a| !a.info.is_software)
        .ok_or("no adapter")?;
    let device = gpu::create_device(adapter, gpu::DebugLayer::Off).map_err(|e| e.to_string())?;
    let mut presenter = VideoPresenter::open(
        &device,
        PresenterOptions {
            title: "presenter resize probe".to_owned(),
            width: 1280,
            height: 720,
            borderless: false,
            overlay: false,
            parent: None,
        },
    )
    .map_err(|e| e.to_string())?;
    let mut feed = Feed::start(&device, &adapter.info, FeedConfig::default())?;
    let started = Instant::now();
    let mut probe: Option<ResizeProbe> = None;
    let mut resizes = 0_usize;
    while started.elapsed().as_secs_f64() < seconds {
        if let Some(frame) = feed.poll()? {
            presenter
                .present(
                    &frame.texture,
                    frame.slice,
                    frame.width,
                    frame.height,
                    FrameMeta {
                        frame_id: frame.frame_id,
                        captured_at: frame.captured_at,
                        decoded_at: frame.decoded_at,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        let elapsed = started.elapsed();
        if elapsed > Duration::from_secs(1) {
            let probe = match &mut probe {
                Some(probe) => probe,
                None => probe.insert(ResizeProbe::start(&device, presenter.hwnd(), (1920, 1080))?),
            };
            probe.check();
            let due = ((elapsed - Duration::from_secs(1)).as_millis() / 50) as usize;
            if due >= resizes {
                let (w, h) = SIZES[resizes % SIZES.len()];
                let mut rect = RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                };
                // SAFETY: SetWindowPos may target another thread's window.
                unsafe {
                    let _ = AdjustWindowRectEx(
                        &mut rect,
                        WS_OVERLAPPEDWINDOW,
                        false,
                        WINDOW_EX_STYLE(0),
                    );
                    let _ = SetWindowPos(
                        presenter.hwnd(),
                        None,
                        0,
                        0,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                resizes += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if let Some(probe) = &mut probe {
        probe.check();
        println!(
            "presenter resize test: {resizes} resizes, {:?}",
            probe.counts
        );
    }
    drop(probe);
    println!(
        "feed {:?}, presenter {:?}",
        feed.counters(),
        presenter.close().counters
    );
    Ok(())
}
