//! `bench encode`: paced synthetic frames -> NV12 (shader) -> hardware H.264
//! encoder. Reports encoder latency (submit to output), CPU cost, bitrate,
//! keyframes and requested-keyframe delay, and writes the Annex B stream to
//! a `.h264` file for validation (ffprobe, frame stamps).

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use capture::synthetic::{Pacer, PatternMode, SyntheticConfig, SyntheticSource};
use encode::annexb::{self, SliceType};
use encode::media_foundation::MediaFoundationEncoder;
use encode::{EncodedChunk, EncoderConfig, Submit, VideoEncoder};
use gpu::convert::{ConvertConfig, Converter, ShaderConverter};
use gpu::{DebugLayer, FrameFormat, GpuFrame};
use serde::Serialize;
use telemetry::{qpc_frequency, qpc_now};

use crate::com::ComApartment;

pub struct EncodeCommand {
    pub width: u32,
    pub height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub fps: u32,
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
    mft: Option<String>,
    mft_candidates: usize,
    applied_settings: String,
    input: [u32; 2],
    output: [u32; 2],
    fps: u32,
    seconds: u32,
    target_bitrate_bps: u32,
    gop: u32,
    build_profile: &'static str,
    frames_rendered: u64,
    frames_submitted: u64,
    frames_busy_dropped: u64,
    access_units: u64,
    bytes: u64,
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
    file: Option<String>,
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

    let mut source = SyntheticSource::new(
        &device,
        SyntheticConfig {
            width: command.width,
            height: command.height,
            mode: PatternMode::Motion,
        },
    )?;
    let mut converter = ShaderConverter::new(
        &device,
        ConvertConfig {
            input_width: command.width,
            input_height: command.height,
            output_width: command.output_width,
            output_height: command.output_height,
            pool_size: 6,
        },
    )?;
    let mut encoder = MediaFoundationEncoder::new(
        &device,
        adapter.info.luid,
        EncoderConfig {
            width: command.output_width,
            height: command.output_height,
            fps: command.fps,
            bitrate_bps: command.bitrate_bps,
            keyframe_interval: command.gop,
            max_in_flight: 4,
        },
    )?;

    let path = command.output.clone().unwrap_or_else(|| {
        telemetry::default_output_dir().join("encode").join(format!(
            "{}-mf-{}x{}-{}kbps.h264",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis()),
            command.output_width,
            command.output_height,
            command.bitrate_bps / 1000
        ))
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = BufWriter::new(File::create(&path)?);

    println!(
        "encode: {} ({:?}, {} candidate(s)) {}x{} -> {}x{} @ {} fps, {} kbps CBR, GOP {} for {} s on {} ({})",
        encoder.name(),
        encoder.info().name.as_deref().unwrap_or("?"),
        encoder.candidates(),
        command.width,
        command.height,
        command.output_width,
        command.output_height,
        command.fps,
        command.bitrate_bps / 1000,
        command.gop,
        command.seconds,
        adapter.info.name,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );
    println!("  applied settings: {:?}", encoder.applied_settings());

    let frequency = qpc_frequency();
    let total_slots = u64::from(command.fps) * u64::from(command.seconds);
    let mut summary = EncodeSummary {
        schema_version: 1,
        encoder: encoder.name(),
        mft: encoder.info().name.clone(),
        mft_candidates: encoder.candidates(),
        applied_settings: format!("{:?}", encoder.applied_settings()),
        input: [command.width, command.height],
        output: [command.output_width, command.output_height],
        fps: command.fps,
        seconds: command.seconds,
        target_bitrate_bps: command.bitrate_bps,
        gop: command.gop,
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        frames_rendered: 0,
        frames_submitted: 0,
        frames_busy_dropped: 0,
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
        file: Some(path.display().to_string()),
    };
    let mut latencies = Vec::new();
    let mut submit_cpu = Vec::new();
    let (mut keyframe_bytes, mut other_bytes) = (Vec::new(), Vec::new());
    let mut pending_requests: Vec<u64> = Vec::new();

    let mut handle_chunk = |chunk: EncodedChunk,
                            file: &mut BufWriter<File>,
                            summary: &mut EncodeSummary,
                            pending_requests: &mut Vec<u64>|
     -> std::io::Result<()> {
        file.write_all(&chunk.data)?;
        summary.access_units += 1;
        summary.bytes += chunk.data.len() as u64;
        latencies.push(
            chunk
                .output_at
                .duration_since(chunk.submitted_at)
                .as_micros(frequency)
                / 1_000.0,
        );
        summary.b_slices += annexb::nal_units(&chunk.data)
            .iter()
            .filter(|nal| annexb::slice_type(nal) == Some(SliceType::B))
            .count() as u64;
        if chunk.keyframe {
            summary.keyframes.push(chunk.frame_id);
            keyframe_bytes.push(chunk.data.len() as f64);
            pending_requests.retain(|&requested| {
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
            other_bytes.push(chunk.data.len() as f64);
        }
        Ok(())
    };

    let mut pacer = Pacer::start_now(command.fps);
    loop {
        let slot = pacer.next_slot(qpc_now());
        if slot.index >= total_slots {
            break;
        }
        pacer.wait(&slot);
        let synthetic = source.render_next()?;
        summary.frames_rendered += 1;
        let Some(texture) = converter.convert(&synthetic.texture)? else {
            continue;
        };
        let frame_id = u64::from(synthetic.stamp.frame_id);
        if command.keyframe_every > 0
            && frame_id > 0
            && frame_id.is_multiple_of(u64::from(command.keyframe_every))
        {
            encoder.request_keyframe();
            pending_requests.push(frame_id);
        }
        let frame = GpuFrame {
            texture,
            format: FrameFormat::Nv12,
            width: command.output_width,
            height: command.output_height,
            frame_id,
            captured_at: synthetic.stamp.timestamp,
        };
        let started = qpc_now();
        let accepted = match encoder.submit(frame)? {
            Submit::Accepted => {
                summary.frames_submitted += 1;
                true
            }
            // Real pipelines drop the frame rather than wait; so does this.
            Submit::Busy(_) => {
                summary.frames_busy_dropped += 1;
                false
            }
        };
        submit_cpu.push(qpc_now().duration_since(started).as_micros(frequency));

        // Follow the output closely (as a dedicated encoder thread would),
        // so the measured latency is the encoder's, not this loop's: poll
        // every ~0.2 ms until this frame's access unit is out, giving up a
        // little before the next frame is due.
        let give_up = pacer.deadline(slot.index + 1);
        let mut done = !accepted;
        loop {
            while let Some(chunk) = encoder.poll()? {
                done |= chunk.frame_id == frame_id;
                handle_chunk(chunk, &mut file, &mut summary, &mut pending_requests)?;
            }
            if done || qpc_now() >= give_up {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }
    for chunk in encoder.drain()? {
        handle_chunk(chunk, &mut file, &mut summary, &mut pending_requests)?;
    }
    file.flush()?;

    let mean = |values: &[f64]| {
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    summary.achieved_bitrate_bps = summary.bytes as f64 * 8.0 / f64::from(command.seconds);
    summary.keyframe_mean_bytes = mean(&keyframe_bytes);
    summary.other_mean_bytes = mean(&other_bytes);
    summary.latency_ms = stats(&latencies);
    summary.submit_cpu_us = stats(&submit_cpu);

    println!(
        "  frames: {} rendered, {} submitted, {} dropped (encoder busy), {} access units out",
        summary.frames_rendered,
        summary.frames_submitted,
        summary.frames_busy_dropped,
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
        std::fs::write(json, serde_json::to_string_pretty(&summary)?)?;
        println!("  summary: {}", json.display());
    }
    Ok(())
}
