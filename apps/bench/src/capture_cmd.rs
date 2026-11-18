//! `bench capture`: runs a WGC or DXGI capture with a consumer that releases
//! every frame at once, and reports cost, cadence, counters and process
//! resource use over time (for long stability runs). With `--summary-json`
//! it also writes a machine-readable summary (used by `bench ab`).

use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use capture::test_window::TestWindow;
use capture::wgc::{CaptureTarget, find_window_by_title};
use capture::{Capture, CaptureEnd, CaptureOptions, CaptureReport, CounterSnapshot, Taken};
use gpu::DebugLayer;
use serde::Serialize;
use telemetry::{QpcFrequency, Session, Stage, StageRecorder, qpc_frequency};

use crate::com::ComApartment;
use crate::process_stats::{ProcessStats, cpu_percent_of_one_core};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Wgc,
    Dxgi,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Backend::Wgc => "wgc",
            Backend::Dxgi => "dxgi",
        }
    }
}

pub enum Source {
    Monitor(usize),
    Window(String),
    TestWindow,
}

pub struct CaptureCommand {
    pub backend: Backend,
    pub source: Source,
    pub fps: u32,
    pub seconds: u32,
    pub cursor: bool,
    pub hide_border: bool,
    pub report_every: u32,
    pub summary_json: Option<PathBuf>,
}

/// What the consumer observed.
struct Consumed {
    taken: u64,
    elapsed_s: f64,
    resources_start: ProcessStats,
    resources_end: ProcessStats,
    end: Option<CaptureEnd>,
}

/// Mean, min, nearest-rank percentiles and max, in the unit of the field name.
#[derive(Clone, Copy, Debug, Serialize)]
struct Stats {
    mean: f64,
    min: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

#[derive(Debug, Serialize)]
struct CaptureSummary {
    schema_version: u32,
    backend: &'static str,
    source: String,
    target_fps: u32,
    seconds: u32,
    frames_taken: u64,
    taken_fps: f64,
    end: String,
    counters: CounterSnapshot,
    thread_cpu_us: Option<Stats>,
    handoff_minus_frame_time_ms: Option<Stats>,
    interval_ms: Option<Stats>,
    handles_start: u32,
    handles_end: u32,
    private_mib_start: f64,
    private_mib_end: f64,
    /// CPU of the whole bench process during the capture (all frames,
    /// including the ones dropped by the rate limiter), % of one core.
    process_cpu_percent: f64,
}

pub fn run(command: CaptureCommand) -> Result<(), Box<dyn Error>> {
    let _com = ComApartment::multithreaded()?;
    let debug_layer = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };

    let result = {
        let adapters = gpu::enumerate_adapters()?;
        let adapter = adapters
            .iter()
            .find(|adapter| !adapter.info.is_software)
            .ok_or("no hardware adapter found")?;
        let device = gpu::create_device(adapter, debug_layer)?;
        let options = CaptureOptions {
            fps: command.fps,
            cursor: command.cursor,
            hide_border: command.hide_border,
            ..CaptureOptions::default()
        };

        // Keep the test window alive for the whole capture.
        let mut test_window = None;
        let (capture, description, label) = match (&command.source, command.backend) {
            (Source::Monitor(index), backend) => {
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
                let capture = match backend {
                    Backend::Wgc => capture::wgc::start(
                        &device,
                        CaptureTarget::Monitor(output.monitor()),
                        options,
                    )?,
                    Backend::Dxgi => capture::dxgi::start(&device, output, options)?,
                };
                (
                    capture,
                    format!("monitor {index} ({}{mode})", output.info.device_name),
                    match backend {
                        Backend::Wgc => "capture-wgc-monitor",
                        Backend::Dxgi => "capture-dxgi-monitor",
                    },
                )
            }
            (_, Backend::Dxgi) => {
                return Err("the DXGI backend captures monitors only; use --monitor".into());
            }
            (Source::Window(needle), Backend::Wgc) => {
                let (window, title) = find_window_by_title(needle)
                    .ok_or_else(|| format!("no visible window title contains {needle:?}"))?;
                let capture = capture::wgc::start(&device, CaptureTarget::Window(window), options)?;
                (capture, format!("window {title:?}"), "capture-wgc-window")
            }
            (Source::TestWindow, Backend::Wgc) => {
                let window =
                    TestWindow::open("ziz capture test window", 1280, 720, [40, 90, 160], true)?;
                std::thread::sleep(Duration::from_millis(300));
                let capture =
                    capture::wgc::start(&device, CaptureTarget::Window(window.hwnd()), options)?;
                test_window = Some(window);
                (
                    capture,
                    "animated test window (1280x720)".to_owned(),
                    "capture-wgc-test-window",
                )
            }
        };

        println!(
            "capture: {} {description} on {} at up to {} fps for {} s ({}, debug layer {}, cursor {})",
            command.backend.name(),
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
            if command.cursor && command.backend == Backend::Wgc {
                "on"
            } else {
                "off"
            },
        );

        let session = Session::start(label, &[("capture", command.backend.name())]);
        let consumed = consume(&capture, &command)?;
        let report = capture.stop();
        drop(test_window);

        let frequency = qpc_frequency();
        print_report(&report, frequency);

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
            // The summary replaces the generic session report (scripts put
            // it next to their other session files).
            let summary = summarize(&command, description, &consumed, &report, frequency);
            std::fs::write(path, serde_json::to_string_pretty(&summary)?)?;
            println!("  summary: {}", path.display());
        } else {
            let mut recorder = StageRecorder::new();
            for &cost in &report.cost {
                recorder.record_duration(Stage::Capture, cost);
            }
            let paths = session
                .finish(&recorder)
                .write_files(&telemetry::default_output_dir())?;
            println!("  report: {}", paths.json.display());
        }

        match consumed.end {
            None | Some(CaptureEnd::Stopped) | Some(CaptureEnd::SourceClosed) => Ok(()),
            Some(CaptureEnd::Failed(message)) => Err(message.into()),
        }
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
    result
}

/// Takes frames until the time is up or the capture ends, releasing each one
/// immediately, and prints progress every `report_every` seconds.
fn consume(capture: &Capture, command: &CaptureCommand) -> Result<Consumed, Box<dyn Error>> {
    let started = Instant::now();
    let duration = Duration::from_secs(u64::from(command.seconds));
    let report_every = Duration::from_secs(u64::from(command.report_every.max(1)));
    let mut next_report = report_every;
    let mut taken = 0_u64;
    let mut taken_at_last_report = 0_u64;
    let resources_start = ProcessStats::current()?;
    println!(
        "  {:>6}  {:>8}  {:>7}  {:>9}  {:>8}  {:>8}  {:>7}  {:>11}",
        "time", "fps", "taken", "delivered", "rate-drp", "pool-drp", "handles", "private MiB"
    );

    let end = loop {
        if started.elapsed() >= duration {
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
                mib(stats.private_bytes)
            );
            taken_at_last_report = taken;
            next_report += report_every;
        }
    };

    let elapsed_s = started.elapsed().as_secs_f64();
    let resources_end = ProcessStats::current()?;
    println!(
        "  resources: handles {} -> {} ({:+}), private {:.1} -> {:.1} MiB, process CPU {:.2}% of one core",
        resources_start.handles,
        resources_end.handles,
        i64::from(resources_end.handles) - i64::from(resources_start.handles),
        mib(resources_start.private_bytes),
        mib(resources_end.private_bytes),
        cpu_percent_of_one_core(&resources_start, &resources_end, elapsed_s)
    );
    if end == Some(CaptureEnd::SourceClosed) {
        println!("  source closed after {elapsed_s:.1} s");
    }
    Ok(Consumed {
        taken,
        elapsed_s,
        resources_start,
        resources_end,
        end,
    })
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn summarize(
    command: &CaptureCommand,
    source: String,
    consumed: &Consumed,
    report: &CaptureReport,
    frequency: QpcFrequency,
) -> CaptureSummary {
    let ticks = |samples: &[telemetry::QpcDuration]| -> Vec<i64> {
        samples
            .iter()
            .map(|duration| duration.ticks() as i64)
            .collect()
    };
    CaptureSummary {
        schema_version: 1,
        backend: command.backend.name(),
        source,
        target_fps: command.fps,
        seconds: command.seconds,
        frames_taken: consumed.taken,
        taken_fps: if consumed.elapsed_s > 0.0 {
            consumed.taken as f64 / consumed.elapsed_s
        } else {
            0.0
        },
        end: format!("{:?}", consumed.end.clone().unwrap_or(report.end.clone())),
        counters: report.counters,
        thread_cpu_us: stats_of_ticks(&ticks(&report.cost), frequency, 1_000_000.0),
        handoff_minus_frame_time_ms: stats_of_ticks(&report.frame_time_offset, frequency, 1_000.0),
        interval_ms: stats_of_ticks(&ticks(&report.intervals), frequency, 1_000.0),
        handles_start: consumed.resources_start.handles,
        handles_end: consumed.resources_end.handles,
        private_mib_start: mib(consumed.resources_start.private_bytes),
        private_mib_end: mib(consumed.resources_end.private_bytes),
        process_cpu_percent: cpu_percent_of_one_core(
            &consumed.resources_start,
            &consumed.resources_end,
            consumed.elapsed_s,
        ),
    }
}

/// Statistics of signed tick samples, scaled to `units_per_second`
/// (1e3 for ms, 1e6 for us). Percentiles are nearest-rank.
fn stats_of_ticks(
    samples: &[i64],
    frequency: QpcFrequency,
    units_per_second: f64,
) -> Option<Stats> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let scale = |ticks: f64| ticks * units_per_second / frequency.ticks_per_second() as f64;
    let rank = |percentile: usize| {
        let index = (percentile * sorted.len()).div_ceil(100).max(1) - 1;
        scale(sorted[index] as f64)
    };
    let mean = sorted.iter().map(|&ticks| ticks as f64).sum::<f64>() / sorted.len() as f64;
    Some(Stats {
        mean: scale(mean),
        min: scale(sorted[0] as f64),
        p50: rank(50),
        p95: rank(95),
        p99: rank(99),
        max: scale(sorted[sorted.len() - 1] as f64),
    })
}

fn print_report(report: &CaptureReport, frequency: QpcFrequency) {
    let counters = report.counters;
    println!(
        "  counters: arrived {}, delivered {}, superseded {}, rate-dropped {}, pool-dropped {}, replaced {}, resizes {}, access-lost {}",
        counters.arrived,
        counters.delivered,
        counters.superseded,
        counters.rate_dropped,
        counters.pool_dropped,
        counters.replaced,
        counters.resizes,
        counters.access_lost
    );
    if report.backend == "wgc" {
        println!(
            "  WGC options: border hidden {}, min update interval set {}",
            report.border_hidden, report.min_update_interval_set
        );
    }
    let ticks = |samples: &[telemetry::QpcDuration]| -> Vec<i64> {
        samples
            .iter()
            .map(|duration| duration.ticks() as i64)
            .collect()
    };
    print_stats(
        "thread CPU",
        "us",
        stats_of_ticks(&ticks(&report.cost), frequency, 1_000_000.0),
    );
    print_stats(
        "hand-off - frame time",
        "ms",
        stats_of_ticks(&report.frame_time_offset, frequency, 1_000.0),
    );
    print_stats(
        "interval",
        "ms",
        stats_of_ticks(&ticks(&report.intervals), frequency, 1_000.0),
    );
}

fn print_stats(name: &str, unit: &str, stats: Option<Stats>) {
    match stats {
        Some(stats) => println!(
            "  {name} ({unit}): mean {:.3}, min {:.3}, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}",
            stats.mean, stats.min, stats.p50, stats.p95, stats.p99, stats.max
        ),
        None => println!("  {name}: no samples"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_of_signed_ticks() {
        let frequency = QpcFrequency::new(1_000).unwrap(); // 1 tick = 1 ms
        let stats = stats_of_ticks(&[-3, -1, 2, 6], frequency, 1_000.0).unwrap();
        assert_eq!((stats.min, stats.max), (-3.0, 6.0));
        assert_eq!(stats.mean, 1.0);
        assert_eq!(stats.p50, -1.0);
        assert_eq!(stats.p99, 6.0);
        assert!(stats_of_ticks(&[], frequency, 1_000.0).is_none());
    }
}
