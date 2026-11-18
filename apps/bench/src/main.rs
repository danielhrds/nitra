//! `bench`: measurement and diagnostics CLI for ziz.

mod ab;
mod capture_cmd;
mod com;
mod frames;
mod os_info;
mod probe;
mod process_stats;
mod synth;

use std::path::PathBuf;
use std::process::ExitCode;

use capture::synthetic::{MIN_HEIGHT, MIN_WIDTH, PatternMode};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "bench",
    version,
    about = "ziz measurement and diagnostics tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List adapters, outputs, capture support and hardware codecs (read-only; starts no capture).
    Probe(ProbeArgs),
    /// Render paced synthetic frames with a visual frame stamp on the GPU.
    Synth(SynthArgs),
    /// Compute frame-pacing metrics from PresentMon CSV captures.
    Frames(FramesArgs),
    /// Capture a monitor or window (WGC or DXGI) and report cost, cadence and stability.
    Capture(CaptureArgs),
    /// Compare frame-pacing metrics between capture conditions of an A/B session folder.
    Ab(AbArgs),
}

#[derive(Debug, Args)]
struct AbArgs {
    /// Session folder with run-<round>-<condition>.csv files.
    #[arg(value_name = "DIR")]
    dir: PathBuf,

    /// Condition every other one is compared with.
    #[arg(long, default_value = "none")]
    baseline: String,

    /// Only use frames of this executable.
    #[arg(long, value_name = "EXE")]
    process: Option<String>,

    /// Where to write the JSON report [default: ab-summary.json in DIR].
    #[arg(long, value_name = "FILE")]
    json: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackendArg {
    Wgc,
    Dxgi,
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("source").required(true).args(["monitor", "window", "test_window"])))]
struct CaptureArgs {
    /// Capture API (DXGI captures monitors only).
    #[arg(long, value_enum, default_value_t = BackendArg::Wgc)]
    backend: BackendArg,

    /// Write a JSON summary of the capture to this file (used by `bench ab`).
    #[arg(long, value_name = "FILE")]
    summary_json: Option<PathBuf>,

    /// Monitor index, in `bench probe` order (hardware adapters only).
    #[arg(long, value_name = "INDEX")]
    monitor: Option<usize>,

    /// First visible window whose title contains this text (case-insensitive).
    #[arg(long, value_name = "TITLE")]
    window: Option<String>,

    /// Capture an animated window created by bench itself.
    #[arg(long)]
    test_window: bool,

    /// Maximum delivered frame rate.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=1000))]
    fps: u32,

    /// Run length in seconds.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=86400))]
    seconds: u32,

    /// Leave the mouse cursor out of the frames.
    #[arg(long)]
    no_cursor: bool,

    /// Keep WGC's yellow capture border.
    #[arg(long)]
    show_border: bool,

    /// Print progress (fps, counters, handles, memory) every N seconds.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=3600))]
    report_every: u32,
}

#[derive(Debug, Args)]
struct FramesArgs {
    /// PresentMon CSV files, one per run.
    #[arg(required = true, value_name = "CSV")]
    files: Vec<PathBuf>,

    /// Only use frames of this executable (needed when a capture has several).
    #[arg(long, value_name = "EXE")]
    process: Option<String>,

    /// Where to write the JSON report [default: frames-summary.json next to the first CSV].
    #[arg(long, value_name = "FILE")]
    json: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SynthArgs {
    /// Target frame rate.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=1000))]
    fps: u32,

    /// Run length in seconds.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=3600))]
    seconds: u32,

    /// Pattern to render.
    #[arg(long, value_enum, default_value_t = ModeArg::Static)]
    mode: ModeArg,

    /// Frame width in pixels.
    #[arg(long, default_value_t = 1920, value_parser = clap::value_parser!(u32).range(i64::from(MIN_WIDTH)..=16384))]
    width: u32,

    /// Frame height in pixels.
    #[arg(long, default_value_t = 1080, value_parser = clap::value_parser!(u32).range(i64::from(MIN_HEIGHT)..=16384))]
    height: u32,

    /// Read every frame stamp back and check it (stalls the GPU; test mode).
    #[arg(long)]
    validate: bool,

    /// Write this frame as a BMP into the lab folder (full readback; diagnostics only).
    #[arg(long, value_name = "FRAME_ID")]
    dump_frame: Option<u32>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModeArg {
    Static,
    Motion,
}

impl From<ModeArg> for PatternMode {
    fn from(mode: ModeArg) -> Self {
        match mode {
            ModeArg::Static => PatternMode::Static,
            ModeArg::Motion => PatternMode::Motion,
        }
    }
}

#[derive(Debug, Args)]
struct ProbeArgs {
    /// Where to write the JSON report [default: <lab>/probe-<unix ms>.json].
    #[arg(long, value_name = "FILE")]
    json: Option<PathBuf>,

    /// Run the probe this many times and report handle and memory growth.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    repeat: u32,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    // Physical pixels everywhere; required by DXGI's DuplicateOutput1.
    capture::dxgi::enable_per_monitor_dpi_awareness();
    let result = match cli.command {
        Command::Probe(args) => probe::run(args.json, args.repeat),
        Command::Synth(args) => synth::run(synth::SynthOptions {
            fps: args.fps,
            seconds: args.seconds,
            mode: args.mode.into(),
            width: args.width,
            height: args.height,
            validate: args.validate,
            dump_frame: args.dump_frame,
        }),
        Command::Frames(args) => frames::run(&args.files, args.process.as_deref(), args.json),
        Command::Ab(args) => ab::run(
            &args.dir,
            &args.baseline,
            args.process.as_deref(),
            args.json,
        ),
        Command::Capture(args) => capture_cmd::run(capture_cmd::CaptureCommand {
            backend: match args.backend {
                BackendArg::Wgc => capture_cmd::Backend::Wgc,
                BackendArg::Dxgi => capture_cmd::Backend::Dxgi,
            },
            summary_json: args.summary_json,
            source: match (args.monitor, args.window) {
                (Some(index), _) => capture_cmd::Source::Monitor(index),
                (None, Some(title)) => capture_cmd::Source::Window(title),
                (None, None) => capture_cmd::Source::TestWindow,
            },
            fps: args.fps,
            seconds: args.seconds,
            cursor: !args.no_cursor,
            hide_border: !args.show_border,
            report_every: args.report_every,
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
