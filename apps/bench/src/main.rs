//! `bench`: measurement and diagnostics CLI for ziz.

mod ab;
mod capture_cmd;
mod com;
mod convert_cmd;
mod encode_cmd;
mod frames;
mod os_info;
mod probe;
mod process_stats;
mod raw_source;
mod stamps_cmd;
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
    /// Convert paced synthetic frames from BGRA8 to NV12 and measure GPU time.
    Convert(ConvertArgs),
    /// Encode paced synthetic frames to H.264 with the hardware encoder.
    Encode(EncodeArgs),
    /// Check the frame stamps of a raw 8-bit luma file (decoded video).
    Stamps(StampsArgs),
}

#[derive(Debug, Args)]
struct StampsArgs {
    /// Raw 8-bit luma frames, back to back.
    #[arg(value_name = "FILE")]
    file: PathBuf,

    /// Frame size in the file [default: the stamp region, 1024x32].
    #[arg(long, default_value_t = 1024)]
    width: u32,
    #[arg(long, default_value_t = 32)]
    height: u32,
}

#[derive(Debug, Args)]
struct EncodeArgs {
    /// Hardware encoder API.
    #[arg(long, value_enum, default_value_t = EncoderArg::Mf)]
    encoder: EncoderArg,

    /// Frame source.
    #[arg(long, value_enum, default_value_t = EncodeSourceArg::Synthetic)]
    source: EncodeSourceArg,

    /// Raw NV12 file of --width x --height frames (with --source raw).
    #[arg(long, value_name = "FILE", required_if_eq("source", "raw"))]
    file: Option<PathBuf>,

    /// Monitor index, as listed by ench probe (with --source monitor).
    #[arg(long, default_value_t = 0)]
    monitor: usize,

    /// Input size (synthetic or raw frames; a monitor uses its own size).
    #[arg(long, default_value_t = 1920, value_parser = clap::value_parser!(u32).range(i64::from(MIN_WIDTH)..=16384))]
    width: u32,
    #[arg(long, default_value_t = 1080, value_parser = clap::value_parser!(u32).range(i64::from(MIN_HEIGHT)..=16384))]
    height: u32,

    /// Encoded size; both even [default: input size].
    #[arg(long, value_parser = clap::value_parser!(u32).range(2..=16384))]
    out_width: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(2..=16384))]
    out_height: Option<u32>,

    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,

    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=3600))]
    seconds: u32,

    /// Target bitrate in kbit/s (CBR).
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(100..=500_000))]
    kbps: u32,

    /// Frames between automatic keyframes [default: 10 s worth of frames].
    #[arg(long)]
    gop: Option<u32>,

    /// Request a keyframe every N frames to measure how fast it comes.
    #[arg(long, default_value_t = 0)]
    keyframe_every: u32,

    /// Annex B output file [default: <lab>/encode/<ms>-<encoder>-<w>x<h>-<kbps>kbps.h264].
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Write a JSON summary to this file.
    #[arg(long, value_name = "FILE")]
    summary_json: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ConvertArgs {
    /// Conversion method.
    #[arg(long, value_enum)]
    method: MethodArg,

    /// Input (synthetic frame) size.
    #[arg(long, default_value_t = 1920, value_parser = clap::value_parser!(u32).range(i64::from(MIN_WIDTH)..=16384))]
    width: u32,
    #[arg(long, default_value_t = 1080, value_parser = clap::value_parser!(u32).range(i64::from(MIN_HEIGHT)..=16384))]
    height: u32,

    /// Output (NV12) size; both must be even [default: input size].
    #[arg(long, value_parser = clap::value_parser!(u32).range(2..=16384))]
    out_width: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(2..=16384))]
    out_height: Option<u32>,

    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=1000))]
    fps: u32,

    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=3600))]
    seconds: u32,

    /// Read every frame stamp back from the Y plane (needs output = input size; stalls the GPU).
    #[arg(long)]
    validate: bool,

    /// Write a JSON summary to this file.
    #[arg(long, value_name = "FILE")]
    summary_json: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum EncodeSourceArg {
    /// Paced synthetic frames with visual stamps.
    Synthetic,
    /// A raw NV12 file (every frame encoded once; for quality metrics).
    Raw,
    /// WGC capture of a monitor (to measure the impact on a running game).
    Monitor,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EncoderArg {
    /// Media Foundation hardware MFT (any vendor).
    Mf,
    /// AMD AMF runtime (AMD GPUs only).
    Amf,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MethodArg {
    /// D3D11 video processor (VideoProcessorBlt).
    Vp,
    /// Own pixel shaders writing the NV12 planes.
    Shader,
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
        Command::Convert(args) => convert_cmd::run(convert_cmd::ConvertCommand {
            method: match args.method {
                MethodArg::Vp => convert_cmd::Method::VideoProcessor,
                MethodArg::Shader => convert_cmd::Method::Shader,
            },
            width: args.width,
            height: args.height,
            output_width: args.out_width.unwrap_or(args.width),
            output_height: args.out_height.unwrap_or(args.height),
            fps: args.fps,
            seconds: args.seconds,
            validate: args.validate,
            summary_json: args.summary_json,
        }),
        Command::Stamps(args) => stamps_cmd::run(&args.file, args.width, args.height),
        Command::Encode(args) => encode_cmd::run(encode_cmd::EncodeCommand {
            backend: match args.encoder {
                EncoderArg::Mf => encode_cmd::Backend::MediaFoundation,
                EncoderArg::Amf => encode_cmd::Backend::Amf,
            },
            source: match args.source {
                EncodeSourceArg::Synthetic => encode_cmd::Source::Synthetic,
                EncodeSourceArg::Raw => {
                    encode_cmd::Source::Raw(args.file.expect("required by clap with --source raw"))
                }
                EncodeSourceArg::Monitor => encode_cmd::Source::Monitor(args.monitor),
            },
            width: args.width,
            height: args.height,
            output_width: args.out_width,
            output_height: args.out_height,
            fps: args.fps,
            seconds: args.seconds,
            bitrate_bps: args.kbps * 1000,
            gop: args.gop.unwrap_or(args.fps * 10),
            keyframe_every: args.keyframe_every,
            output: args.output,
            summary_json: args.summary_json,
        }),
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
