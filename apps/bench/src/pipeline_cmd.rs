//! `bench pipeline`: runs the host pipeline (capture -> NV12 -> encoder
//! thread -> bounded output) on a monitor, a window or the test window,
//! writes the access units to a `.h264` file and reports each stage.
//! With `--until convert` it stops after the NV12 conversion, to measure
//! that stage's cost on a running game.

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use capture::select::{CapturePreference, CaptureSelection, start_monitor};
use capture::test_window::TestWindow;
use capture::wgc::{CaptureTarget, find_window_by_title};
use capture::{CaptureOptions, CounterSnapshot};
use encode::{EncoderConfig, EncoderPreference, EncoderSelection};
use gpu::DebugLayer;
use pipeline::{HostPipeline, PipelineConfig, PipelineCounters, Received, Until};
use serde::Serialize;
use telemetry::{QpcDuration, Stage, Summary, qpc_frequency};

use crate::com::ComApartment;
use crate::process_stats::{ProcessStats, cpu_percent_of_one_core};

pub enum Source {
    Monitor(usize),
    Window(String),
    TestWindow,
}

pub struct PipelineCommand {
    pub source: Source,
    pub capture: CapturePreference,
    pub encoder: EncoderPreference,
    pub until: Until,
    /// Encoded size [default: source size].
    pub output_width: Option<u32>,
    pub output_height: Option<u32>,
    pub fps: u32,
    pub seconds: u32,
    pub bitrate_bps: u32,
    pub gop: u32,
    /// Request a keyframe every N seconds (0 = never).
    pub keyframe_every_s: u32,
    pub wgc_interval_factor: f64,
    pub low_priority: bool,
    pub output: Option<PathBuf>,
    pub summary_json: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct PipelineSummary {
    schema_version: u32,
    until: Until,
    source: String,
    capture: Option<CaptureSelection>,
    encoder: Option<EncoderSelection>,
    output: [u32; 2],
    fps: u32,
    target_bitrate_bps: u32,
    gop: u32,
    wgc_interval_factor: f64,
    low_priority: bool,
    build_profile: &'static str,
    seconds: f64,
    /// Frames taken from the capture per second (same key as `bench capture`).
    taken_fps: f64,
    access_units_per_s: f64,
    /// Over the delivered access units' duration (units / fps).
    achieved_bitrate_bps: f64,
    keyframes: u64,
    /// Whole process, % of one core.
    process_cpu_percent: f64,
    /// The capture's counters (same key as `bench capture`).
    counters: CounterSnapshot,
    pipeline: PipelineCounters,
    capture_cost_us: Option<Summary>,
    convert_gpu_us: Option<Summary>,
    submit_cpu_us: Option<Summary>,
    encode_latency_us: Option<Summary>,
    /// Access unit out minus capture timestamp (signed; WGC's timestamp runs ahead).
    capture_to_output_ms: Option<SignedStats>,
    timer_skipped: u64,
    capture_end: Option<String>,
    error: Option<String>,
    file: Option<String>,
}

#[derive(Debug, Serialize)]
struct SignedStats {
    count: usize,
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
}

fn signed_stats(ticks: &[i64], ticks_per_ms: f64) -> Option<SignedStats> {
    if ticks.is_empty() {
        return None;
    }
    let mut values: Vec<f64> = ticks.iter().map(|&t| t as f64 / ticks_per_ms).collect();
    values.sort_unstable_by(f64::total_cmp);
    let rank = |p: usize| values[(p * values.len()).div_ceil(100).max(1) - 1];
    Some(SignedStats {
        count: values.len(),
        mean: values.iter().sum::<f64>() / values.len() as f64,
        p50: rank(50),
        p95: rank(95),
        p99: rank(99),
    })
}

pub fn run(command: PipelineCommand) -> Result<(), Box<dyn Error>> {
    let _com = ComApartment::multithreaded()?;
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
    let options = CaptureOptions {
        fps: command.fps,
        cursor: false,
        hide_border: true,
        wgc_interval_factor: command.wgc_interval_factor,
        low_priority: command.low_priority,
        ..CaptureOptions::default()
    };

    // Keep the test window alive for the whole run.
    let mut test_window = None;
    let (capture, selection, source, (width, height)) = match &command.source {
        Source::Monitor(index) => {
            let outputs: Vec<_> = adapters
                .iter()
                .filter(|adapter| !adapter.info.is_software)
                .flat_map(|adapter| adapter.outputs.iter())
                .collect();
            let output = outputs
                .get(*index)
                .ok_or_else(|| format!("no monitor {index}; {} found", outputs.len()))?;
            let desktop = output.info.desktop;
            let size = (
                u32::try_from(desktop.right - desktop.left)?,
                u32::try_from(desktop.bottom - desktop.top)?,
            );
            let (capture, selection) = start_monitor(&device, output, options, command.capture)?;
            let name = format!("monitor {index} ({})", output.info.device_name);
            (capture, Some(selection), name, size)
        }
        Source::Window(needle) => {
            let (window, title) = find_window_by_title(needle)
                .ok_or_else(|| format!("no visible window title contains {needle:?}"))?;
            let size = command
                .output_width
                .zip(command.output_height)
                .ok_or("a window source needs --out-width and --out-height")?;
            let capture = capture::wgc::start(&device, CaptureTarget::Window(window), options)?;
            (capture, None, format!("window {title:?}"), size)
        }
        Source::TestWindow => {
            let window = TestWindow::open("nitra pipeline", 1280, 720, [40, 90, 160], true)?;
            std::thread::sleep(Duration::from_millis(300));
            let capture =
                capture::wgc::start(&device, CaptureTarget::Window(window.hwnd()), options)?;
            test_window = Some(window);
            (
                capture,
                None,
                "animated test window".to_owned(),
                (1280, 720),
            )
        }
    };
    let output_width = command.output_width.unwrap_or(width);
    let output_height = command.output_height.unwrap_or(height);
    let pipeline = HostPipeline::start(
        &device,
        &adapter.info,
        capture,
        PipelineConfig {
            encoder: EncoderConfig {
                width: output_width,
                height: output_height,
                fps: command.fps,
                bitrate_bps: command.bitrate_bps,
                keyframe_interval: command.gop,
                max_in_flight: 4,
            },
            encoder_preference: command.encoder,
            until: command.until,
            output_capacity: 8,
            low_priority: command.low_priority,
        },
    )?;
    let build_profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "pipeline: {source} via {} -> {output_width}x{output_height} @ {} fps, until {:?}, encoder {}, {} kbps for {} s ({build_profile})",
        selection
            .as_ref()
            .map_or("wgc".to_owned(), |selection| selection.kind.to_string()),
        command.fps,
        command.until,
        pipeline
            .selection()
            .map_or("none".to_owned(), |selection| format!(
                "{} ({})",
                selection.kind, selection.detail
            )),
        command.bitrate_bps / 1000,
        command.seconds,
    );
    for failed in selection.iter().flat_map(|selection| &selection.failed) {
        println!(
            "  capture fallback: {} failed: {}",
            failed.kind, failed.error
        );
    }
    for failed in pipeline
        .selection()
        .iter()
        .flat_map(|selection| &selection.failed)
    {
        println!(
            "  encoder fallback: {} failed: {}",
            failed.kind, failed.error
        );
    }

    let path = match command.until {
        Until::Encode => Some(command.output.clone().unwrap_or_else(|| {
            telemetry::default_output_dir()
                .join("pipeline")
                .join(format!(
                    "{}-{output_width}x{output_height}-{}kbps.h264",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |since| since.as_millis()),
                    command.bitrate_bps / 1000
                ))
        })),
        Until::Convert => None,
    };
    let mut file = match &path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Some(BufWriter::new(File::create(path)?))
        }
        None => None,
    };

    let resources_start = ProcessStats::current()?;
    let started = Instant::now();
    let duration = Duration::from_secs(u64::from(command.seconds));
    let keyframe_every = Duration::from_secs(u64::from(command.keyframe_every_s));
    let mut next_keyframe = keyframe_every;
    let (mut bytes, mut units, mut keyframes) = (0_u64, 0_u64, 0_u64);
    let mut write = |chunk: &encode::EncodedChunk| -> std::io::Result<()> {
        if let Some(file) = file.as_mut() {
            file.write_all(&chunk.data)?;
        }
        bytes += chunk.data.len() as u64;
        units += 1;
        keyframes += u64::from(chunk.keyframe);
        Ok(())
    };
    while started.elapsed() < duration {
        if command.keyframe_every_s > 0 && started.elapsed() >= next_keyframe {
            pipeline.request_keyframe();
            next_keyframe += keyframe_every;
        }
        match pipeline.recv(Duration::from_millis(50)) {
            Received::Chunk(chunk) => write(&chunk)?,
            Received::Timeout => {}
            Received::Finished => break,
        }
    }
    let encoder = pipeline.selection().cloned();
    let (report, remaining) = pipeline.stop();
    for chunk in &remaining {
        write(chunk)?;
    }
    drop(write);
    if let Some(file) = file.as_mut() {
        file.flush()?;
    }
    drop(test_window);
    let elapsed = started.elapsed().as_secs_f64();
    let resources_end = ProcessStats::current()?;

    let frequency = qpc_frequency();
    let ticks_per_ms = QpcDuration::from_micros(1_000.0, frequency).ticks() as f64;
    let summary = PipelineSummary {
        schema_version: 1,
        until: command.until,
        source,
        capture: selection,
        encoder,
        output: [output_width, output_height],
        fps: command.fps,
        target_bitrate_bps: command.bitrate_bps,
        gop: command.gop,
        wgc_interval_factor: command.wgc_interval_factor,
        low_priority: command.low_priority,
        build_profile,
        seconds: elapsed,
        taken_fps: report.counters.frames_taken as f64 / elapsed,
        access_units_per_s: units as f64 / elapsed,
        achieved_bitrate_bps: if units > 0 {
            bytes as f64 * 8.0 * f64::from(command.fps) / units as f64
        } else {
            0.0
        },
        keyframes,
        process_cpu_percent: cpu_percent_of_one_core(&resources_start, &resources_end, elapsed),
        counters: report.capture.counters,
        pipeline: report.counters,
        capture_cost_us: Summary::from_durations(&report.capture.cost, frequency),
        convert_gpu_us: Summary::from_durations(report.stages.samples(Stage::Convert), frequency),
        submit_cpu_us: Summary::from_durations(report.stages.samples(Stage::Submit), frequency),
        encode_latency_us: Summary::from_durations(
            report.stages.samples(Stage::EncodeOutput),
            frequency,
        ),
        capture_to_output_ms: signed_stats(&report.capture_to_output, ticks_per_ms),
        timer_skipped: report.timer_skipped,
        capture_end: report.capture_end.map(|end| format!("{end:?}")),
        error: report.error,
        file: path.as_ref().map(|path| path.display().to_string()),
    };

    let counters = &summary.pipeline;
    println!(
        "  frames: {} taken ({:.1}/s), {} converted, {} not converted (pool), {} held frames replaced, {} submitted, {} busy",
        counters.frames_taken,
        summary.taken_fps,
        counters.converted,
        counters.convert_dropped,
        counters.held_replaced,
        counters.submitted,
        counters.busy
    );
    println!(
        "  output: {} access units, {} delivered, {} dropped (queue full), {} dropped until keyframe; {} keyframes; {:.2} Mbit/s",
        counters.access_units,
        counters.delivered,
        counters.output_full_dropped,
        counters.dropped_until_keyframe,
        summary.keyframes,
        summary.achieved_bitrate_bps / 1e6
    );
    let print = |name: &str, stats: &Option<Summary>| match stats {
        Some(s) => println!(
            "  {name:<16} (us): mean {:.1}, p50 {:.1}, p95 {:.1}, p99 {:.1}, max {:.1}",
            s.mean_us, s.p50_us, s.p95_us, s.p99_us, s.max_us
        ),
        None => println!("  {name:<16}: no samples"),
    };
    print("capture cost", &summary.capture_cost_us);
    print("convert GPU", &summary.convert_gpu_us);
    print("submit CPU", &summary.submit_cpu_us);
    print("encode latency", &summary.encode_latency_us);
    if let Some(s) = &summary.capture_to_output_ms {
        println!(
            "  capture->output (ms, signed): mean {:.2}, p50 {:.2}, p95 {:.2}, p99 {:.2}",
            s.mean, s.p50, s.p95, s.p99
        );
    }
    println!(
        "  process CPU: {:.1}% of one core",
        summary.process_cpu_percent
    );
    if let Some(error) = &summary.error {
        println!("  error: {error}");
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
    if let Some(path) = &path {
        println!("  file: {}", path.display());
    }
    if let Some(json) = &command.summary_json {
        std::fs::write(json, serde_json::to_string_pretty(&summary)?)?;
        println!("  summary: {}", json.display());
    }
    match summary.error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}
