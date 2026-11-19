//! `bench decode <file.h264>`: splits an Annex B file into access units,
//! decodes them with the hardware decoder (NV12 textures on the GPU),
//! optionally paced, and reports frames, sizes, submit-to-output time and,
//! with `--stamps`, the visual frame stamp of every frame, read from the
//! stamp region only (no whole-frame readback).

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use capture::synthetic::Pacer;
use decode::media_foundation::MediaFoundationDecoder;
use decode::{DecodeSubmit, DecodedFrame, VideoDecoder};
use gpu::DebugLayer;
use gpu::convert::read_nv12_luma_region;
use serde::Serialize;
use telemetry::frame_code::{self, read_frame_stamp};
use telemetry::{QpcDuration, QpcTimestamp, Summary, qpc_frequency, qpc_now};

use crate::com::ComApartment;
use crate::process_stats::{ProcessStats, cpu_percent_of_one_core};
use crate::stamps_cmd::{StampCheck, check_sequence};

pub struct DecodeCommand {
    pub file: PathBuf,
    /// Submit at this rate; 0 = as fast as the decoder accepts.
    pub fps: u32,
    pub stamps: bool,
    pub summary_json: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct DecodeSummary {
    schema_version: u32,
    decoder: &'static str,
    file: String,
    fps: u32,
    access_units: usize,
    frames: usize,
    sizes: Vec<[u32; 2]>,
    busy_retries: u64,
    seconds: f64,
    /// Submit to frame out of the decoder (CPU hand-off; the GPU finishes
    /// the decode afterwards, in order).
    output_latency_us: Option<Summary>,
    process_cpu_percent: f64,
    stamps: Option<StampReport>,
}

#[derive(Debug, Serialize)]
struct StampReport {
    frames: u64,
    unreadable: u64,
    missing: u64,
    out_of_order: u64,
    first_id: Option<u32>,
    last_id: Option<u32>,
}

impl From<StampCheck> for StampReport {
    fn from(check: StampCheck) -> Self {
        Self {
            frames: check.frames,
            unreadable: check.unreadable,
            missing: check.missing,
            out_of_order: check.out_of_order,
            first_id: check.first_id,
            last_id: check.last_id,
        }
    }
}

struct Collector<'a> {
    device: &'a gpu::D3d11Device,
    stamps: bool,
    latencies: Vec<QpcDuration>,
    sizes: Vec<[u32; 2]>,
    stamp_ids: Vec<Option<u32>>,
}

impl Collector<'_> {
    fn take(&mut self, frame: DecodedFrame) -> Result<(), Box<dyn Error>> {
        self.latencies
            .push(frame.decoded_at.duration_since(frame.submitted_at));
        let size = [frame.width, frame.height];
        if !self.sizes.contains(&size) {
            self.sizes.push(size);
        }
        if self.stamps {
            let width = frame_code::CODE_WIDTH;
            let luma = read_nv12_luma_region(
                self.device,
                &frame.texture,
                frame.slice,
                width,
                frame_code::CODE_HEIGHT,
            )?;
            let stamp = read_frame_stamp(|x, y| luma[(y * width + x) as usize]);
            self.stamp_ids.push(stamp.ok().map(|stamp| stamp.frame_id));
        }
        Ok(())
        // `frame` drops here: the decoder's surface is released at once.
    }
}

pub fn run(command: DecodeCommand) -> Result<(), Box<dyn Error>> {
    let _com = ComApartment::multithreaded()?;
    let _media_foundation = mf::MediaFoundation::start()?;
    let stream = std::fs::read(&command.file)?;
    let units = encode::annexb::access_units(&stream);
    if units.is_empty() {
        return Err(format!("no access units in {}", command.file.display()).into());
    }
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
    let mut decoder = MediaFoundationDecoder::new(&device)?;
    println!(
        "decode: {} ({} access units, {:.1} MiB) with {} on {}, {}{}",
        command.file.display(),
        units.len(),
        stream.len() as f64 / (1024.0 * 1024.0),
        decoder.name(),
        adapter.info.name,
        if command.fps > 0 {
            format!("paced at {} fps", command.fps)
        } else {
            "unpaced".to_owned()
        },
        if command.stamps {
            ", reading stamps"
        } else {
            ""
        }
    );

    let mut collector = Collector {
        device: &device,
        stamps: command.stamps,
        latencies: Vec::with_capacity(units.len()),
        sizes: Vec::new(),
        stamp_ids: Vec::with_capacity(units.len()),
    };
    let mut busy_retries = 0_u64;
    let resources_start = ProcessStats::current()?;
    let started = std::time::Instant::now();
    let mut pacer = (command.fps > 0).then(|| Pacer::start_now(command.fps));
    let mut frames = 0_usize;
    for (index, unit) in units.iter().enumerate() {
        if let Some(pacer) = pacer.as_mut() {
            let slot = pacer.next_slot(qpc_now());
            pacer.wait(&slot);
        }
        loop {
            match decoder.submit(unit, index as u64, QpcTimestamp::from_ticks(0))? {
                DecodeSubmit::Accepted => break,
                DecodeSubmit::Busy => {
                    busy_retries += 1;
                    while let Some(frame) = decoder.poll()? {
                        frames += 1;
                        collector.take(frame)?;
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
        }
        while let Some(frame) = decoder.poll()? {
            frames += 1;
            collector.take(frame)?;
        }
    }
    for frame in decoder.drain()? {
        frames += 1;
        collector.take(frame)?;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let resources_end = ProcessStats::current()?;

    let frequency = qpc_frequency();
    let summary = DecodeSummary {
        schema_version: 1,
        decoder: decoder.name(),
        file: command.file.display().to_string(),
        fps: command.fps,
        access_units: units.len(),
        frames,
        sizes: collector.sizes.clone(),
        busy_retries,
        seconds: elapsed,
        output_latency_us: Summary::from_durations(&collector.latencies, frequency),
        process_cpu_percent: cpu_percent_of_one_core(&resources_start, &resources_end, elapsed),
        stamps: command
            .stamps
            .then(|| check_sequence(collector.stamp_ids.iter().copied()).into()),
    };
    println!(
        "  frames: {} of {} access units in {:.2} s ({:.1}/s), sizes {:?}, {} busy retries",
        summary.frames,
        summary.access_units,
        elapsed,
        summary.frames as f64 / elapsed,
        summary.sizes,
        summary.busy_retries
    );
    if let Some(s) = &summary.output_latency_us {
        println!(
            "  submit->output (us): mean {:.1}, p50 {:.1}, p95 {:.1}, p99 {:.1}, max {:.1}",
            s.mean_us, s.p50_us, s.p95_us, s.p99_us, s.max_us
        );
    }
    println!(
        "  process CPU: {:.1}% of one core",
        summary.process_cpu_percent
    );
    if let Some(stamps) = &summary.stamps {
        println!(
            "  stamps: {} frames, {} unreadable, {} missing, {} out of order, ids {:?}..{:?}",
            stamps.frames,
            stamps.unreadable,
            stamps.missing,
            stamps.out_of_order,
            stamps.first_id,
            stamps.last_id
        );
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
    if let Some(json) = &command.summary_json {
        std::fs::write(json, serde_json::to_string_pretty(&summary)?)?;
        println!("  summary: {}", json.display());
    }
    let stamp_problems = summary
        .stamps
        .as_ref()
        .is_some_and(|stamps| stamps.unreadable + stamps.missing + stamps.out_of_order > 0);
    if summary.frames != summary.access_units || stamp_problems {
        return Err("decoded stream has problems".into());
    }
    Ok(())
}
