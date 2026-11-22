//! ziz: screen sharing in rooms. Two executables share this library:
//! `ziz.exe`, the app's window (no console), and `ziz-cli.exe`, the command
//! line for tests and measurements (host or viewer with `--role`; without it,
//! the window too). Several instances can run side by side, each with its
//! own profile.
//!
//! ```text
//! ziz-cli --profile test-viewer --print-key           (prints VIEWER-KEY)
//! ziz-cli --profile test-host   --print-key           (prints HOST-KEY)
//! ziz-cli --role host   --profile test-host   --synthetic --trust VIEWER-KEY
//! ziz-cli --role viewer --profile test-viewer --connect 127.0.0.1:47100 --host-key HOST-KEY
//! ziz-cli --role host   --profile me     --invite    (manual: invite on the clipboard)
//! ziz-cli --role viewer --profile friend --join      (reads it, answers on the clipboard)
//! ```

mod audio_out;
mod check;
mod clipboard;
mod contacts;
mod fanout;
mod hardware;
mod host;
mod manual;
mod preview;
mod profile;
mod room;
mod secure;
mod source;
#[cfg(test)]
mod testing;
mod ui;
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
/// The room server on this PC (`npm run dev` in worker/).
const LOCAL_ROOM_SERVER: &str = "ws://127.0.0.1:8787";
/// The development room server on Cloudflare.
const DEV_ROOM_SERVER: &str = "wss://ziz-coordination-dev.ziz-coordination.workers.dev";
/// The production room server on Cloudflare (the beta's).
const PRODUCTION_ROOM_SERVER: &str = "wss://ziz-coordination.ziz-coordination.workers.dev";

/// A beta build: `ZIZ_CHANNEL=beta` when it was compiled (`scripts/release.ps1`).
/// It keeps its data in its own folder and uses the production room server;
/// every other build is a development one, apart from the friends' rooms.
pub(crate) const BETA: bool = match option_env!("ZIZ_CHANNEL") {
    Some(channel) => same_bytes(channel.as_bytes(), b"beta"),
    None => false,
};

/// Byte equality usable in a constant (`==` on strings is not, yet).
const fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The room server a build uses unless `--room-server` says otherwise.
fn default_room_server() -> &'static str {
    if BETA {
        PRODUCTION_ROOM_SERVER
    } else {
        DEV_ROOM_SERVER
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Role {
    Host,
    Viewer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum AudioArg {
    None,
    /// What the source calls for: a window's own app, or for a monitor
    /// everything except the excluded apps (Discord by default) and ziz.
    Auto,
    System,
    App,
    Synthetic,
}

#[derive(Debug, Parser)]
#[command(
    name = "ziz",
    about = "Screen sharing in rooms. Without --role it opens the app's window."
)]
struct Args {
    /// Command-line host or viewer (tests, measurements); without it, the window.
    #[arg(long, value_enum)]
    role: Option<Role>,
    /// Profile name (own data directory, logs and identity key).
    #[arg(long, default_value = "default")]
    profile: String,
    /// Profiles directory (default: %LOCALAPPDATA%\ziz\profiles in a beta build,
    /// %LOCALAPPDATA%\ziz-dev\profiles otherwise).
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
    #[arg(long, conflicts_with_all = ["synthetic", "window"])]
    monitor: Option<usize>,
    /// Host: share the first visible window whose title contains this
    /// (scaled to --width x --height).
    #[arg(long, conflicts_with = "synthetic")]
    window: Option<String>,
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
    /// Host: sound to share. `auto` follows the source (a window: only its
    /// app; a monitor: everything except the excluded apps and any ziz).
    /// `system` is everything except this process (a viewer on the same PC
    /// would be captured: echo).
    #[arg(long, value_enum, default_value_t = AudioArg::None)]
    audio: AudioArg,
    /// Host: process id for --audio app.
    #[arg(long, required_if_eq("audio", "app"))]
    audio_pid: Option<u32>,
    /// Host: with --audio auto on a monitor, also leave out this app
    /// (executable name, e.g. TeamSpeak.exe); repeatable. Discord is
    /// always left out.
    #[arg(long = "audio-exclude")]
    audio_exclude: Vec<String>,
    #[arg(long, default_value_t = 128)]
    audio_kbps: u32,
    /// Host and viewer: addresses to offer. `loopback` (one PC), `lan` (every
    /// adapter) or `internet` (also public addresses from STUN servers).
    /// Default: `internet` with --invite or --join, else `loopback`.
    #[arg(long = "network", value_enum)]
    network_scope: Option<NetworkArg>,
    /// Host: also invite one viewer by a text to copy and paste (no server):
    /// the invite goes to the clipboard, then the answer is read from it.
    #[arg(long)]
    invite: bool,
    /// Viewer: join with a host's invite (read from the clipboard) instead
    /// of local signaling; the answer goes to the clipboard.
    #[arg(long, conflicts_with = "connect")]
    join: bool,
    /// With --invite or --join: the invite text in this file instead of the
    /// clipboard (scripts, tests).
    #[arg(long, value_name = "PATH")]
    invite_file: Option<PathBuf>,
    /// With --invite or --join: the answer text in this file instead of the
    /// clipboard.
    #[arg(long, value_name = "PATH")]
    answer_file: Option<PathBuf>,
    /// Host: a viewer key code allowed to watch (repeatable). Viewers whose
    /// key is not listed are refused, and their code is shown.
    #[arg(long = "trust", value_name = "KEY")]
    trust: Vec<PublicKey>,
    /// Host: accept any viewer whose messages are correctly signed
    /// (development on one PC). Still checks signatures, expiry and replays.
    #[arg(long, conflicts_with = "trust")]
    trust_anyone: bool,

    /// Rooms: the room server (`ws://` or `wss://`). Default: the local one
    /// (`ws://127.0.0.1:8787`) with --role, the development server in the window.
    #[arg(long)]
    room_server: Option<String>,
    /// Rooms: use this room (as a participant already).
    #[arg(long, value_name = "ID", conflicts_with_all = ["room_create", "room_join"])]
    room: Option<String>,
    /// Host: create a new room with this name (its id is printed).
    #[arg(long, value_name = "NAME", conflicts_with = "room_join")]
    room_create: Option<String>,
    /// Rooms: join with a room invite text (`zizroom:...`).
    #[arg(long, value_name = "INVITE")]
    room_join: Option<String>,
    /// Host in a room: also make a room invite (1 hour, 5 uses), printed and
    /// copied to the clipboard.
    #[arg(long)]
    room_invite: bool,
    /// Viewer in a room: the key code of the participant whose screen to watch.
    #[arg(long, value_name = "KEY")]
    watch: Option<PublicKey>,

    /// Viewer: receive and decode the sound, but do not play it.
    #[arg(long)]
    mute: bool,
    /// Viewer: the host's key code; the answer must be signed by it.
    /// Required without --join; with --join, trusts that key without asking.
    #[arg(long, value_name = "KEY")]
    host_key: Option<PublicKey>,
    /// Viewer: the host's signaling address (default 127.0.0.1:47100).
    #[arg(long)]
    connect: Option<SocketAddr>,
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

/// Which executable is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `ziz.exe`: the app's window, with no console to print to, so errors
    /// go to a message box.
    Window,
    /// `ziz-cli.exe`: the command line (a console program).
    Cli,
}

/// The executables' `main`.
pub fn main_entry(kind: Kind) {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) if kind == Kind::Window => {
            show_error(&error.to_string());
            std::process::exit(2);
        }
        Err(error) => error.exit(),
    };
    if kind == Kind::Window && (args.role.is_some() || args.print_key) {
        show_error("O ziz.exe é a janela do app. Para --role e --print-key, use o ziz-cli.exe.");
        std::process::exit(2);
    }
    if let Err(error) = run(args) {
        match kind {
            Kind::Window => show_error(&format!("O ziz não abriu: {error}")),
            Kind::Cli => eprintln!("error: {error}"),
        }
        std::process::exit(1);
    }
}

/// A message box for the window app, which has no console.
fn show_error(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let text: Vec<u16> = text.encode_utf16().chain([0]).collect();
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call;
    // no owner window.
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(text.as_ptr()),
            windows::core::w!("ziz"),
            MB_OK | MB_ICONERROR,
        );
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
    let Some(role) = args.role else {
        return run_window(&profile, &args);
    };
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
        let network_scope = args.network_scope.unwrap_or(if args.invite || args.join {
            NetworkArg::Internet
        } else {
            NetworkArg::Loopback
        });
        let manual = manual::ManualArgs {
            invite_file: args.invite_file.clone(),
            answer_file: args.answer_file.clone(),
        };
        let room = room_args(&args, role)?;
        match role {
            Role::Host => host::run(
                &profile,
                &host::HostArgs {
                    port: args.port,
                    source: match (args.synthetic, &args.window, args.monitor) {
                        (true, _, _) => host::Source::Synthetic {
                            width: args.width,
                            height: args.height,
                        },
                        (false, Some(title), _) => host::Source::Window {
                            target: host::WindowTarget::Title(title.clone()),
                            width: args.width,
                            height: args.height,
                        },
                        (false, None, index) => host::Source::Monitor {
                            index: index.unwrap_or(0),
                            size: None,
                        },
                    },
                    fps: args.fps,
                    bitrate_bps: args.kbps * 1000,
                    seconds: args.seconds,
                    audio: match args.audio {
                        AudioArg::None => host::AudioPlan::Off,
                        AudioArg::Auto => host::AudioPlan::Auto {
                            also_excluded: args.audio_exclude.clone(),
                        },
                        AudioArg::System => {
                            host::AudioPlan::Fixed(audio::capture::CaptureSource::System)
                        }
                        AudioArg::App => host::AudioPlan::Fixed(
                            audio::capture::CaptureSource::App(args.audio_pid.unwrap_or(0)),
                        ),
                        AudioArg::Synthetic => {
                            host::AudioPlan::Fixed(audio::capture::CaptureSource::Synthetic)
                        }
                    },
                    audio_bitrate_bps: args.audio_kbps * 1000,
                    network: network_config(network_scope),
                    trust: if args.trust_anyone {
                        Trust::Anyone
                    } else {
                        Trust::Keys(args.trust.clone())
                    },
                    invite: args.invite.then(|| manual.clone()),
                    room,
                    audience: None,
                    preview_interval: Some(preview::INTERVAL),
                },
                &STOP,
            ),
            Role::Viewer => viewer::run(
                &profile,
                &viewer::ViewerArgs {
                    signaling: if let Some(room) = room {
                        viewer::ViewerSignaling::Room {
                            room,
                            host_key: args.watch.ok_or("--watch KEY is required in a room")?,
                        }
                    } else if args.join {
                        viewer::ViewerSignaling::Manual {
                            manual: manual.clone(),
                            host_key: args.host_key,
                        }
                    } else {
                        viewer::ViewerSignaling::Local {
                            connect: args.connect.unwrap_or(SocketAddr::from((
                                [127, 0, 0, 1],
                                DEFAULT_SIGNALING_PORT,
                            ))),
                            host_key: args.host_key.ok_or("--host-key is required")?,
                        }
                    },
                    position: args.at,
                    stamp_every: args.stamp_every,
                    seconds: args.seconds,
                    summary_json: args.summary_json,
                    headless: args.headless,
                    decode: args.decode,
                    impairment: args.network.impairment(),
                    mute: args.mute,
                    network: network_config(network_scope),
                    embed: None,
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

/// The app's window: the rooms of this profile, live from the room server.
fn run_window(profile: &Profile, args: &Args) -> Result<(), Box<dyn Error>> {
    let log = std::fs::File::create(profile.log_path("app"))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(Mutex::new(log))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        channel = if BETA { "beta" } else { "dev" },
        profile = %profile.name,
        key = %profile.identity.public_key(),
        "window starting"
    );
    ui::run(
        profile,
        &ui::AppArgs {
            server: args
                .room_server
                .clone()
                .unwrap_or_else(|| default_room_server().to_owned()),
            seconds: (args.seconds > 0).then_some(args.seconds as f64),
            at: args.at,
        },
    )
}

/// The room to use, from `--room`, `--room-create` or `--room-join`.
fn room_args(args: &Args, role: Role) -> Result<Option<room::RoomArgs>, Box<dyn Error>> {
    let (room, join) = if let Some(id) = &args.room {
        if !rooms::protocol::is_id(id) {
            return Err(format!("{id:?} is not a room id").into());
        }
        (id.clone(), rooms::Join::Member)
    } else if let Some(name) = &args.room_create {
        if role != Role::Host {
            return Err("--room-create is for the host".into());
        }
        let id = rooms::protocol::new_room_id()?;
        println!("new room: {id}");
        (id, rooms::Join::Create { name: name.clone() })
    } else if let Some(text) = &args.room_join {
        let invite =
            rooms::RoomInvite::parse(text).ok_or("that is not a room invite (zizroom:...)")?;
        (invite.room, rooms::Join::Invite(invite.token))
    } else {
        if args.room_invite || args.watch.is_some() {
            return Err(
                "--room-invite and --watch need --room, --room-create or --room-join".into(),
            );
        }
        return Ok(None);
    };
    Ok(Some(room::RoomArgs {
        server: args
            .room_server
            .clone()
            .unwrap_or_else(|| LOCAL_ROOM_SERVER.to_owned()),
        room,
        join,
        invite: args.room_invite,
    }))
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
