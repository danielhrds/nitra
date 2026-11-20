//! ziz: host or viewer of a screen-sharing session. Command line until the
//! UI (Dear ImGui) lands; several instances can run side by side, each with
//! its own profile.
//!
//! ```text
//! ziz --profile test-viewer --print-key           (prints VIEWER-KEY)
//! ziz --profile test-host   --print-key           (prints HOST-KEY)
//! ziz --role host   --profile test-host   --synthetic --trust VIEWER-KEY
//! ziz --role viewer --profile test-viewer --connect 127.0.0.1:47100 --host-key HOST-KEY
//! ```

mod audio_out;
mod check;
mod fanout;
mod host;
mod profile;
mod secure;
mod source;
mod viewer;

use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::BOOL;

use identity::PublicKey;
use signaling::signed::Trust;
use transport::impair::ImpairmentConfig;

use crate::profile::Profile;

const DEFAULT_SIGNALING_PORT: u16 = 47100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Role {
    Host,
    Viewer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum AudioArg {
    None,
    System,
    App,
    Synthetic,
}

#[derive(Debug, Parser)]
#[command(name = "ziz", about = "Screen sharing: host or viewer.")]
struct Args {
    #[arg(long, value_enum, required_unless_present = "print_key")]
    role: Option<Role>,
    /// Profile name (own data directory, logs and identity key).
    #[arg(long)]
    profile: String,
    /// Profiles directory (default: %LOCALAPPDATA%\ziz-dev\profiles).
    #[arg(long)]
    profiles_dir: Option<PathBuf>,
    /// Stop after this many seconds (0: until Ctrl+C or the window closes).
    #[arg(long, default_value_t = 0)]
    seconds: u64,
    /// Print this profile's key code (what others pass to --trust or
    /// --host-key) and exit.
    #[arg(long)]
    print_key: bool,

    /// Host: signaling port on 127.0.0.1.
    #[arg(long, default_value_t = DEFAULT_SIGNALING_PORT)]
    port: u16,
    /// Host: share monitor N (bench probe order).
    #[arg(long, conflicts_with = "synthetic")]
    monitor: Option<usize>,
    /// Host: share a synthetic stamped window instead (latency measurements).
    #[arg(long)]
    synthetic: bool,
    #[arg(long, default_value_t = 1920)]
    width: u32,
    #[arg(long, default_value_t = 1080)]
    height: u32,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value_t = 10_000)]
    kbps: u32,
    /// Host: sound to share. `system` is everything playing except this
    /// process; on one PC with a viewer playing, that captures the viewer
    /// too (echo): use `synthetic` or `none` there.
    #[arg(long, value_enum, default_value_t = AudioArg::None)]
    audio: AudioArg,
    /// Host: process id for --audio app.
    #[arg(long, required_if_eq("audio", "app"))]
    audio_pid: Option<u32>,
    #[arg(long, default_value_t = 128)]
    audio_kbps: u32,
    /// Host and viewer: addresses to offer. `loopback` (one PC), `lan` (every
    /// adapter) or `internet` (also public addresses from STUN servers).
    #[arg(long = "network", value_enum, default_value_t = NetworkArg::Loopback)]
    network_scope: NetworkArg,
    /// Host: a viewer key code allowed to watch (repeatable). Viewers whose
    /// key is not listed are refused, and their code is shown.
    #[arg(long = "trust", value_name = "KEY")]
    trust: Vec<PublicKey>,
    /// Host: accept any viewer whose messages are correctly signed
    /// (development on one PC). Still checks signatures, expiry and replays.
    #[arg(long, conflicts_with = "trust")]
    trust_anyone: bool,

    /// Viewer: receive and decode the sound, but do not play it.
    #[arg(long)]
    mute: bool,
    /// Viewer: the host's key code; the answer must be signed by it.
    #[arg(long, value_name = "KEY", required_if_eq("role", "viewer"))]
    host_key: Option<PublicKey>,
    /// Viewer: the host's signaling address.
    #[arg(long, default_value_t = SocketAddr::from(([127, 0, 0, 1], DEFAULT_SIGNALING_PORT)))]
    connect: SocketAddr,
    /// Viewer: window position "X,Y" (opens without taking the focus).
    #[arg(long, value_parser = parse_position, allow_hyphen_values = true)]
    at: Option<(i32, i32)>,
    /// Viewer: read the frame stamp every N frames (host --synthetic; 0: off).
    #[arg(long, default_value_t = 10)]
    stamp_every: u32,
    /// Viewer: write the summary as JSON.
    #[arg(long)]
    summary_json: Option<PathBuf>,
    /// Viewer: no window; check the stream only (sink for host measurements).
    #[arg(long)]
    headless: bool,
    /// Viewer with --headless: also decode on the GPU (nothing presented).
    #[arg(long, requires = "headless")]
    decode: bool,

    #[command(flatten)]
    network: NetworkArgs,
}

/// Viewer (testing): impairs what the viewer receives. See `transport::impair`.
#[derive(Debug, clap::Args)]
struct NetworkArgs {
    /// Random loss, 0..1.
    #[arg(long, default_value_t = 0.0)]
    loss: f64,
    /// Loss bursts "P,N": a burst starts at each datagram with probability P and drops N.
    #[arg(long, value_parser = parse_burst)]
    burst: Option<(f64, u32)>,
    #[arg(long, default_value_t = 0)]
    delay_ms: u64,
    /// Delay varies by up to ± this.
    #[arg(long, default_value_t = 0)]
    jitter_ms: u64,
    /// Probability that a datagram is overtaken by later ones, 0..1.
    #[arg(long, default_value_t = 0.0)]
    reorder: f64,
    /// A bottleneck link of this rate (queue, then tail drop).
    #[arg(long)]
    limit_kbps: Option<u64>,
    /// The bottleneck's queue.
    #[arg(long, default_value_t = 100)]
    queue_ms: u64,
    /// Drop everything "AT,LEN" seconds after joining (e.g. 10,2).
    #[arg(long, value_parser = parse_outage)]
    outage: Option<(Duration, Duration)>,
    /// Hold everything "AT,LEN" seconds after joining, then deliver it at once (a hiccup).
    #[arg(long, value_parser = parse_outage)]
    stall: Option<(Duration, Duration)>,
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

impl NetworkArgs {
    fn impairment(&self) -> Option<ImpairmentConfig> {
        let config = ImpairmentConfig {
            loss: self.loss,
            burst: self.burst,
            delay: Duration::from_millis(self.delay_ms),
            jitter: Duration::from_millis(self.jitter_ms),
            reorder: self.reorder,
            rate_bps: self.limit_kbps.map(|kbps| kbps * 1000),
            queue: Duration::from_millis(self.queue_ms),
            outage: self.outage,
            stall: self.stall,
            seed: self.seed,
        };
        // Seed and queue alone impair nothing.
        let none = ImpairmentConfig {
            seed: config.seed,
            queue: config.queue,
            ..Default::default()
        };
        (config != none).then_some(config)
    }
}

fn parse_position(value: &str) -> Result<(i32, i32), String> {
    let (x, y) = value
        .split_once(',')
        .ok_or_else(|| format!("expected X,Y, got {value}"))?;
    let number = |text: &str| {
        text.trim()
            .parse::<i32>()
            .map_err(|e| format!("{text}: {e}"))
    };
    Ok((number(x)?, number(y)?))
}

fn parse_burst(value: &str) -> Result<(f64, u32), String> {
    let (p, n) = value
        .split_once(',')
        .ok_or_else(|| format!("expected P,N, got {value}"))?;
    let p: f64 = p.trim().parse().map_err(|e| format!("{p}: {e}"))?;
    let n: u32 = n.trim().parse().map_err(|e| format!("{n}: {e}"))?;
    Ok((p, n))
}

fn parse_outage(value: &str) -> Result<(Duration, Duration), String> {
    let (at, length) = value
        .split_once(',')
        .ok_or_else(|| format!("expected AT,LEN in seconds, got {value}"))?;
    let seconds = |text: &str| {
        text.trim()
            .parse::<f64>()
            .map_err(|e| format!("{text}: {e}"))
            .and_then(|s| Duration::try_from_secs_f64(s).map_err(|e| format!("{text}: {e}")))
    };
    Ok((seconds(at)?, seconds(length)?))
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "system" fn on_console_ctrl(_kind: u32) -> BOOL {
    STOP.store(true, Ordering::Relaxed);
    true.into()
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let root = args
        .profiles_dir
        .clone()
        .unwrap_or_else(profile::default_root);
    let profile = Profile::open(&root, &args.profile)?;
    if args.print_key {
        println!("{}", profile.identity.public_key());
        return Ok(());
    }
    let role = args.role.ok_or("--role is required")?;
    let role_name = match role {
        Role::Host => "host",
        Role::Viewer => "viewer",
    };
    let log = std::fs::File::create(profile.log_path(role_name))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(Mutex::new(log))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tracing::info!(profile = %profile.name, key = %profile.identity.public_key(), role = role_name, "starting");

    // SAFETY: process-wide settings with constant arguments; the handler is
    // a plain function that only stores to an atomic. COM is joined for this
    // thread (Media Foundation, WGC) and left at the end of `run`.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        SetConsoleCtrlHandler(Some(on_console_ctrl), true)?;
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    }
    let result = (|| -> Result<(), Box<dyn Error>> {
        let _media_foundation = mf::MediaFoundation::start()?;
        match role {
            Role::Host => host::run(
                &profile,
                &host::HostArgs {
                    port: args.port,
                    source: match (args.synthetic, args.monitor) {
                        (true, _) => host::Source::Synthetic {
                            width: args.width,
                            height: args.height,
                        },
                        (false, index) => host::Source::Monitor(index.unwrap_or(0)),
                    },
                    fps: args.fps,
                    bitrate_bps: args.kbps * 1000,
                    seconds: args.seconds,
                    audio: match args.audio {
                        AudioArg::None => None,
                        AudioArg::System => Some(audio::capture::CaptureSource::System),
                        AudioArg::App => Some(audio::capture::CaptureSource::App(
                            args.audio_pid.unwrap_or(0),
                        )),
                        AudioArg::Synthetic => Some(audio::capture::CaptureSource::Synthetic),
                    },
                    audio_bitrate_bps: args.audio_kbps * 1000,
                    network: network_config(args.network_scope),
                    trust: if args.trust_anyone {
                        Trust::Anyone
                    } else {
                        Trust::Keys(args.trust.clone())
                    },
                },
                &STOP,
            ),
            Role::Viewer => viewer::run(
                &profile,
                &viewer::ViewerArgs {
                    connect: args.connect,
                    host_key: args.host_key.ok_or("--host-key is required")?,
                    position: args.at,
                    stamp_every: args.stamp_every,
                    seconds: args.seconds,
                    summary_json: args.summary_json,
                    headless: args.headless,
                    decode: args.decode,
                    impairment: args.network.impairment(),
                    mute: args.mute,
                    network: network_config(args.network_scope),
                },
                &STOP,
            ),
        }
    })();
    // SAFETY: balances the CoInitializeEx above, after every COM object is gone.
    unsafe { CoUninitialize() };
    tracing::info!(ok = result.is_ok(), "finished");
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum NetworkArg {
    Loopback,
    Lan,
    Internet,
}

fn network_config(scope: NetworkArg) -> transport::gather::NetworkConfig {
    use transport::gather::{NetworkConfig, Scope};
    match scope {
        NetworkArg::Loopback => NetworkConfig::loopback(),
        NetworkArg::Lan => NetworkConfig {
            scope: Scope::Lan,
            stun_servers: Vec::new(),
        },
        NetworkArg::Internet => NetworkConfig::internet(),
    }
}
