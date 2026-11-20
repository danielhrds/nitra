//! `bench loopback`: the whole path in one process, host and viewer on the
//! same GPU and clock.
//!
//! ```text
//! source thread: synthetic stamped frames -> NV12 -> "source" presenter window
//! host pipeline: WGC capture of that window -> NV12 -> hardware encoder
//! main thread:   access units -> hardware decoder -> "viewer" presenter window
//! probe:         WGC capture of the viewer window -> stamp of what is on screen
//! ```
//!
//! Each synthetic frame carries in its stamp the QPC time it was rendered,
//! so latency needs no clock sync:
//! - to decoded: decoder hand-off minus stamp time, read from a small region
//!   of every Nth decoded picture;
//! - to displayed: the refresh (vblank) at which the viewer window showed
//!   a sampled frame, from the swap chain's DXGI frame statistics, minus
//!   its stamp time (no probe involved);
//! - to screen: arrival of a probe capture of the viewer window minus the
//!   stamp time read from it (includes both windows' composition and the
//!   probe capture itself).
//!
//! With `--monitor N` the host captures a real monitor instead (no stamps:
//! only per-stage times and counters).

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use capture::synthetic::{Pacer, PatternMode, SyntheticConfig, SyntheticSource, read_bgra_stamp};
use capture::wgc::CaptureTarget;
use capture::{CaptureOptions, CounterSnapshot, Taken};
use decode::media_foundation::MediaFoundationDecoder;
use decode::{DecodeSubmit, DecodedFrame, VideoDecoder};
use encode::{EncoderConfig, EncoderPreference, EncoderSelection};
use gpu::convert::{ConvertConfig, Converter, ShaderConverter, read_nv12_luma_region};
use gpu::{D3d11Device, DebugLayer};
use pipeline::{HostPipeline, PipelineConfig, PipelineCounters, Received, Until};
use present::{
    DisplayCounters, DisplayReport, FrameMeta, PresenterCounters, PresenterOptions, VideoPresenter,
};
use serde::Serialize;
use telemetry::frame_code::{self, read_frame_stamp};
use telemetry::{QpcDuration, Stage, Summary, qpc_frequency, qpc_now};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

use crate::com::ComApartment;
use crate::process_stats::{ProcessStats, cpu_percent_of_one_core};

/// Probe captures whose stamp is read, at most this often (each read
/// stalls the GPU briefly).
const PROBE_INTERVAL: Duration = Duration::from_millis(50);

pub enum LoopbackSource {
    /// Synthetic stamped frames shown in a source window of this size.
    Synthetic { width: u32, height: u32 },
    /// A real monitor (by `bench probe` index).
    Monitor(usize),
}

pub struct LoopbackCommand {
    pub source: LoopbackSource,
    pub encoder: EncoderPreference,
    pub fps: u32,
    pub seconds: u32,
    pub bitrate_bps: u32,
    /// WGC `MinUpdateInterval` as a fraction of the frame period (host capture).
    pub wgc_interval_factor: f64,
    /// Read the stamp of every Nth decoded picture (0 = never).
    pub decoded_stamp_every: u32,
    /// Probe the viewer window (synthetic source only).
    pub probe: bool,
    /// Per-frame QPC times of the viewer.
    pub display_csv: Option<PathBuf>,
    pub summary_json: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct LoopbackSummary {
    schema_version: u32,
    source: String,
    encoder: Option<EncoderSelection>,
    size: [u32; 2],
    fps: u32,
    target_bitrate_bps: u32,
    wgc_interval_factor: f64,
    build_profile: &'static str,
    seconds: f64,
    /// Synthetic frame render -> picture out of the decoder (CPU hand-off).
    latency_to_decoded_ms: Option<Summary>,
    /// Synthetic frame render -> decoded picture readable on the GPU (the
    /// stamp read waits for the decode to finish), same sampled frames.
    latency_to_decoded_on_gpu_ms: Option<Summary>,
    decoded_stamps: StampCount,
    /// Synthetic frame render -> refresh at which the viewer window showed
    /// it (DXGI frame statistics), same sampled frames.
    latency_to_displayed_ms: Option<Summary>,
    /// Synthetic frame render -> refresh at which the source window showed
    /// it (where the host capture can first see it), every frame.
    source_to_displayed_ms: Option<Summary>,
    source_display: Option<DisplayCounters>,
    /// Synthetic frame render -> probe capture of the viewer window arrived.
    latency_to_screen_ms: Option<Summary>,
    screen_stamps: StampCount,
    host: HostSide,
    viewer: ViewerSide,
    process_cpu_percent: f64,
    achieved_bitrate_bps: f64,
}

#[derive(Debug, Default, Serialize)]
struct StampCount {
    read: u64,
    unreadable: u64,
}

#[derive(Debug, Serialize)]
struct HostSide {
    capture: CounterSnapshot,
    pipeline: PipelineCounters,
    capture_cost_us: Option<Summary>,
    convert_gpu_us: Option<Summary>,
    submit_cpu_us: Option<Summary>,
    encode_latency_us: Option<Summary>,
}

#[derive(Debug, Serialize)]
struct ViewerSide {
    access_units: u64,
    decoded: u64,
    decoder_busy_retries: u64,
    /// Submit to the decoder -> picture out (CPU hand-off).
    decode_us: Option<Summary>,
    presenter: PresenterCounters,
    /// Decoder hand-off -> `Present` call.
    present_us: Option<Summary>,
    /// Decoder hand-off -> refresh at which the frame was shown.
    decoded_to_displayed_ms: Option<Summary>,
    /// `Present` call -> refresh at which the frame was shown.
    present_to_displayed_ms: Option<Summary>,
    display: DisplayCounters,
    refresh_period_us: Option<f64>,
    probe: Option<CounterSnapshot>,
}

/// Milliseconds as a `Summary` built from `QpcDuration`s (its fields are
/// named `_us`; here they hold milliseconds).
fn summary_ms(samples: &[QpcDuration]) -> Option<Summary> {
    let mut summary = Summary::from_durations(samples, qpc_frequency())?;
    for value in [
        &mut summary.mean_us,
        &mut summary.min_us,
        &mut summary.p50_us,
        &mut summary.p95_us,
        &mut summary.p99_us,
        &mut summary.max_us,
    ] {
        *value /= 1000.0;
    }
    Some(summary)
}

/// Shows synthetic stamped frames in the source window until `stop`.
fn source_thread(
    device: D3d11Device,
    width: u32,
    height: u32,
    fps: u32,
    stop: Arc<AtomicBool>,
    window: mpsc::SyncSender<Result<isize, String>>,
) -> Result<DisplayReport, String> {
    let started = (|| -> Result<(SyntheticSource, ShaderConverter, VideoPresenter), String> {
        let source = SyntheticSource::new(
            &device,
            SyntheticConfig {
                width,
                height,
                mode: PatternMode::Motion,
            },
        )
        .map_err(|error| format!("synthetic source: {error}"))?;
        let converter = ShaderConverter::new(
            &device,
            ConvertConfig {
                input_width: width,
                input_height: height,
                output_width: width,
                output_height: height,
                pool_size: 4,
            },
        )
        .map_err(|error| format!("converter: {error}"))?;
        let presenter = VideoPresenter::open(
            &device,
            PresenterOptions {
                title: "ziz loopback source".to_owned(),
                width,
                height,
                borderless: true,
                overlay: false,
                parent: None,
                position: None,
                background: false,
                skip_when_hidden: false,
            },
        )
        .map_err(|error| format!("source window: {error}"))?;
        Ok((source, converter, presenter))
    })();
    let (mut source, mut converter, mut presenter) = match started {
        Ok(parts) => parts,
        Err(message) => {
            let _ = window.send(Err(message.clone()));
            return Err(message);
        }
    };
    let _ = window.send(Ok(presenter.hwnd().0 as isize));
    let mut pacer = Pacer::start_now(fps);
    while !stop.load(Ordering::Relaxed) && !presenter.is_closed() {
        let slot = pacer.next_slot(qpc_now());
        pacer.wait(&slot);
        let frame = source.render_next().map_err(|e| e.to_string())?;
        let Some(nv12) = converter
            .convert(&frame.texture)
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        presenter
            .present(
                &nv12,
                0,
                width,
                height,
                FrameMeta {
                    frame_id: u64::from(frame.stamp.frame_id),
                    captured_at: frame.stamp.timestamp,
                    decoded_at: frame.stamp.timestamp,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    let report = presenter.close();
    // `decoded_at` of the source frames is their render time.
    report.error.map_or(Ok(report.display), Err)
}

pub fn run(command: LoopbackCommand) -> Result<(), Box<dyn Error>> {
    // Window sizes in physical pixels, 1:1 with the frames.
    // SAFETY: process-wide setting with a constant argument; fails
    // harmlessly if already set.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let _com = ComApartment::multithreaded()?;
    let _media_foundation = mf::MediaFoundation::start()?;
    let adapters = gpu::enumerate_adapters()?;
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .ok_or("no hardware adapter found")?;
    let debug_layer = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };
    let device = gpu::create_device(adapter, debug_layer)?;
    let capture_options = CaptureOptions {
        fps: command.fps,
        cursor: false,
        hide_border: true,
        wgc_interval_factor: command.wgc_interval_factor,
        ..CaptureOptions::default()
    };

    // The host's source: a synthetic window or a monitor.
    let stop_source = Arc::new(AtomicBool::new(false));
    let mut source_thread_handle = None;
    let (capture, size, source_name, synthetic) = match command.source {
        LoopbackSource::Synthetic { width, height } => {
            let (window_sender, window_receiver) = mpsc::sync_channel(1);
            let (thread_device, stop, fps) =
                (device.clone(), Arc::clone(&stop_source), command.fps);
            source_thread_handle = Some(
                std::thread::Builder::new()
                    .name("loopback-source".to_owned())
                    .spawn(move || {
                        source_thread(thread_device, width, height, fps, stop, window_sender)
                    })?,
            );
            let window = window_receiver
                .recv()
                .map_err(|_| "source thread exited")??;
            std::thread::sleep(Duration::from_millis(300));
            let capture = capture::wgc::start(
                &device,
                CaptureTarget::Window(windows::Win32::Foundation::HWND(window as *mut _)),
                capture_options,
            )?;
            (
                capture,
                (width, height),
                format!("synthetic window {width}x{height}"),
                true,
            )
        }
        LoopbackSource::Monitor(index) => {
            let outputs: Vec<_> = adapters
                .iter()
                .filter(|adapter| !adapter.info.is_software)
                .flat_map(|adapter| adapter.outputs.iter())
                .collect();
            let output = outputs
                .get(index)
                .ok_or_else(|| format!("no monitor {index}; {} found", outputs.len()))?;
            let desktop = output.info.desktop;
            let size = (
                u32::try_from(desktop.right - desktop.left)?,
                u32::try_from(desktop.bottom - desktop.top)?,
            );
            let (capture, _) = capture::select::start_monitor(
                &device,
                output,
                capture_options,
                capture::select::CapturePreference::Auto,
            )?;
            (
                capture,
                size,
                format!("monitor {index} ({})", output.info.device_name),
                false,
            )
        }
    };

    let pipeline = HostPipeline::start(
        &device,
        &adapter.info,
        capture,
        PipelineConfig {
            encoder: EncoderConfig {
                width: size.0,
                height: size.1,
                fps: command.fps,
                bitrate_bps: command.bitrate_bps,
                keyframe_interval: command.fps * 10,
                max_in_flight: 4,
            },
            encoder_preference: command.encoder,
            until: Until::Encode,
            output_capacity: 8,
            low_priority: false,
        },
    )?;
    let encoder = pipeline.selection().cloned();
    let mut decoder = MediaFoundationDecoder::new(&device)?;
    let mut viewer = VideoPresenter::open(
        &device,
        PresenterOptions {
            title: "ziz loopback viewer".to_owned(),
            width: size.0,
            height: size.1,
            // 1:1 and frameless, so the probe reads the stamp at (0, 0).
            borderless: synthetic,
            overlay: true,
            parent: None,
            position: None,
            background: false,
            skip_when_hidden: false,
        },
    )?;
    let probe = if synthetic && command.probe {
        Some(capture::wgc::start(
            &device,
            CaptureTarget::Window(viewer.hwnd()),
            CaptureOptions {
                fps: 240,
                cursor: false,
                hide_border: true,
                ..CaptureOptions::default()
            },
        )?)
    } else {
        None
    };
    println!(
        "loopback: {source_name} (WGC interval {:.2} of the period) -> {} -> decode -> viewer, {}x{} @ {} fps, {} kbps, {} s",
        command.wgc_interval_factor,
        encoder.as_ref().map_or("?".to_owned(), |selection| format!(
            "{} ({})",
            selection.kind, selection.detail
        )),
        size.0,
        size.1,
        command.fps,
        command.bitrate_bps / 1000,
        command.seconds
    );

    let resources_start = ProcessStats::current()?;
    let started = Instant::now();
    let duration = Duration::from_secs(u64::from(command.seconds));
    let mut viewer_state = Viewer {
        device: &device,
        synthetic,
        decoded_stamp_every: command.decoded_stamp_every,
        decoded: 0,
        decode_latencies: Vec::new(),
        to_decoded: Vec::new(),
        to_decoded_on_gpu: Vec::new(),
        decoded_stamps: StampCount::default(),
        stamps: HashMap::new(),
    };
    let mut access_units = 0_u64;
    let mut bytes = 0_u64;
    let mut busy_retries = 0_u64;
    let mut to_screen = Vec::new();
    let mut screen_stamps = StampCount::default();
    let mut next_probe = Instant::now();
    while started.elapsed() < duration && !viewer.is_closed() {
        match pipeline.recv(Duration::from_millis(2)) {
            Received::Chunk(chunk) => {
                access_units += 1;
                bytes += chunk.data.len() as u64;
                while decoder.submit(&chunk.data, chunk.frame_id, chunk.captured_at)?
                    == DecodeSubmit::Busy
                {
                    busy_retries += 1;
                    while let Some(frame) = decoder.poll()? {
                        viewer_state.show(frame, &mut viewer)?;
                    }
                }
                while let Some(frame) = decoder.poll()? {
                    viewer_state.show(frame, &mut viewer)?;
                }
            }
            Received::Timeout => {}
            Received::Finished => break,
        }
        if let Some(probe) = &probe {
            while let Taken::Value(captured) = probe.next(Duration::ZERO) {
                let arrived = qpc_now();
                if Instant::now() < next_probe {
                    continue;
                }
                next_probe = Instant::now() + PROBE_INTERVAL;
                match read_bgra_stamp(&device, &captured.texture) {
                    Ok(stamp) => {
                        screen_stamps.read += 1;
                        to_screen.push(arrived.duration_since(stamp.timestamp));
                    }
                    Err(_) => screen_stamps.unreadable += 1,
                }
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let resources_end = ProcessStats::current()?;
    let probe_counters = probe.map(|probe| probe.stop().counters);
    let (host_report, remaining) = pipeline.stop();
    for chunk in remaining {
        access_units += 1;
        bytes += chunk.data.len() as u64;
        let _ = decoder.submit(&chunk.data, chunk.frame_id, chunk.captured_at)?;
    }
    for frame in decoder.drain()? {
        viewer_state.show(frame, &mut viewer)?;
    }
    let viewer_report = viewer.close();
    stop_source.store(true, Ordering::Relaxed);
    let source_display = match source_thread_handle {
        Some(handle) => Some(
            handle
                .join()
                .map_err(|_| "source thread panicked")?
                .map_err(|error| format!("source window: {error}"))?,
        ),
        None => None,
    };

    let frequency = qpc_frequency();
    let display = &viewer_report.display;
    let mut to_displayed = Vec::new();
    let mut decoded_to_displayed = Vec::new();
    let mut present_to_displayed = Vec::new();
    for frame in &display.frames {
        decoded_to_displayed.push(frame.displayed_at.duration_since(frame.decoded_at));
        present_to_displayed.push(frame.displayed_at.duration_since(frame.presented_at));
        if let Some(rendered_at) = viewer_state.stamps.get(&frame.frame_id) {
            to_displayed.push(frame.displayed_at.duration_since(*rendered_at));
        }
    }
    let source_to_displayed: Vec<_> = source_display
        .iter()
        .flat_map(|report| &report.frames)
        .map(|frame| frame.displayed_at.duration_since(frame.decoded_at))
        .collect();
    let summary = LoopbackSummary {
        schema_version: 2,
        source: source_name,
        encoder,
        size: [size.0, size.1],
        fps: command.fps,
        target_bitrate_bps: command.bitrate_bps,
        wgc_interval_factor: command.wgc_interval_factor,
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        seconds: elapsed,
        latency_to_decoded_ms: summary_ms(&viewer_state.to_decoded),
        latency_to_decoded_on_gpu_ms: summary_ms(&viewer_state.to_decoded_on_gpu),
        decoded_stamps: viewer_state.decoded_stamps,
        latency_to_displayed_ms: summary_ms(&to_displayed),
        source_to_displayed_ms: summary_ms(&source_to_displayed),
        source_display: source_display.as_ref().map(|report| report.counters),
        latency_to_screen_ms: summary_ms(&to_screen),
        screen_stamps,
        host: HostSide {
            capture: host_report.capture.counters,
            pipeline: host_report.counters,
            capture_cost_us: Summary::from_durations(&host_report.capture.cost, frequency),
            convert_gpu_us: Summary::from_durations(
                host_report.stages.samples(Stage::Convert),
                frequency,
            ),
            submit_cpu_us: Summary::from_durations(
                host_report.stages.samples(Stage::Submit),
                frequency,
            ),
            encode_latency_us: Summary::from_durations(
                host_report.stages.samples(Stage::EncodeOutput),
                frequency,
            ),
        },
        viewer: ViewerSide {
            access_units,
            decoded: viewer_state.decoded,
            decoder_busy_retries: busy_retries,
            decode_us: Summary::from_durations(&viewer_state.decode_latencies, frequency),
            presenter: viewer_report.counters,
            present_us: Summary::from_durations(
                viewer_report.stages.samples(Stage::Present),
                frequency,
            ),
            decoded_to_displayed_ms: summary_ms(&decoded_to_displayed),
            present_to_displayed_ms: summary_ms(&present_to_displayed),
            display: display.counters,
            refresh_period_us: display.refresh_period_us,
            probe: probe_counters,
        },
        process_cpu_percent: cpu_percent_of_one_core(&resources_start, &resources_end, elapsed),
        achieved_bitrate_bps: bytes as f64 * 8.0 / elapsed,
    };

    let print_ms = |name: &str, stats: &Option<Summary>| match stats {
        Some(s) => println!(
            "  {name:<20} (ms): mean {:.2}, p50 {:.2}, p95 {:.2}, p99 {:.2}, max {:.2} ({} samples)",
            s.mean_us, s.p50_us, s.p95_us, s.p99_us, s.max_us, s.count
        ),
        None => println!("  {name:<20}: no samples"),
    };
    let print_us = |name: &str, stats: &Option<Summary>| match stats {
        Some(s) => println!(
            "  {name:<20} (us): mean {:.1}, p50 {:.1}, p95 {:.1}, p99 {:.1}, max {:.1}",
            s.mean_us, s.p50_us, s.p95_us, s.p99_us, s.max_us
        ),
        None => println!("  {name:<20}: no samples"),
    };
    print_ms("render -> decoded", &summary.latency_to_decoded_ms);
    print_ms("render -> on GPU", &summary.latency_to_decoded_on_gpu_ms);
    print_ms("render -> displayed", &summary.latency_to_displayed_ms);
    print_ms("render -> screen", &summary.latency_to_screen_ms);
    print_ms("render -> source shown", &summary.source_to_displayed_ms);
    print_ms(
        "decoded -> displayed",
        &summary.viewer.decoded_to_displayed_ms,
    );
    print_ms(
        "present -> displayed",
        &summary.viewer.present_to_displayed_ms,
    );
    println!(
        "  display (viewer): {:?}, refresh period {:.0} us; source: {:?}",
        summary.viewer.display,
        summary.viewer.refresh_period_us.unwrap_or(0.0),
        summary.source_display
    );
    println!(
        "  stamps: decoded {} read / {} unreadable, screen {} read / {} unreadable",
        summary.decoded_stamps.read,
        summary.decoded_stamps.unreadable,
        summary.screen_stamps.read,
        summary.screen_stamps.unreadable
    );
    print_us("host capture cost", &summary.host.capture_cost_us);
    print_us("host convert GPU", &summary.host.convert_gpu_us);
    print_us("host submit CPU", &summary.host.submit_cpu_us);
    print_us("host encode latency", &summary.host.encode_latency_us);
    print_us("viewer decode", &summary.viewer.decode_us);
    print_us("viewer present", &summary.viewer.present_us);
    let host = &summary.host.pipeline;
    println!(
        "  host: {} taken, {} submitted, {} busy, {} held replaced, {} access units, {} queue-full drops",
        host.frames_taken,
        host.submitted,
        host.busy,
        host.held_replaced,
        host.access_units,
        host.output_full_dropped
    );
    let viewer_counters = &summary.viewer.presenter;
    println!(
        "  viewer: {} access units, {} decoded, {} decoder busy, presenter {} presented / {} replaced / {} dropped",
        summary.viewer.access_units,
        summary.viewer.decoded,
        summary.viewer.decoder_busy_retries,
        viewer_counters.presented,
        viewer_counters.replaced,
        viewer_counters.pool_dropped
    );
    println!(
        "  process CPU: {:.1}% of one core; {:.2} Mbit/s",
        summary.process_cpu_percent,
        summary.achieved_bitrate_bps / 1e6
    );
    if let Some(error) = &host_report.error {
        println!("  host error: {error}");
    }
    if let Some(error) = &viewer_report.error {
        println!("  viewer error: {error}");
    }
    let messages = device.take_debug_messages();
    if device.debug_layer_enabled() {
        let problems: Vec<_> = messages
            .iter()
            .filter(|message| message.severity.is_problem())
            .collect();
        println!(
            "  debug layer: {} message(s), {} warning/error",
            messages.len(),
            problems.len()
        );
        for message in &problems {
            println!("    {:?}: {}", message.severity, message.text);
        }
    }
    if let Some(path) = &command.display_csv {
        let mut csv =
            String::from("frame_id,rendered_qpc,decoded_qpc,presented_qpc,dxgi_displayed_qpc\n");
        for frame in &display.frames {
            let rendered = viewer_state
                .stamps
                .get(&frame.frame_id)
                .map_or(String::new(), |time| time.ticks().to_string());
            csv.push_str(&format!(
                "{},{rendered},{},{},{}\n",
                frame.frame_id,
                frame.decoded_at.ticks(),
                frame.presented_at.ticks(),
                frame.displayed_at.ticks()
            ));
        }
        std::fs::write(path, csv)?;
        println!("  display CSV: {}", path.display());
    }
    if let Some(json) = &command.summary_json {
        std::fs::write(json, serde_json::to_string_pretty(&summary)?)?;
        println!("  summary: {}", json.display());
    }
    match (host_report.error, viewer_report.error) {
        (Some(error), _) | (None, Some(error)) => Err(error.into()),
        (None, None) => Ok(()),
    }
}

/// The viewer side: decoded pictures go to the viewer presenter; every Nth
/// one's stamp is read first (synthetic source).
struct Viewer<'a> {
    device: &'a D3d11Device,
    synthetic: bool,
    decoded_stamp_every: u32,
    decoded: u64,
    decode_latencies: Vec<QpcDuration>,
    to_decoded: Vec<QpcDuration>,
    to_decoded_on_gpu: Vec<QpcDuration>,
    decoded_stamps: StampCount,
    /// Render time of the sampled frames, by decoded frame id (bounded).
    stamps: HashMap<u64, telemetry::QpcTimestamp>,
}

/// Sampled frames whose render time is kept (an hour at 60 fps, 1 in 10).
const MAX_STAMPS: usize = 1 << 15;

impl Viewer<'_> {
    fn show(
        &mut self,
        frame: DecodedFrame,
        viewer: &mut VideoPresenter,
    ) -> Result<(), Box<dyn Error>> {
        self.decoded += 1;
        self.decode_latencies
            .push(frame.decoded_at.duration_since(frame.submitted_at));
        if self.synthetic
            && self.decoded_stamp_every > 0
            && self
                .decoded
                .is_multiple_of(u64::from(self.decoded_stamp_every))
        {
            let width = frame_code::CODE_WIDTH;
            let luma = read_nv12_luma_region(
                self.device,
                &frame.texture,
                frame.slice,
                width,
                frame_code::CODE_HEIGHT,
            )?;
            // The read waited for the GPU to finish decoding this picture.
            let readable = qpc_now();
            match read_frame_stamp(|x, y| luma[(y * width + x) as usize]) {
                Ok(stamp) => {
                    self.decoded_stamps.read += 1;
                    self.to_decoded
                        .push(frame.decoded_at.duration_since(stamp.timestamp));
                    self.to_decoded_on_gpu
                        .push(readable.duration_since(stamp.timestamp));
                    if self.stamps.len() < MAX_STAMPS {
                        self.stamps.insert(frame.frame_id, stamp.timestamp);
                    }
                }
                Err(_) => self.decoded_stamps.unreadable += 1,
            }
        }
        viewer.present(
            &frame.texture,
            frame.slice,
            frame.width,
            frame.height,
            FrameMeta {
                frame_id: frame.frame_id,
                captured_at: frame.captured_at,
                decoded_at: frame.decoded_at,
            },
        )?;
        Ok(())
    }
}
