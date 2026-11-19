//! The host pipeline on a real GPU, capturing an animated test window with
//! WGC. Ignored by default; run with `scripts/gpu-tests.ps1`.

use std::time::{Duration, Instant};

use capture::CaptureOptions;
use capture::test_window::TestWindow;
use capture::wgc::CaptureTarget;
use encode::{EncodedChunk, EncoderConfig, EncoderKind, EncoderPreference};
use gpu::{Adapter, D3d11Device, DebugLayer, Vendor};
use pipeline::{HostPipeline, PipelineConfig, Received, Until};
use telemetry::Stage;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

struct Rig {
    // Field order is drop order: the pipeline goes before the window.
    pipeline: Option<HostPipeline>,
    _window: TestWindow,
    device: D3d11Device,
    adapter: Adapter,
}

fn rig(until: Until, output_capacity: usize) -> Rig {
    // SAFETY: joins the MTA for this test thread (it stays joined).
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let adapter = gpu::enumerate_adapters()
        .unwrap()
        .into_iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    let device = gpu::create_device(&adapter, DebugLayer::IfAvailable).unwrap();
    let window = TestWindow::open(
        "ziz pipeline test",
        WIDTH as i32,
        HEIGHT as i32,
        [40, 90, 160],
        true,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let capture = capture::wgc::start(
        &device,
        CaptureTarget::Window(window.hwnd()),
        CaptureOptions::default(),
    )
    .unwrap();
    let pipeline = HostPipeline::start(
        &device,
        &adapter.info,
        capture,
        PipelineConfig {
            encoder: EncoderConfig {
                width: WIDTH,
                height: HEIGHT,
                fps: 60,
                bitrate_bps: 6_000_000,
                keyframe_interval: 600,
                max_in_flight: 4,
            },
            encoder_preference: EncoderPreference::Auto,
            until,
            output_capacity,
        },
    )
    .unwrap();
    Rig {
        pipeline: Some(pipeline),
        _window: window,
        device,
        adapter,
    }
}

impl Rig {
    fn pipeline(&self) -> &HostPipeline {
        self.pipeline.as_ref().expect("running")
    }

    /// Receives access units for `duration`.
    fn receive_for(&self, duration: Duration) -> Vec<EncodedChunk> {
        let deadline = Instant::now() + duration;
        let mut chunks = Vec::new();
        while Instant::now() < deadline {
            match self.pipeline().recv(Duration::from_millis(20)) {
                Received::Chunk(chunk) => chunks.push(chunk),
                Received::Timeout => {}
                Received::Finished => panic!("pipeline finished early"),
            }
        }
        chunks
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
#[ignore = "requires a GPU with a hardware H.264 encoder"]
fn delivers_frames_in_order_with_requested_keyframe() {
    let mut rig = rig(Until::Encode, 8);
    let selection = rig.pipeline().selection().cloned().expect("encoder");
    eprintln!("encoder: {selection:?}");
    if rig.adapter.info.vendor == Vendor::Amd {
        assert_eq!(selection.kind, EncoderKind::Amf);
    }
    let mut chunks = rig.receive_for(Duration::from_secs(1));
    let requested_after = chunks.last().map_or(0, |chunk| chunk.frame_id);
    rig.pipeline().request_keyframe();
    chunks.extend(rig.receive_for(Duration::from_secs(2)));
    let (report, remaining) = rig.pipeline.take().unwrap().stop();
    chunks.extend(remaining);

    eprintln!("counters: {:?}", report.counters);
    assert!(report.error.is_none(), "{:?}", report.error);
    assert!(chunks.len() >= 60, "{} access units in 3 s", chunks.len());
    assert!(chunks[0].keyframe, "stream must start with a keyframe");
    assert!(
        chunks
            .windows(2)
            .all(|pair| pair[0].frame_id < pair[1].frame_id),
        "frame ids must increase"
    );
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk.keyframe && chunk.frame_id > requested_after),
        "requested keyframe did not come"
    );
    assert_eq!(report.counters.output_full_dropped, 0);
    assert_eq!(report.counters.delivered as usize, chunks.len());
    assert!(!report.stages.samples(Stage::EncodeOutput).is_empty());
    assert!(!report.stages.samples(Stage::Convert).is_empty());
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU with a hardware H.264 encoder"]
fn slow_consumer_gets_a_keyframe_after_the_gap() {
    const CAPACITY: usize = 4;
    let mut rig = rig(Until::Encode, CAPACITY);
    std::thread::sleep(Duration::from_millis(1500));
    let chunks = rig.receive_for(Duration::from_millis(1500));
    let (report, _) = rig.pipeline.take().unwrap().stop();
    eprintln!("counters: {:?}", report.counters);
    assert!(
        report.counters.output_full_dropped > 0,
        "queue never overflowed"
    );
    assert!(
        chunks.len() > CAPACITY + 10,
        "{} access units",
        chunks.len()
    );
    // The queued units from before the stall come first; the next one must
    // be a keyframe so the stream can be decoded across the gap.
    assert!(chunks[CAPACITY].keyframe, "no keyframe after the gap");
    assert!(chunks[0].keyframe);
}

#[test]
#[ignore = "requires a GPU with a hardware H.264 encoder"]
fn sustained_pipeline_does_not_leak_handles() {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let handles = || {
        let mut count = 0;
        // SAFETY: pseudo-handle of the current process and a live out-pointer.
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }.unwrap();
        count
    };
    let mut rig = rig(Until::Encode, 8);
    let warm_up = rig.receive_for(Duration::from_secs(2));
    let before = handles();
    let steady = rig.receive_for(Duration::from_secs(8));
    let after = handles();
    let (report, _) = rig.pipeline.take().unwrap().stop();
    assert!(
        !warm_up.is_empty() && steady.len() > 200,
        "{}",
        steady.len()
    );
    assert!(report.error.is_none(), "{:?}", report.error);
    assert!(after <= before + 5, "handles {before} -> {after}");
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU"]
fn convert_only_runs_without_an_encoder() {
    let mut rig = rig(Until::Convert, 8);
    assert!(rig.pipeline().selection().is_none());
    assert!(rig.receive_for(Duration::from_secs(2)).is_empty());
    let (report, remaining) = rig.pipeline.take().unwrap().stop();
    assert!(remaining.is_empty());
    assert!(report.error.is_none(), "{:?}", report.error);
    assert!(report.counters.converted > 30, "{:?}", report.counters);
    assert_eq!(report.counters.access_units, 0);
    assert!(!report.stages.samples(Stage::Convert).is_empty());
    assert_no_debug_problems(&rig.device);
}
