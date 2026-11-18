//! `bench convert`: converts paced synthetic frames from BGRA8 to NV12 with
//! one method and reports its GPU time (timestamp queries), CPU submit time
//! and, with `--validate`, whether the frame stamp survives in the Y plane.

use std::error::Error;
use std::path::PathBuf;

use capture::synthetic::{Pacer, PatternMode, SyntheticConfig, SyntheticSource};
use gpu::convert::{
    ConvertConfig, Converter, ShaderConverter, VideoProcessorConverter, read_nv12_luma,
};
use gpu::{DebugLayer, GpuTimer};
use serde::Serialize;
use telemetry::frame_code::{self, read_frame_stamp};
use telemetry::{qpc_frequency, qpc_now};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    VideoProcessor,
    Shader,
}

pub struct ConvertCommand {
    pub method: Method,
    pub width: u32,
    pub height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub fps: u32,
    pub seconds: u32,
    pub validate: bool,
    pub summary_json: Option<PathBuf>,
}

/// Mean, nearest-rank percentiles and max.
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
struct ConvertSummary {
    schema_version: u32,
    method: &'static str,
    input: [u32; 2],
    output: [u32; 2],
    fps: u32,
    seconds: u32,
    build_profile: &'static str,
    frames: u64,
    gpu_us: Option<Stats>,
    cpu_submit_us: Option<Stats>,
    /// Mean GPU time x fps: share of GPU time spent converting.
    gpu_share_percent: Option<f64>,
    pool_dropped: u64,
    timer_skipped: u64,
    validated: Option<u64>,
}

pub fn run(command: ConvertCommand) -> Result<(), Box<dyn Error>> {
    if command.validate
        && (command.output_width, command.output_height) != (command.width, command.height)
    {
        return Err(
            "--validate reads the frame stamp, which needs output size = input size".into(),
        );
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
    let context = device.immediate_context()?;

    let mut source = SyntheticSource::new(
        &device,
        SyntheticConfig {
            width: command.width,
            height: command.height,
            mode: PatternMode::Motion,
        },
    )?;
    let config = ConvertConfig {
        input_width: command.width,
        input_height: command.height,
        output_width: command.output_width,
        output_height: command.output_height,
        pool_size: 3,
    };
    let mut converter: Box<dyn Converter> = match command.method {
        Method::VideoProcessor => Box::new(VideoProcessorConverter::new(&device, config)?),
        Method::Shader => Box::new(ShaderConverter::new(&device, config)?),
    };
    let mut timer = GpuTimer::new(&device, 16)?;

    println!(
        "convert: {} {}x{} -> {}x{} NV12 @ {} fps for {} s on {} ({}, debug layer {})",
        converter.method(),
        command.width,
        command.height,
        command.output_width,
        command.output_height,
        command.fps,
        command.seconds,
        adapter.info.name,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        if device.debug_layer_enabled() {
            "on"
        } else {
            "off"
        }
    );
    if command.validate {
        println!(
            "  --validate reads every stamp back and stalls the GPU: timings are not representative"
        );
    }

    let frequency = qpc_frequency();
    let total_slots = u64::from(command.fps) * u64::from(command.seconds);
    let mut gpu_times = Vec::with_capacity(total_slots as usize);
    let mut cpu_times = Vec::with_capacity(total_slots as usize);
    let mut frames = 0_u64;
    let mut validated = 0_u64;
    let mut pacer = Pacer::start_now(command.fps);
    loop {
        let slot = pacer.next_slot(qpc_now());
        if slot.index >= total_slots {
            break;
        }
        pacer.wait(&slot);
        let frame = source.render_next()?;

        let started = qpc_now();
        let token = timer.begin(&context);
        let output = converter.convert(&frame.texture)?;
        if let Some(token) = token {
            timer.end(&context, token);
        }
        // SAFETY: valid context; submits the conversion right away.
        unsafe { context.Flush() };
        cpu_times.push(qpc_now().duration_since(started));
        gpu_times.extend(timer.collect(&context));
        frames += 1;

        if command.validate
            && let Some(output) = &output
        {
            let luma = read_nv12_luma(
                &device,
                output,
                frame_code::CODE_WIDTH,
                frame_code::CODE_HEIGHT,
            )?;
            let stamp = read_frame_stamp(|x, y| luma[(y * frame_code::CODE_WIDTH + x) as usize])?;
            if stamp != frame.stamp {
                return Err(format!(
                    "frame {}: stamp read {stamp:?}, expected {:?}",
                    frame.stamp.frame_id, frame.stamp
                )
                .into());
            }
            validated += 1;
        }
    }
    gpu_times.extend(timer.finish(&context));

    let cpu_us: Vec<f64> = cpu_times
        .iter()
        .map(|duration| duration.as_micros(frequency))
        .collect();
    let gpu_stats = stats(&gpu_times);
    let summary = ConvertSummary {
        schema_version: 1,
        method: converter.method(),
        input: [command.width, command.height],
        output: [command.output_width, command.output_height],
        fps: command.fps,
        seconds: command.seconds,
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        frames,
        gpu_us: gpu_stats,
        cpu_submit_us: stats(&cpu_us),
        gpu_share_percent: gpu_stats.map(|gpu| gpu.mean * f64::from(command.fps) / 10_000.0),
        pool_dropped: converter.pool_dropped(),
        timer_skipped: timer.skipped(),
        validated: command.validate.then_some(validated),
    };

    let print = |name: &str, stats: Option<Stats>| match stats {
        Some(s) => println!(
            "  {name:<11} (us): mean {:.1}, p50 {:.1}, p95 {:.1}, p99 {:.1}, max {:.1} ({} samples)",
            s.mean, s.p50, s.p95, s.p99, s.max, s.samples
        ),
        None => println!("  {name:<11}: no samples"),
    };
    println!(
        "  frames: {frames}, pool drops {}, timer skipped {}",
        summary.pool_dropped, summary.timer_skipped
    );
    print("GPU", summary.gpu_us);
    print("CPU submit", summary.cpu_submit_us);
    if let Some(share) = summary.gpu_share_percent {
        println!("  GPU time share at {} fps: {share:.2}%", command.fps);
    }
    if let Some(validated) = summary.validated {
        println!("  validation: {validated}/{frames} stamps read from the Y plane and matched");
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
    if let Some(path) = &command.summary_json {
        std::fs::write(path, serde_json::to_string_pretty(&summary)?)?;
        println!("  summary: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_use_nearest_rank() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = stats(&values).unwrap();
        assert_eq!((s.p50, s.p95, s.p99, s.max), (50.0, 95.0, 99.0, 100.0));
        assert_eq!(s.mean, 50.5);
        assert!(stats(&[]).is_none());
    }
}
