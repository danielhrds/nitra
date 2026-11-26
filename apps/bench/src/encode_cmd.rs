//! `bench encode`: frames -> NV12 -> hardware H.264 encoder. Reports encoder
//! latency (submit to output), CPU cost, bitrate, keyframes and
//! requested-keyframe delay, and writes the Annex B stream to a `.h264` file
//! for validation (ffprobe, frame stamps, quality metrics).
//!
//! Sources:
//! - `synthetic`: paced synthetic frames (with visual stamps), converted to
//!   NV12 by the shader converter. Busy frames are dropped.
//! - `raw`: a raw NV12 file (e.g. recorded game footage), paced, to its end; every frame
//!   is encoded exactly once (a busy encoder is waited for), so the output
//!   lines up frame by frame with the file for quality metrics.
//! - `monitor`: WGC capture of a monitor, converted to NV12, one frame at a
//!   time on this thread (a minimal stand-in for the host pipeline, used to
//!   measure the impact on a running game). Busy frames are dropped.

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use capture::synthetic::{Pacer, PatternMode, SyntheticConfig, SyntheticSource};
use capture::wgc::CaptureTarget;
use capture::{CaptureOptions, CounterSnapshot, Taken};
use encode::amf::AmfEncoder;
use encode::annexb::{self, SliceType};
use encode::media_foundation::MediaFoundationEncoder;
use encode::{EncodedChunk, EncoderConfig, Submit, VideoEncoder};
use gpu::convert::{ConvertConfig, Converter, ShaderConverter};
use gpu::{DebugLayer, FrameFormat, GpuFrame};
use serde::Serialize;
use telemetry::{QpcDuration, QpcFrequency, QpcTimestamp, qpc_frequency, qpc_now};

use crate::com::ComApartment;
use crate::process_stats::{ProcessStats, cpu_percent_of_one_core};
use crate::raw_source::RawSource;

#[derive(Clone, Copy, Debug)]
pub enum Backend {
    MediaFoundation,
    Amf,
}

#[derive(Clone, Debug)]
pub enum Source {
    Synthetic,
    /// Raw NV12 frames of the command's input size.
    Raw(PathBuf),
    /// Monitor index in `bench probe` order.
    Monitor(usize),
}

pub struct EncodeCommand {
    pub backend: Backend,
    pub source: Source,
    /// Input size (synthetic and raw sources; a monitor has its own).
    pub width: u32,
    pub height: u32,
    /// Encoded size [default: input size].
    pub output_width: Option<u32>,
    pub output_height: Option<u32>,
    pub fps: u32,
    /// Run time (synthetic and monitor sources; a raw source runs to the
    /// end of its file).
    pub seconds: u32,
    pub bitrate_bps: u32,
    pub gop: u32,
    /// Request a keyframe every N frames (0 = never) to measure the delay.
    pub keyframe_every: u32,
    pub output: Option<PathBuf>,
    pub summary_json: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Stats {
    samples: usize,
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn stats(values: &[f64]) -> Option<Stats> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable_by(f64::total_cmp);
    let rank = |percentile: usize| sorted[(percentile * sorted.len()).div_ceil(100).max(1) - 1];
    Some(Stats {
        samples: sorted.len(),
        mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
        p50: rank(50),
        p95: rank(95),
        p99: rank(99),
        max: sorted[sorted.len() - 1],
    })
}

#[derive(Debug, Serialize)]
struct EncodeSummary {
    schema_version: u32,
    encoder: &'static str,
    /// Which implementation: the MFT name and candidates, or the AMF runtime.
    encoder_detail: String,
    applied_settings: String,
    source: String,
    input: [u32; 2],
    output: [u32; 2],
    fps: u32,
    /// Wall time of the run.
    seconds: f64,
    target_bitrate_bps: u32,
    gop: u32,
    build_profile: &'static str,
    /// Frames the source produced (rendered, read or captured).
    frames_in: u64,
    /// Source frames per second of run time.
    taken_fps: f64,
    /// Frames not converted because every NV12 texture was in use.
    frames_convert_dropped: u64,
    frames_submitted: u64,
    frames_busy_dropped: u64,
    /// Raw source: submissions retried because the encoder was busy.
    busy_retries: u64,
    access_units: u64,
    bytes: u64,
    /// Over the encoded frames' duration (access units / fps).
    achieved_bitrate_bps: f64,
    keyframes: Vec<u64>,
    keyframe_mean_bytes: Option<f64>,
    other_mean_bytes: Option<f64>,
    b_slices: u64,
    /// Submit to output, per access unit.
    latency_ms: Option<Stats>,
    submit_cpu_us: Option<Stats>,
    /// Frames between a keyframe request and the next keyframe.
    requested_keyframe_delay_frames: Vec<u64>,
    /// Whole process, % of one core.
    process_cpu_percent: f64,
    /// Monitor source: the capture's counters.
    counters: Option<CounterSnapshot>,
    file: Option<String>,
}

/// What `Run::encode` does when the encoder is busy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WhenBusy {
    Drop,
    Wait,
}

/// The encoder plus everything recorded about its output.
struct Run {
    encoder: Box<dyn VideoEncoder>,
    file: BufWriter<File>,
    summary: EncodeSummary,
    frequency: QpcFrequency,
    latencies: Vec<f64>,
    submit_cpu: Vec<f64>,
    keyframe_bytes: Vec<f64>,
    other_bytes: Vec<f64>,
    pending_requests: Vec<u64>,
    keyframe_every: u32,
}

impl Run {
    /// Submits one NV12 frame (requesting a keyframe if due), then follows
    /// the output closely, as a dedicated encoder thread would, so the
    /// measured latency is the encoder's, not this loop's: polls every
    /// ~0.2 ms until this frame's access unit is out or `give_up`.
    fn encode(
        &mut self,
        mut frame: GpuFrame,
        when_busy: WhenBusy,
        give_up: QpcTimestamp,
    ) -> Result<(), Box<dyn Error>> {
        let frame_id = frame.frame_id;
        if self.keyframe_every > 0
            && frame_id > 0
            && frame_id.is_multiple_of(u64::from(self.keyframe_every))
        {
            self.encoder.request_keyframe();
            self.pending_requests.push(frame_id);
        }
        let accepted = loop {
            let started = qpc_now();
            let result = self.encoder.submit(frame)?;
            self.submit_cpu
                .push(qpc_now().duration_since(started).as_micros(self.frequency));
            match (result, when_busy) {
                (Submit::Accepted, _) => {
                    self.summary.frames_submitted += 1;
                    break true;
                }
                // Real pipelines drop the frame rather than wait.
                (Submit::Busy(_), WhenBusy::Drop) => {
                    self.summary.frames_busy_dropped += 1;
                    break false;
                }
                (Submit::Busy(returned), WhenBusy::Wait) => {
                    self.summary.busy_retries += 1;
                    frame = returned;
                    self.take_ready()?;
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
        };
        let mut done = !accepted;
        loop {
            while let Some(chunk) = self.encoder.poll()? {
                done |= chunk.frame_id == frame_id;
                self.record(chunk)?;
            }
            if done || qpc_now() >= give_up {
                return Ok(());
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    fn take_ready(&mut self) -> Result<(), Box<dyn Error>> {
        while let Some(chunk) = self.encoder.poll()? {
            self.record(chunk)?;
        }
        Ok(())
    }

    fn record(&mut self, chunk: EncodedChunk) -> std::io::Result<()> {
        self.file.write_all(&chunk.data)?;
        let summary = &mut self.summary;
        summary.access_units += 1;
        summary.bytes += chunk.data.len() as u64;
        self.latencies.push(
            chunk
                .output_at
                .duration_since(chunk.submitted_at)
                .as_micros(self.frequency)
                / 1_000.0,
        );
        summary.b_slices += annexb::nal_units(&chunk.data)
            .iter()
            .filter(|nal| annexb::slice_type(nal) == Some(SliceType::B))
            .count() as u64;
        if chunk.keyframe {
            summary.keyframes.push(chunk.frame_id);
            self.keyframe_bytes.push(chunk.data.len() as f64);
            self.pending_requests.retain(|&requested| {
                if chunk.frame_id >= requested {
                    summary
                        .requested_keyframe_delay_frames
                        .push(chunk.frame_id - requested);
                    false
                } else {
                    true
                }
            });
        } else {
            self.other_bytes.push(chunk.data.len() as f64);
        }
        Ok(())
    }
}

pub fn run(command: EncodeCommand) -> Result<(), Box<dyn Error>> {
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

    // The monitor fixes the input size; the other sources take it as given.
    let monitor = match command.source {
        Source::Monitor(index) => {
            let outputs: Vec<_> = adapters
                .iter()
                .filter(|adapter| !adapter.info.is_software)
                .flat_map(|adapter| adapter.outputs.iter())
                .collect();
            Some(
                *outputs
                    .get(index)
                    .ok_or_else(|| format!("no monitor {index}; {} found", outputs.len()))?,
            )
        }
        _ => None,
    };
    let (width, height) = match monitor {
        Some(output) => {
            let desktop = output.info.desktop;
            (
                u32::try_from(desktop.right - desktop.left)?,
                u32::try_from(desktop.bottom - desktop.top)?,
            )
        }
        None => (command.width, command.height),
    };
    let output_width = command.output_width.unwrap_or(width);
    let output_height = command.output_height.unwrap_or(height);
    if matches!(command.source, Source::Raw(_)) && (output_width, output_height) != (width, height)
    {
        return Err("a raw source is encoded at its own size (NV12 input is not scaled)".into());
    }

    let config = EncoderConfig {
        width: output_width,
        height: output_height,
        fps: command.fps,
        bitrate_bps: command.bitrate_bps,
        keyframe_interval: command.gop,
        max_in_flight: 4,
        max_reference_frames: 0,
    };
    let (encoder, encoder_detail, applied_settings): (Box<dyn VideoEncoder>, String, String) =
        match command.backend {
            Backend::MediaFoundation => {
                let encoder = MediaFoundationEncoder::new(&device, adapter.info.luid, config)?;
                let detail = format!(
                    "MFT {}, {} candidate(s)",
                    encoder.info().name.as_deref().unwrap_or("?"),
                    encoder.candidates()
                );
                let applied = format!("{:?}", encoder.applied_settings());
                (Box::new(encoder), detail, applied)
            }
            Backend::Amf => {
                let encoder = AmfEncoder::new(&device, config)?;
                let detail = format!("AMF runtime {}", encoder.runtime_version());
                let applied = format!("{:?}", encoder.applied_settings());
                (Box::new(encoder), detail, applied)
            }
        };

    let source_name = match &command.source {
        Source::Synthetic => "synthetic".to_owned(),
        Source::Raw(path) => format!("raw {}", path.display()),
        Source::Monitor(index) => format!(
            "monitor {index} ({})",
            monitor.map_or("?", |output| output.info.device_name.as_str())
        ),
    };
    let path = command.output.clone().unwrap_or_else(|| {
        telemetry::default_output_dir().join("encode").join(format!(
            "{}-{}-{}x{}-{}kbps.h264",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis()),
            encoder.name(),
            output_width,
            output_height,
            command.bitrate_bps / 1000
        ))
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let build_profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "encode: {} ({encoder_detail}) from {source_name}: {width}x{height} -> {output_width}x{output_height} @ {} fps, {} kbps CBR, GOP {} for {} s on {} ({build_profile})",
        encoder.name(),
        command.fps,
        command.bitrate_bps / 1000,
        command.gop,
        command.seconds,
        adapter.info.name,
    );
    println!("  applied settings: {applied_settings}");

    let mut run = Run {
        summary: EncodeSummary {
            schema_version: 3,
            encoder: encoder.name(),
            encoder_detail,
            applied_settings,
            source: source_name,
            input: [width, height],
            output: [output_width, output_height],
            fps: command.fps,
            seconds: 0.0,
            target_bitrate_bps: command.bitrate_bps,
            gop: command.gop,
            build_profile,
            frames_in: 0,
            taken_fps: 0.0,
            frames_convert_dropped: 0,
            frames_submitted: 0,
            frames_busy_dropped: 0,
            busy_retries: 0,
            access_units: 0,
            bytes: 0,
            achieved_bitrate_bps: 0.0,
            keyframes: Vec::new(),
            keyframe_mean_bytes: None,
            other_mean_bytes: None,
            b_slices: 0,
            latency_ms: None,
            submit_cpu_us: None,
            requested_keyframe_delay_frames: Vec::new(),
            process_cpu_percent: 0.0,
            counters: None,
            file: Some(path.display().to_string()),
        },
        encoder,
        file: BufWriter::new(File::create(&path)?),
        frequency: qpc_frequency(),
        latencies: Vec::new(),
        submit_cpu: Vec::new(),
        keyframe_bytes: Vec::new(),
        other_bytes: Vec::new(),
        pending_requests: Vec::new(),
        keyframe_every: command.keyframe_every,
    };

    let converter = |input_width, input_height| {
        ShaderConverter::new(
            &device,
            ConvertConfig {
                input_width,
                input_height,
                output_width,
                output_height,
                pool_size: 6,
            },
        )
    };
    let nv12 = |texture, frame_id, captured_at| GpuFrame {
        texture,
        format: FrameFormat::Nv12,
        width: output_width,
        height: output_height,
        frame_id,
        captured_at,
    };

    let resources_start = ProcessStats::current()?;
    let started = Instant::now();
    let slots = u64::from(command.fps) * u64::from(command.seconds);
    match &command.source {
        Source::Synthetic => {
            let mut source = SyntheticSource::new(
                &device,
                SyntheticConfig {
                    width,
                    height,
                    mode: PatternMode::Motion,
                },
            )?;
            let mut converter = converter(width, height)?;
            let mut pacer = Pacer::start_now(command.fps);
            loop {
                let slot = pacer.next_slot(qpc_now());
                if slot.index >= slots {
                    break;
                }
                pacer.wait(&slot);
                let synthetic = source.render_next()?;
                run.summary.frames_in += 1;
                let Some(texture) = converter.convert(&synthetic.texture)? else {
                    run.summary.frames_convert_dropped += 1;
                    continue;
                };
                let frame = nv12(
                    texture,
                    u64::from(synthetic.stamp.frame_id),
                    synthetic.stamp.timestamp,
                );
                run.encode(frame, WhenBusy::Drop, pacer.deadline(slot.index + 1))?;
            }
        }
        Source::Raw(file) => {
            let mut source = RawSource::open(&device, file, width, height)?;
            println!("  raw file: {} frames", source.frames());
            // Paced, but never cut short: every frame of the file is encoded
            // so the output lines up with it (`--seconds` does not apply).
            let mut pacer = Pacer::start_now(command.fps);
            loop {
                let slot = pacer.next_slot(qpc_now());
                pacer.wait(&slot);
                let Some(texture) = source.next()? else {
                    break;
                };
                let frame = nv12(texture, run.summary.frames_in, qpc_now());
                run.summary.frames_in += 1;
                run.encode(frame, WhenBusy::Wait, pacer.deadline(slot.index + 1))?;
            }
        }
        Source::Monitor(_) => {
            let output = monitor.expect("monitor resolved above");
            let mut converter = converter(width, height)?;
            let capture = capture::wgc::start(
                &device,
                CaptureTarget::Monitor(output.monitor()),
                CaptureOptions {
                    fps: command.fps,
                    cursor: false,
                    hide_border: true,
                    ..CaptureOptions::default()
                },
            )?;
            let duration = Duration::from_secs(u64::from(command.seconds));
            let frame_interval =
                QpcDuration::from_micros(1_000_000.0 / f64::from(command.fps), qpc_frequency())
                    .ticks() as i64;
            let end = loop {
                if started.elapsed() >= duration {
                    break None;
                }
                match capture.next(Duration::from_millis(200)) {
                    Taken::Value(captured) => {
                        run.summary.frames_in += 1;
                        let converted = converter.convert(&captured.texture)?;
                        let captured_at = captured.captured_at;
                        // The copy is queued: the capture texture can go back.
                        drop(captured);
                        let Some(texture) = converted else {
                            run.summary.frames_convert_dropped += 1;
                            continue;
                        };
                        // Our own sequence (capture ids may skip), so
                        // keyframe requests and output matching line up.
                        let frame = nv12(texture, run.summary.frames_in - 1, captured_at);
                        let give_up = QpcTimestamp::from_ticks(qpc_now().ticks() + frame_interval);
                        run.encode(frame, WhenBusy::Drop, give_up)?;
                    }
                    Taken::Ended(end) => break Some(end),
                    Taken::Timeout => {}
                }
            };
            let report = capture.stop();
            run.summary.counters = Some(report.counters);
            if let Some(end) = end {
                println!("  capture ended early: {end:?}");
            }
        }
    }
    let chunks = run.encoder.drain()?;
    for chunk in chunks {
        run.record(chunk)?;
    }
    run.file.flush()?;
    let elapsed = started.elapsed().as_secs_f64();
    let resources_end = ProcessStats::current()?;

    let mean = |values: &[f64]| {
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    let summary = &mut run.summary;
    summary.seconds = elapsed;
    summary.taken_fps = summary.frames_in as f64 / elapsed;
    summary.achieved_bitrate_bps = if summary.access_units > 0 {
        summary.bytes as f64 * 8.0 * f64::from(command.fps) / summary.access_units as f64
    } else {
        0.0
    };
    summary.keyframe_mean_bytes = mean(&run.keyframe_bytes);
    summary.other_mean_bytes = mean(&run.other_bytes);
    summary.latency_ms = stats(&run.latencies);
    summary.submit_cpu_us = stats(&run.submit_cpu);
    summary.process_cpu_percent =
        cpu_percent_of_one_core(&resources_start, &resources_end, elapsed);

    println!(
        "  frames: {} in ({:.1}/s), {} not converted (pool), {} submitted, {} dropped (encoder busy), {} busy retries, {} access units out",
        summary.frames_in,
        summary.taken_fps,
        summary.frames_convert_dropped,
        summary.frames_submitted,
        summary.frames_busy_dropped,
        summary.busy_retries,
        summary.access_units
    );
    println!(
        "  bitrate: {:.2} Mbit/s achieved (target {:.2}); keyframes {} (mean {:.0} KiB) vs other frames mean {:.1} KiB; B slices {}",
        summary.achieved_bitrate_bps / 1e6,
        f64::from(command.bitrate_bps) / 1e6,
        summary.keyframes.len(),
        summary.keyframe_mean_bytes.unwrap_or(0.0) / 1024.0,
        summary.other_mean_bytes.unwrap_or(0.0) / 1024.0,
        summary.b_slices
    );
    let print = |name: &str, unit: &str, stats: Option<Stats>| match stats {
        Some(s) => println!(
            "  {name:<14} ({unit}): mean {:.3}, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}",
            s.mean, s.p50, s.p95, s.p99, s.max
        ),
        None => println!("  {name:<14}: no samples"),
    };
    print("latency", "ms", summary.latency_ms);
    print("submit CPU", "us", summary.submit_cpu_us);
    println!(
        "  process CPU: {:.1}% of one core",
        summary.process_cpu_percent
    );
    if command.keyframe_every > 0 {
        println!(
            "  requested keyframe delay (frames): {:?}",
            summary.requested_keyframe_delay_frames
        );
    }
    println!("  keyframes at: {:?}", summary.keyframes);
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
    println!("  file: {}", path.display());
    if let Some(json) = &command.summary_json {
        std::fs::write(json, serde_json::to_string_pretty(&run.summary)?)?;
        println!("  summary: {}", json.display());
    }
    Ok(())
}
