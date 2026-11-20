//! ziz: host or viewer of a screen-sharing session. Command line until the
//! UI (Dear ImGui) lands; several instances can run side by side, each with
//! its own profile.
//!
//! ```text
//! ziz --role host   --profile test-host   --synthetic
//! ziz --role viewer --profile test-viewer --connect 127.0.0.1:47100
//! ```

mod host;
mod profile;
mod source;
mod viewer;

use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, ValueEnum};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::BOOL;

use crate::profile::Profile;

const DEFAULT_SIGNALING_PORT: u16 = 47100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Role {
    Host,
    Viewer,
}

#[derive(Debug, Parser)]
#[command(name = "ziz", about = "Screen sharing: host or viewer.")]
struct Args {
    #[arg(long, value_enum)]
    role: Role,
    /// Profile name (own data directory, logs and id).
    #[arg(long)]
    profile: String,
    /// Profiles directory (default: %LOCALAPPDATA%\ziz-dev\profiles).
    #[arg(long)]
    profiles_dir: Option<PathBuf>,
    /// Stop after this many seconds (0: until Ctrl+C or the window closes).
    #[arg(long, default_value_t = 0)]
    seconds: u64,

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
    let role = match args.role {
        Role::Host => "host",
        Role::Viewer => "viewer",
    };
    let log = std::fs::File::create(profile.log_path(role))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(Mutex::new(log))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tracing::info!(profile = %profile.name, id = %profile.id, role, "starting");

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
        match args.role {
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
                },
                &STOP,
            ),
            Role::Viewer => viewer::run(
                &profile,
                &viewer::ViewerArgs {
                    connect: args.connect,
                    position: args.at,
                    stamp_every: args.stamp_every,
                    seconds: args.seconds,
                    summary_json: args.summary_json,
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
