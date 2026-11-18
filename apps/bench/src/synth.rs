//! `bench synth`: renders paced synthetic frames on the GPU and reports how
//! well the requested rate was held.

use std::error::Error;

use capture::synthetic::{Pacer, PatternMode, SyntheticConfig, SyntheticSource};
use gpu::DebugLayer;
use telemetry::{QpcDuration, Session, Stage, StageRecorder, Summary, qpc_frequency, qpc_now};

pub struct SynthOptions {
    pub fps: u32,
    pub seconds: u32,
    pub mode: PatternMode,
    pub width: u32,
    pub height: u32,
    pub validate: bool,
    pub dump_frame: Option<u32>,
}

pub fn run(options: SynthOptions) -> Result<(), Box<dyn Error>> {
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
            width: options.width,
            height: options.height,
            mode: options.mode,
        },
    )?;

    println!(
        "synth: {}x{} {} @ {} fps for {} s on {} ({}, debug layer {})",
        options.width,
        options.height,
        options.mode.name(),
        options.fps,
        options.seconds,
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
    if options.validate {
        println!(
            "  --validate reads every stamp back and stalls the GPU: timings are not representative"
        );
    }

    let frequency = qpc_frequency();
    let total_slots = u64::from(options.fps) * u64::from(options.seconds);
    let label = match options.mode {
        PatternMode::Static => "synth-static",
        PatternMode::Motion => "synth-motion",
    };
    let session = Session::start(
        label,
        &[("source", "synthetic"), ("pattern", options.mode.name())],
    );
    let mut recorder = StageRecorder::new();
    let mut intervals = Vec::with_capacity(total_slots as usize);
    let mut lateness = Vec::with_capacity(total_slots as usize);
    let mut skipped = 0_u64;
    let mut rendered = 0_u64;
    let mut validated = 0_u64;
    let mut dumped = None;
    let mut previous_start = None;
    let mut first_start = None;
    let mut last_start = None;

    let mut pacer = Pacer::start_now(options.fps);
    loop {
        let slot = pacer.next_slot(qpc_now());
        if slot.index >= total_slots {
            break;
        }
        skipped += slot.skipped;
        pacer.wait(&slot);

        let start = qpc_now();
        let frame = source.render_next()?;
        let end = qpc_now();
        rendered += 1;

        recorder.record(Stage::Capture, start, end);
        lateness.push(start.duration_since(slot.deadline));
        if let Some(previous) = previous_start {
            intervals.push(start.duration_since(previous));
        }
        previous_start = Some(start);
        first_start.get_or_insert(start);
        last_start = Some(start);

        if options.validate {
            let read = source.read_stamp(&frame)?;
            if read != frame.stamp {
                return Err(format!(
                    "frame {}: read back {read:?}, expected {:?}",
                    frame.stamp.frame_id, frame.stamp
                )
                .into());
            }
            validated += 1;
        }
        if options.dump_frame == Some(frame.stamp.frame_id) {
            let path = telemetry::default_output_dir().join(format!(
                "synth-{}-{}x{}-frame{}-{}.bmp",
                options.mode.name(),
                options.width,
                options.height,
                frame.stamp.frame_id,
                frame.stamp.timestamp.ticks()
            ));
            std::fs::create_dir_all(path.parent().expect("file has a parent"))?;
            source.dump_bmp(&frame, &path)?;
            dumped = Some(path);
        }
    }

    let elapsed_s = match (first_start, last_start) {
        (Some(first), Some(last)) => last.duration_since(first).as_micros(frequency) / 1e6,
        _ => 0.0,
    };
    let achieved_fps = if elapsed_s > 0.0 {
        (rendered.saturating_sub(1)) as f64 / elapsed_s
    } else {
        0.0
    };

    println!(
        "  frames: {rendered} rendered of {total_slots} slots, {skipped} skipped, achieved {achieved_fps:.2} fps"
    );
    print_summary("interval", &intervals, frequency, 1_000.0, "ms");
    print_summary("lateness", &lateness, frequency, 1_000.0, "ms");
    print_summary(
        "submit CPU",
        recorder.samples(Stage::Capture),
        frequency,
        1.0,
        "us",
    );
    if options.validate {
        println!("  validation: {validated}/{rendered} stamps read back and matched");
    }
    if let Some(path) = &dumped {
        println!("  frame dump: {}", path.display());
    } else if let Some(frame_id) = options.dump_frame {
        println!("  frame dump: frame {frame_id} was not rendered (skipped or out of range)");
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

    let report = session.finish(&recorder);
    let paths = report.write_files(&telemetry::default_output_dir())?;
    println!("  report: {}", paths.json.display());
    Ok(())
}

/// Prints mean/p50/p95/p99/max of `samples`, dividing microseconds by `divisor`.
fn print_summary(
    name: &str,
    samples: &[QpcDuration],
    frequency: telemetry::QpcFrequency,
    divisor: f64,
    unit: &str,
) {
    match Summary::from_durations(samples, frequency) {
        Some(summary) => println!(
            "  {name:<10} ({unit}): mean {:.3}, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}",
            summary.mean_us / divisor,
            summary.p50_us / divisor,
            summary.p95_us / divisor,
            summary.p99_us / divisor,
            summary.max_us / divisor
        ),
        None => println!("  {name:<10}: no samples"),
    }
}
