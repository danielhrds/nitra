//! `bench capture`: runs a WGC capture with a consumer that releases every
//! frame at once, and reports cost, latency, cadence, counters and process
//! resource use over time (for long stability runs).

use std::error::Error;
use std::time::{Duration, Instant};

use capture::test_window::TestWindow;
use capture::wgc::{CaptureTarget, find_window_by_title};
use capture::{Capture, CaptureEnd, CaptureOptions, Taken};
use gpu::DebugLayer;
use telemetry::{QpcDuration, QpcFrequency, Session, Stage, StageRecorder, Summary, qpc_frequency};

use crate::com::ComApartment;
use crate::process_stats::ProcessStats;

pub enum Source {
    Monitor(usize),
    Window(String),
    TestWindow,
}

pub struct CaptureCommand {
    pub source: Source,
    pub fps: u32,
    pub seconds: u32,
    pub cursor: bool,
    pub hide_border: bool,
    pub report_every: u32,
}

pub fn run(command: CaptureCommand) -> Result<(), Box<dyn Error>> {
    let _com = ComApartment::multithreaded()?;
    let debug_layer = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };

    let report = {
        let adapters = gpu::enumerate_adapters()?;
        let adapter = adapters
            .iter()
            .find(|adapter| !adapter.info.is_software)
            .ok_or("no hardware adapter found")?;
        let device = gpu::create_device(adapter, debug_layer)?;

        // Keep the test window alive for the whole capture.
        let mut test_window = None;
        let (target, description, label) = match &command.source {
            Source::Monitor(index) => {
                let outputs: Vec<_> = adapters
                    .iter()
                    .filter(|adapter| !adapter.info.is_software)
                    .flat_map(|adapter| adapter.outputs.iter())
                    .collect();
                let output = outputs
                    .get(*index)
                    .ok_or_else(|| format!("no monitor {index}; {} found", outputs.len()))?;
                let mode = output
                    .info
                    .current_mode
                    .map(|mode| {
                        format!(
                            " {}x{} @ {} Hz",
                            mode.width, mode.height, mode.refresh_rate_hz
                        )
                    })
                    .unwrap_or_default();
                (
                    CaptureTarget::Monitor(output.monitor()),
                    format!("monitor {index} ({}{mode})", output.info.device_name),
                    "capture-monitor",
                )
            }
            Source::Window(needle) => {
                let (window, title) = find_window_by_title(needle)
                    .ok_or_else(|| format!("no visible window title contains {needle:?}"))?;
                (
                    CaptureTarget::Window(window),
                    format!("window {title:?}"),
                    "capture-window",
                )
            }
            Source::TestWindow => {
                let window =
                    TestWindow::open("ziz capture test window", 1280, 720, [40, 90, 160], true)?;
                std::thread::sleep(Duration::from_millis(300));
                let target = CaptureTarget::Window(window.hwnd());
                test_window = Some(window);
                (
                    target,
                    "animated test window (1280x720)".to_owned(),
                    "capture-test-window",
                )
            }
        };

        println!(
            "capture: {description} on {} at up to {} fps for {} s ({}, debug layer {}, cursor {})",
            adapter.info.name,
            command.fps,
            command.seconds,
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            if device.debug_layer_enabled() {
                "on"
            } else {
                "off"
            },
            if command.cursor { "on" } else { "off" },
        );

        let session = Session::start(label, &[("capture", "wgc")]);
        let options = CaptureOptions {
            fps: command.fps,
            cursor: command.cursor,
            hide_border: command.hide_border,
            ..CaptureOptions::default()
        };
        let capture = capture::wgc::start(&device, target, options)?;
        let report = consume(&capture, &command);
        let capture_report = capture.stop();
        drop(test_window);

        print_report(&capture_report, qpc_frequency());

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

        let mut recorder = StageRecorder::new();
        for &cost in &capture_report.cost {
            recorder.record_duration(Stage::Capture, cost);
        }
        let paths = session
            .finish(&recorder)
            .write_files(&telemetry::default_output_dir())?;
        println!("  report: {}", paths.json.display());
        report
    };

    if cfg!(debug_assertions) {
        match gpu::report_live_objects() {
            None => println!("  DXGI live objects: not checked (Graphics Tools not installed)"),
            Some(messages) if messages.is_empty() => println!("  DXGI live objects: none"),
            Some(messages) => {
                println!("  DXGI live objects: {} message(s)", messages.len());
                for message in messages {
                    println!("    {message}");
                }
            }
        }
    }
    report
}

/// Takes frames until the time is up or the capture ends, releasing each one
/// immediately, and prints progress every `report_every` seconds.
fn consume(capture: &Capture, command: &CaptureCommand) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let duration = Duration::from_secs(u64::from(command.seconds));
    let report_every = Duration::from_secs(u64::from(command.report_every.max(1)));
    let mut next_report = report_every;
    let mut taken = 0_u64;
    let mut taken_at_last_report = 0_u64;
    let baseline = ProcessStats::current()?;
    println!(
        "  {:>6}  {:>8}  {:>7}  {:>9}  {:>8}  {:>8}  {:>7}  {:>11}",
        "time", "fps", "taken", "delivered", "rate-drp", "pool-drp", "handles", "private MiB"
    );

    let end = loop {
        let elapsed = started.elapsed();
        if elapsed >= duration {
            break None;
        }
        match capture.next(Duration::from_millis(200)) {
            Taken::Value(frame) => {
                drop(frame);
                taken += 1;
            }
            Taken::Ended(end) => break Some(end),
            Taken::Timeout => {}
        }
        if started.elapsed() >= next_report {
            let stats = ProcessStats::current()?;
            let counters = capture.counters();
            println!(
                "  {:>5}s  {:>8.2}  {:>7}  {:>9}  {:>8}  {:>8}  {:>7}  {:>11.1}",
                next_report.as_secs(),
                (taken - taken_at_last_report) as f64 / report_every.as_secs_f64(),
                taken,
                counters.delivered,
                counters.rate_dropped,
                counters.pool_dropped,
                stats.handles,
                stats.private_bytes as f64 / (1024.0 * 1024.0)
            );
            taken_at_last_report = taken;
            next_report += report_every;
        }
    };

    let stats = ProcessStats::current()?;
    println!(
        "  resources: handles {} -> {} ({:+}), private {:.1} -> {:.1} MiB",
        baseline.handles,
        stats.handles,
        i64::from(stats.handles) - i64::from(baseline.handles),
        baseline.private_bytes as f64 / (1024.0 * 1024.0),
        stats.private_bytes as f64 / (1024.0 * 1024.0)
    );
    match end {
        None | Some(CaptureEnd::Stopped) => Ok(()),
        Some(CaptureEnd::SourceClosed) => {
            println!(
                "  source closed after {:.1} s",
                started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        Some(CaptureEnd::Failed(message)) => Err(message.into()),
    }
}

fn print_report(report: &capture::CaptureReport, frequency: QpcFrequency) {
    let counters = report.counters;
    println!(
        "  counters: arrived {}, delivered {}, superseded {}, rate-dropped {}, pool-dropped {}, replaced {}, resizes {}",
        counters.arrived,
        counters.delivered,
        counters.superseded,
        counters.rate_dropped,
        counters.pool_dropped,
        counters.replaced,
        counters.resizes
    );
    println!(
        "  WGC options: border hidden {}, min update interval set {}",
        report.border_hidden, report.min_update_interval_set
    );
    print_summary("thread CPU", &report.cost, frequency, 1.0, "us");
    print_signed_summary(
        "hand-off - WGC frame time",
        &report.frame_time_offset,
        frequency,
    );
    print_summary("interval", &report.intervals, frequency, 1_000.0, "ms");
}

/// Mean, nearest-rank percentiles, min and max of signed tick samples, in ms.
fn print_signed_summary(name: &str, samples: &[i64], frequency: QpcFrequency) {
    if samples.is_empty() {
        println!("  {name}: no samples");
        return;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let to_ms = |ticks: f64| ticks * 1_000.0 / frequency.ticks_per_second() as f64;
    let rank = |percentile: usize| {
        let index = (percentile * sorted.len()).div_ceil(100).max(1) - 1;
        to_ms(sorted[index] as f64)
    };
    let mean = sorted.iter().map(|&ticks| ticks as f64).sum::<f64>() / sorted.len() as f64;
    println!(
        "  {name} (ms): mean {:.3}, min {:.3}, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}",
        to_ms(mean),
        to_ms(sorted[0] as f64),
        rank(50),
        rank(95),
        rank(99),
        to_ms(sorted[sorted.len() - 1] as f64)
    );
}

fn print_summary(
    name: &str,
    samples: &[QpcDuration],
    frequency: QpcFrequency,
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
