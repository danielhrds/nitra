//! Viewer role: host's stream over WebRTC -> hardware decoder -> player window.
//!
//! Threads:
//! - network: owns the `ViewerSession`; hands complete frames to this thread
//!   through a bounded queue, asks the host for keyframes when needed;
//! - signaling watcher: notices the host's signed goodbye (or a closed
//!   connection);
//! - this thread: checks the stream, then decoder and `VideoPresenter`
//!   (`--headless`: no window; `--decode`: decode without presenting).
//!
//! With `--synthetic` hosts, frames carry their render time: every Nth
//! decoded frame is read back (a small luma region) for the latency to
//! decoded and to displayed (DXGI frame statistics), across processes.

use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use decode::media_foundation::MediaFoundationDecoder;
use decode::{DecodeSubmit, DecodedFrame, VideoDecoder};
use gpu::convert::read_nv12_luma_region;
use gpu::{D3d11Device, DebugLayer};
use identity::PublicKey;
use present::{Cadence, CadenceSummary, FrameMeta, PresenterOptions, Smoothing, VideoPresenter};
use serde::Serialize;
use signaling::Message;
use signaling::local::LocalChannel;
use signaling::signed::{self, Trust, Verifier};
use telemetry::frame_code::{self, read_frame_stamp};
use telemetry::{QpcDuration, QpcTimestamp, Summary, qpc_frequency, qpc_now};
use transport::gather::NetworkConfig;
use transport::impair::{ImpairmentConfig, ImpairmentStats};
use transport::{AUDIO_CLOCK_HZ, AudioPacket, Frame, ViewerEvent, ViewerSession};

use crate::audio_out::{self, AudioOut, AudioReport};
use crate::check::StreamCheck;
use crate::contacts::Contacts;
use crate::i18n::{Lang, lang, tr};
use crate::manual::{self, ManualArgs};
use crate::profile::Profile;
use crate::room::{self, RoomArgs, RoomLink};
use crate::secure::{RecvError, Secure};

/// How the viewer finds the host.
#[derive(Clone, Debug)]
pub enum ViewerSignaling {
    /// Local signaling on 127.0.0.1. Offers are sealed for `host_key`, and
    /// answers must be signed by it.
    Local {
        connect: SocketAddr,
        host_key: PublicKey,
    },
    /// A manual invite from the host. `host_key`, if given, is trusted
    /// without asking (otherwise a key that is not a contact is asked about).
    Manual {
        manual: ManualArgs,
        host_key: Option<PublicKey>,
    },
    /// Through a room: the offer goes to `host_key` (a participant sharing
    /// a screen there) and its signed answer comes back.
    Room { room: RoomArgs, host_key: PublicKey },
}

/// The signaling connection kept while watching (manual invites keep none).
enum Link {
    Local(LocalLink),
    Room(RoomLink),
}

#[derive(Clone, Debug)]
pub struct ViewerArgs {
    pub signaling: ViewerSignaling,
    pub position: Option<(i32, i32)>,
    /// Read frame stamps (host uses `--synthetic`) every this many frames (0: never).
    pub stamp_every: u32,
    pub seconds: u64,
    pub summary_json: Option<PathBuf>,
    /// No window: check the stream (and decode it with `decode`) only.
    pub headless: bool,
    pub decode: bool,
    /// Testing: impair what this viewer receives.
    pub impairment: Option<ImpairmentConfig>,
    /// Decode the sound but do not play it.
    pub mute: bool,
    /// Which addresses to offer (and whether to ask STUN servers).
    pub network: transport::gather::NetworkConfig,
    /// How the picture trades delay for smoothness ([`Smoothing`] as `u8`);
    /// the app changes it while watching.
    pub smoothing: Arc<std::sync::atomic::AtomicU8>,
    /// Inside the app's window instead of a window of its own.
    pub embed: Option<Embed>,
}

/// Watching inside another window (the app's): the video is a child window
/// the app positions, and what the app shows about it is shared here.
#[derive(Clone, Debug)]
pub struct Embed {
    /// The window the video goes in (an `HWND`).
    pub parent: isize,
    /// The video's child window once open (0 before): the app moves it.
    pub video_window: Arc<AtomicIsize>,
    pub stats: Arc<Mutex<LiveStats>>,
    /// Drop the sound (the app's mute button).
    pub muted: Arc<AtomicBool>,
    /// The sound's volume in percent (the app's slider; 0 to 100).
    pub volume: Arc<AtomicU8>,
    /// Show the stream's numbers over the video (the card's "⋯" button).
    pub metrics: Arc<AtomicBool>,
    /// The device to decode and present on: the app's window device, for
    /// every watched screen (each device costs ~17 MB, ~550 handles and ~18
    /// threads). Safe to share: decoding, copies and the video processor
    /// are single calls that use no pipeline state, so they cannot disturb
    /// the window's drawing (shaders on two threads would). Multithread
    /// protection must be on.
    pub device: Option<D3d11Device>,
}

/// The sound of a watched screen over a span (see [`LiveStats::audio`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioStats {
    /// From a packet's arrival to it being heard: the player's queue, the
    /// device buffer and the device's own latency (Bluetooth adds a lot).
    /// `None` while nothing plays (muted, or no output device).
    pub heard_p50_ms: Option<f32>,
    pub heard_max_ms: Option<f32>,
    /// Lost packets made up by the decoder.
    pub concealed: u64,
    /// Losses too long to make up: playback started over.
    pub gaps: u64,
    /// Late audio dropped at once to get back in step with the picture.
    pub catch_ups: u64,
    /// Device periods with no sound queued (silence filled in).
    pub underruns: u64,
}

/// What a watched screen looks like right now (refreshed every second).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LiveStats {
    /// New frames shown on screen per second (decoded, without a window).
    pub fps: f32,
    /// Frames that arrived per second.
    pub received_fps: f32,
    pub decoded_fps: f32,
    /// Frames lost before the decoder per second: its queue was full, or
    /// they came while waiting for a keyframe.
    pub dropped_per_s: f32,
    /// Decoded frames never shown per second (a newer one replaced them
    /// before the next refresh, or the presenter had no texture free).
    pub replaced_per_s: f32,
    /// How new frames were spaced on screen lately.
    pub cadence: Option<CadenceSummary>,
    /// The sound, once packets come.
    pub audio: Option<AudioStats>,
    /// How long frames are held to even them out (see [`Smoothing`]), in ms.
    pub hold_ms: Option<f32>,
    /// Picture size of the latest decoded frame.
    pub size: Option<(u32, u32)>,
    /// Media received, in megabits per second.
    pub mbps: f32,
    /// Share of packets lost on the way (0..1), before retransmissions.
    pub loss: Option<f32>,
    pub rtt: Option<Duration>,
    /// Which way the media comes (LAN, internet, IPv6), once it flows.
    pub route: Option<transport::Route>,
}

impl LiveStats {
    /// The numbers shown over the video (the card's "⋯" button), in the
    /// app's language.
    pub fn metrics_lines(&self) -> Vec<String> {
        // Decimal commas in Portuguese.
        let pt = lang() == Lang::Pt;
        let comma = |text: String| if pt { text.replace('.', ",") } else { text };
        let mut lines = vec![
            comma(format!(
                "{} {:.1} fps · {} {:.1} · {} {:.1}",
                tr("recebidos", "received"),
                self.received_fps,
                tr("decodificados", "decoded"),
                self.decoded_fps,
                tr("na tela", "shown"),
                self.fps
            )),
            comma(format!(
                "{} {:.1}/s · {} {:.1}/s{}",
                tr("descartados", "dropped"),
                self.dropped_per_s,
                tr("substituídos", "replaced"),
                self.replaced_per_s,
                self.hold_ms.map_or_else(String::new, |ms| format!(
                    " · {} {ms:.0} ms",
                    tr("fila", "held")
                ))
            )),
        ];
        if let Some(c) = &self.cadence {
            lines.push(comma(format!(
                "{}: p50 {:.1} · p95 {:.1} · {} {:.1} ms · {} {:.1}%",
                tr("entre quadros", "between frames"),
                c.p50_ms,
                c.p95_ms,
                tr("máx", "max"),
                c.max_ms,
                tr("longos", "long"),
                c.long_share * 100.0
            )));
        }
        if let Some(a) = &self.audio {
            let heard = a.heard_p50_ms.map_or_else(
                || tr("sem saída", "no output").to_owned(),
                |ms| match lang() {
                    Lang::Pt => format!("ouvido {ms:.0} ms após chegar"),
                    Lang::En => format!("heard {ms:.0} ms after arriving"),
                },
            );
            lines.push(format!(
                "{}: {heard} · {} {} · {} {}",
                tr("áudio", "audio"),
                tr("ajustes", "catch-ups"),
                a.catch_ups,
                tr("falhas", "gaps"),
                a.concealed + a.gaps + a.underruns
            ));
        }
        let mut link = Vec::new();
        if let Some((w, h)) = self.size {
            link.push(format!("{w}x{h}"));
        }
        link.push(comma(format!("{:.1} Mbps", self.mbps)));
        if let Some(loss) = self.loss {
            link.push(comma(format!(
                "{} {:.1}%",
                tr("perda", "loss"),
                loss * 100.0
            )));
        }
        if let Some(rtt) = self.rtt {
            link.push(format!("{} ms", rtt.as_millis()));
        }
        lines.push(link.join(" · "));
        lines
    }

    /// One line for the log: every stage's rate, then the spacing.
    fn log_line(&self) -> String {
        let mut line = format!(
            "received {:.1} fps, decoded {:.1}, shown {:.1}, dropped {:.1}/s, replaced {:.1}/s",
            self.received_fps, self.decoded_fps, self.fps, self.dropped_per_s, self.replaced_per_s
        );
        if let Some(hold) = self.hold_ms {
            line += &format!(", hold {hold:.0} ms");
        }
        if let Some(c) = &self.cadence {
            line += &format!(
                "; on screen p50 {:.1} ms p95 {:.1} ms max {:.1} ms, long {:.1}%",
                c.p50_ms,
                c.p95_ms,
                c.max_ms,
                c.long_share * 100.0
            );
        }
        if let Some(a) = &self.audio {
            line += "; audio";
            if let (Some(p50), Some(max)) = (a.heard_p50_ms, a.heard_max_ms) {
                line += &format!(" heard after p50 {p50:.1} ms max {max:.1} ms,");
            }
            line += &format!(
                " catch-ups {}, concealed {}, gaps {}, underruns {}",
                a.catch_ups, a.concealed, a.gaps, a.underruns
            );
        }
        if let Some((w, h)) = self.size {
            line += &format!("; {w}x{h}");
        }
        line += &format!(", {:.1} Mbps", self.mbps);
        if let Some(loss) = self.loss {
            line += &format!(", loss {:.1}%", loss * 100.0);
        }
        if let Some(rtt) = self.rtt {
            line += &format!(", rtt {} ms", rtt.as_millis());
        }
        line
    }
}

/// Audio held ahead of the speakers (queue + device buffer).
const AUDIO_TARGET: Duration = Duration::from_millis(60);

/// Frames waiting for the decoder (~250 ms at 60 fps): absorbs a burst
/// (a paced keyframe, then the frames queued behind it) without a drop.
const FRAME_QUEUE: usize = 16;
/// Spacing of repeated keyframe requests.
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);
const MAX_STAMPS: usize = 1 << 15;
/// How long the direct connection may take to open once the offer and
/// answer are exchanged (it takes about a second when it works; there is
/// no relay to fall back on). Manual invites have no limit:
/// a person pastes the answer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The media connection never opened: no direct path between the two PCs.
#[derive(Debug)]
pub struct NoDirectPath {
    /// Whether this PC learned a public address (STUN answered, or a public
    /// IPv6 one): if not, its own network blocks UDP to the internet.
    pub had_public_address: bool,
}

impl std::fmt::Display for NoDirectPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.had_public_address {
            f.write_str("no direct connection to the host (strict NAT on both sides, or a firewall blocking UDP)")
        } else {
            f.write_str("no direct connection: this PC got no public address (UDP to the internet blocked?)")
        }
    }
}

impl Error for NoDirectPath {}

/// The H.264 decoder did not start on the GPU.
#[derive(Debug)]
pub struct NoDecoder(pub String);

impl std::fmt::Display for NoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the H.264 decoder did not start: {}", self.0)
    }
}

impl Error for NoDecoder {}

#[derive(Debug, Default, Serialize)]
struct ViewerSummary {
    profile: String,
    seconds: f64,
    frames_received: u64,
    frames_dropped_queue: u64,
    frames_skipped_waiting_keyframe: u64,
    keyframe_requests: u64,
    /// "window", "headless" or "headless+decode".
    mode: String,
    stream: StreamCheck,
    impairment: Option<Impaired>,
    /// Longest time between two frames handed to the decoder (a freeze).
    longest_gap_ms: f64,
    audio: AudioSummary,
    decoded: u64,
    presented: u64,
    stamps_read: u64,
    to_decoded_ms: Option<LatencyMs>,
    /// Last packet of a frame received to the decoder thread taking it.
    queue_wait_ms: Option<LatencyMs>,
    decode_ms: Option<LatencyMs>,
    to_displayed_ms: Option<LatencyMs>,
    /// Which way the media came (LAN, internet, IPv6); no addresses.
    route: Option<String>,
    end: String,
}

/// A device for decoding and presenting video, on the first hardware adapter.
pub fn video_device() -> Result<D3d11Device, Box<dyn Error>> {
    let adapters = gpu::enumerate_adapters()?;
    let adapter = crate::hardware::main_adapter(&adapters).ok_or("no hardware adapter")?;
    let debug_layer = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };
    let device = gpu::create_device(adapter, debug_layer)?;
    // Several threads use it (decoder, presenter; in the app, every
    // watched screen).
    device.enable_multithread_protection()?;
    Ok(device)
}

pub fn run(profile: &Profile, args: &ViewerArgs, stop: &AtomicBool) -> Result<(), Box<dyn Error>> {
    // Signaling and connection setup.
    let (mut session, mut link, host_label) = match &args.signaling {
        ViewerSignaling::Local { connect, host_key } => {
            let (session, link) = connect_local(profile, &args.network, *connect, *host_key)?;
            (session, Some(Link::Local(link)), connect.to_string())
        }
        ViewerSignaling::Manual { manual, host_key } => {
            let (session, name) = join(profile, &args.network, manual, *host_key)?;
            (session, None, name)
        }
        ViewerSignaling::Room { room, host_key } => {
            let (session, link) = room::watch(profile, &args.network, room, *host_key)?;
            (
                session,
                Some(Link::Room(link)),
                format!("room {}", room.room),
            )
        }
    };
    // Kinds only: a public address identifies the user's connection.
    tracing::info!(candidates = %session.gathered().summary(), "gathered");
    session.set_impairment(args.impairment.clone());

    // Headless without decoding needs no GPU at all.
    let device = if args.headless && !args.decode {
        None
    } else if let Some(shared) = args.embed.as_ref().and_then(|e| e.device.clone()) {
        Some(shared)
    } else {
        Some(video_device()?)
    };
    let mut decoder = device
        .as_ref()
        .map(MediaFoundationDecoder::new)
        .transpose()
        .map_err(|error| NoDecoder(error.to_string()))?;
    let mut presenter = match (&device, args.headless) {
        (Some(device), false) => Some(VideoPresenter::open(
            device,
            PresenterOptions {
                title: format!("nitra - watching ({})", profile.name),
                width: 1280,
                height: 720,
                borderless: args.embed.is_some(),
                overlay: false,
                parent: args.embed.as_ref().map(|e| e.parent),
                position: args.position,
                background: args.position.is_some(),
                skip_when_hidden: true,
            },
        )?),
        _ => None,
    };
    if let Some(presenter) = &presenter {
        presenter.set_smoothing(Smoothing::from_u8(args.smoothing.load(Ordering::Relaxed)));
    }
    if let (Some(embed), Some(presenter)) = (&args.embed, &presenter) {
        embed
            .video_window
            .store(presenter.hwnd().0 as isize, Ordering::Relaxed);
    }
    let mode = match (args.headless, decoder.is_some()) {
        (false, _) => "window",
        (true, true) => "headless+decode",
        (true, false) => "headless",
    };

    let host_gone = Arc::new(AtomicBool::new(false));
    let network_stop = Arc::new(AtomicBool::new(false));
    let keyframe_wanted = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(NetworkCounters {
        muted: args
            .embed
            .as_ref()
            .map_or_else(Arc::default, |e| Arc::clone(&e.muted)),
        ..NetworkCounters::default()
    });
    let waker = session.waker();
    // A tunnel's IPv6 address (Teredo, VPNs) does not count: it says
    // nothing about this network letting UDP out.
    let had_public_address = session.gathered().candidates.iter().any(|c| {
        c.reflexive
            || (c.interface == transport::interfaces::InterfaceKind::Physical
                && transport::Route::of(c.addr) == transport::Route::Ipv6)
    });
    let (frame_tx, frame_rx) = mpsc::sync_channel::<Frame>(FRAME_QUEUE);
    // Sound: played unless headless or muted; without an output device it
    // is still decoded (and measured).
    let play = !args.headless && !args.mute;
    let volume = args
        .embed
        .as_ref()
        .map_or_else(|| Arc::new(AtomicU8::new(100)), |e| Arc::clone(&e.volume));
    let audio_out = match AudioOut::start(play, AUDIO_TARGET, Arc::clone(&volume)) {
        Ok(out) => out,
        Err(error) if play => {
            tracing::warn!(%error, "no audio playback; decoding only");
            AudioOut::start(false, AUDIO_TARGET, volume)?
        }
        Err(error) => return Err(error.into()),
    };
    let audio_live = audio_out.live();
    let network = {
        let (stop, wanted, counters, gone, audio) = (
            Arc::clone(&network_stop),
            Arc::clone(&keyframe_wanted),
            Arc::clone(&counters),
            Arc::clone(&host_gone),
            audio_out.sender(),
        );
        std::thread::Builder::new()
            .name("network".into())
            .spawn(move || network(session, &frame_tx, &audio, &stop, &wanted, &counters, &gone))?
    };
    // Manual sessions have no signaling channel: the media connection
    // ending (`Disconnected`) is how the host's leaving shows.
    let watcher = match &mut link {
        None => None,
        Some(Link::Room(link)) => {
            let waker = waker.clone();
            Some(link.watch_host(Arc::clone(&host_gone), move || waker.wake())?)
        }
        Some(Link::Local(link)) => {
            let (mut watch, gone, waker, secure) = (
                link.channel.try_clone()?,
                Arc::clone(&host_gone),
                waker.clone(),
                link.secure.clone(),
            );
            Some(
                std::thread::Builder::new()
                    .name("signaling-watch".into())
                    .spawn(move || {
                        // A goodbye, a closed connection or a message that fails
                        // verification: the signaling channel is over either way.
                        match secure.recv(&mut watch) {
                            Ok(opened) => {
                                tracing::info!(message = ?opened.message, "signaling ended")
                            }
                            Err(error) => tracing::info!(%error, "signaling ended"),
                        }
                        gone.store(true, Ordering::Relaxed);
                        waker.wake();
                    })?,
            )
        }
    };
    println!(
        "viewer '{}' ({mode}): connected to {} — close the window or Ctrl+C to stop",
        profile.name, host_label
    );

    let started = Instant::now();
    let connect_by = link.is_some().then(|| started + CONNECT_TIMEOUT);
    // Which screen a log line is about (several can be watched at once):
    // the start of the host's public key.
    let host_key = match &args.signaling {
        ViewerSignaling::Local { host_key, .. } | ViewerSignaling::Room { host_key, .. } => {
            Some(host_key)
        }
        ViewerSignaling::Manual { host_key, .. } => host_key.as_ref(),
    };
    let host_tag: String = host_key.map_or_else(
        || "host".to_owned(),
        |key| key.to_string().chars().take(8).collect(),
    );
    let mut live = LiveMeter::new();
    let mut metrics_shown = false;
    let mut smoothing = None;
    // Test hook: the command-line viewer shows the card's numbers over its
    // window (the app shows them with the card's "⋯" button).
    let test_metrics = std::env::var_os("NITRA_TEST_METRICS").is_some();
    let mut stream = StreamCheck::default();
    let mut last_frame: Option<Instant> = None;
    let mut longest_gap = Duration::ZERO;
    let mut state = Decoding {
        device: device.as_ref(),
        stamp_every: args.stamp_every,
        ..Default::default()
    };
    let end = loop {
        if stop.load(Ordering::Relaxed) {
            break "stopped";
        }
        if presenter.as_ref().is_some_and(VideoPresenter::is_closed) {
            break "window closed";
        }
        if host_gone.load(Ordering::Relaxed) {
            break "host ended the session";
        }
        if connect_by.is_some_and(|t| Instant::now() >= t)
            && !counters.connected.load(Ordering::Relaxed)
        {
            break NO_PATH;
        }
        if args.seconds > 0 && started.elapsed() >= Duration::from_secs(args.seconds) {
            break "time up";
        }
        let link_stats = *counters.link.lock().unwrap();
        if let Some((mut stats, mut log)) = live.tick(
            || {
                let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
                let presented = presenter.as_ref().map(VideoPresenter::counters);
                let mut sound = audio_live.lock().unwrap();
                let counts = Counts {
                    received: load(&counters.received),
                    dropped: load(&counters.dropped) + load(&counters.skipped),
                    decoded: state.decoded,
                    shown: presented.map_or(state.decoded, |p| p.presented),
                    replaced: presented.map_or(0, |p| p.replaced + p.pool_dropped),
                    media_bytes: 0,
                    audio_concealed: sound.concealed,
                    audio_gaps: sound.gaps,
                    audio_catch_ups: sound.catch_ups,
                    audio_underruns: sound.underruns,
                    audio_packets: sound.packets,
                };
                let heard = sound.take_delays();
                drop(sound);
                let cadence = presenter
                    .as_ref()
                    .map(VideoPresenter::take_cadence)
                    .unwrap_or_default();
                (counts, cadence, heard)
            },
            link_stats,
            state.size,
        ) {
            let hold = presenter.as_ref().map(|p| p.hold().as_secs_f32() * 1000.0);
            stats.hold_ms = hold;
            if let Some(log) = log.as_mut() {
                log.hold_ms = hold;
            }
            if let Some(embed) = &args.embed {
                *embed.stats.lock().unwrap() = stats;
            }
            let mode = Smoothing::from_u8(args.smoothing.load(Ordering::Relaxed));
            if let Some(presenter) = &presenter
                && smoothing != Some(mode)
            {
                presenter.set_smoothing(mode);
                smoothing = Some(mode);
            }
            let wanted = args
                .embed
                .as_ref()
                .map_or(test_metrics, |e| e.metrics.load(Ordering::Relaxed));
            if let Some(presenter) = &presenter
                && (wanted || metrics_shown)
            {
                presenter.set_overlay(wanted.then(|| stats.metrics_lines()));
                metrics_shown = wanted;
            }
            // Only while frames come: a paused or hidden screen logs nothing.
            if let Some(log) = log.filter(|log| log.received_fps > 0.0) {
                tracing::info!("watching {host_tag}: {}", log.log_line());
            }
        }
        match next_frame(&frame_rx) {
            Ok(Some(frame)) => {
                state.queue_waits.push(frame.received_at.elapsed());
                stream.frame(&frame.data, frame.rtp_time);
                if let Some(last) = last_frame {
                    longest_gap =
                        longest_gap.max(frame.received_at.saturating_duration_since(last));
                }
                last_frame = Some(frame.received_at);
                let Some(decoder) = decoder.as_mut() else {
                    continue;
                };
                if let Err(error) = state.decode(decoder, &frame, presenter.as_mut()) {
                    tracing::warn!(%error, "decode failed; asking for a keyframe");
                    keyframe_wanted.store(true, Ordering::Relaxed);
                    waker.wake();
                }
            }
            Ok(None) => {}
            Err(()) => break "connection lost",
        }
    };

    // Shutdown: say goodbye, stop the network, close the window.
    match link {
        Some(Link::Local(mut link)) => {
            if !host_gone.load(Ordering::Relaxed) {
                let bye = Message::Bye { reason: end.into() };
                let _ = link
                    .secure
                    .send(&mut link.channel, &link.host_key, bye, None);
            }
            link.channel.shutdown();
        }
        Some(Link::Room(link)) => link.finish(end, !host_gone.load(Ordering::Relaxed)),
        None => {}
    }
    network_stop.store(true, Ordering::Relaxed);
    waker.wake();
    let _ = network.join();
    if let Some(watcher) = watcher {
        let _ = watcher.join();
    }
    let audio_report = audio_out.finish(counters.audio_dropped.load(Ordering::Relaxed));
    let report = presenter.map(VideoPresenter::close);
    if let Some(report) = &report {
        tracing::info!(
            display = ?report.display.counters,
            composition = ?report.composition,
            "presenter at the end"
        );
    }

    let to_displayed: Vec<QpcDuration> = report
        .iter()
        .flat_map(|r| &r.display.frames)
        .filter_map(|f| {
            state
                .stamps
                .get(&f.frame_id)
                .map(|stamp| f.displayed_at.duration_since(*stamp))
        })
        .collect();
    let impairment = *counters.impairment.lock().unwrap();
    let summary = ViewerSummary {
        profile: profile.name.clone(),
        seconds: started.elapsed().as_secs_f64(),
        frames_received: counters.received.load(Ordering::Relaxed),
        frames_dropped_queue: counters.dropped.load(Ordering::Relaxed),
        frames_skipped_waiting_keyframe: counters.skipped.load(Ordering::Relaxed),
        keyframe_requests: counters.keyframe_requests.load(Ordering::Relaxed),
        mode: mode.to_owned(),
        stream,
        impairment: impairment.map(Impaired::from),
        longest_gap_ms: longest_gap.as_secs_f64() * 1000.0,
        audio: AudioSummary::new(
            &audio_report,
            play,
            &state.origins,
            if to_displayed.is_empty() {
                ("decoded", &state.to_decoded)
            } else {
                ("displayed", &to_displayed)
            },
        ),
        decoded: state.decoded,
        presented: report.as_ref().map_or(0, |r| r.counters.presented),
        stamps_read: state.to_decoded.len() as u64,
        to_decoded_ms: summary_ms(&state.to_decoded),
        queue_wait_ms: summary_durations(&state.queue_waits),
        decode_ms: summary_ms(&state.decode_times),
        to_displayed_ms: summary_ms(&to_displayed),
        route: counters
            .link
            .lock()
            .unwrap()
            .route
            .map(|route| format!("{route:?}")),
        end: end.to_owned(),
    };
    if summary.stream.problems() > 0 {
        tracing::warn!(stream = ?summary.stream, "the stream had problems");
    }
    // How smooth it was, in the log (the window app has no console for the
    // JSON below); no addresses.
    tracing::info!(
        seconds = summary.seconds,
        received = summary.frames_received,
        decoded = summary.decoded,
        presented = summary.presented,
        dropped_queue = summary.frames_dropped_queue,
        skipped = summary.frames_skipped_waiting_keyframe,
        keyframe_requests = summary.keyframe_requests,
        longest_gap_ms = summary.longest_gap_ms,
        decode_p95_ms = summary.decode_ms.as_ref().map(|d| d.p95),
        decode_max_ms = summary.decode_ms.as_ref().map(|d| d.max),
        end = summary.end,
        "watching summary"
    );
    let json = serde_json::to_string_pretty(&summary)?;
    println!("{json}");
    if let Some(path) = &args.summary_json {
        std::fs::write(path, &json)?;
    }
    if let Some(device) = device.as_ref().filter(|d| d.debug_layer_enabled()) {
        let problems = device
            .take_debug_messages()
            .into_iter()
            .filter(|m| m.severity.is_problem())
            .count();
        println!("debug layer: {problems} warning/error");
    }
    if end == NO_PATH {
        return Err(NoDirectPath { had_public_address }.into());
    }
    match report.and_then(|r| r.error) {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

/// How a session that never connected ends.
const NO_PATH: &str = "no direct connection";

#[derive(Debug, Default, Serialize)]
struct AudioSummary {
    /// Played on the default output (else decoded only).
    played: bool,
    packets: u64,
    dropped_queue: u64,
    late: u64,
    concealed: u64,
    gaps: u64,
    decode_errors: u64,
    underruns: u64,
    /// Late audio dropped to get back to the target (after a stall).
    catch_ups: u64,
    caught_up_ms: f64,
    playback_level_ms: Option<(f64, f64)>,
    output_drift_ppm: Option<f64>,
    /// Sound made on the host (its media time on the video's clock) to
    /// heard here (or decoded, if not played). Needs a `--synthetic` host:
    /// the video stamps give the clock's origin.
    heard_after_sound_ms: Option<LatencyMs>,
    /// The video latency it is compared with ("displayed" or "decoded").
    video_reference: String,
    /// Positive: sound comes after the picture (p50 audio minus p50 video).
    av_offset_ms: Option<f64>,
}

impl AudioSummary {
    fn new(
        report: &AudioReport,
        played: bool,
        origins: &[i64],
        (reference, video): (&str, &[QpcDuration]),
    ) -> Self {
        let mut sorted = origins.to_vec();
        sorted.sort_unstable();
        let origin = sorted.get(sorted.len() / 2).copied();
        let frequency = qpc_frequency().ticks_per_second() as i128;
        let heard: Vec<QpcDuration> = origin.map_or_else(Vec::new, |origin| {
            report
                .heard
                .iter()
                .map(|(rtp, at)| {
                    let sound = origin
                        + (*rtp as i128 * frequency / i128::from(AUDIO_CLOCK_HZ as u32)) as i64;
                    at.duration_since(QpcTimestamp::from_ticks(sound))
                })
                .collect()
        });
        let heard_after_sound_ms = summary_ms(&heard);
        let av_offset_ms = heard_after_sound_ms
            .as_ref()
            .zip(summary_ms(video))
            .map(|(a, v)| a.p50 - v.p50);
        let player = report.player.as_ref();
        Self {
            played: played && player.is_some(),
            packets: report.packets,
            dropped_queue: report.dropped_queue,
            late: report.late,
            concealed: report.concealed,
            gaps: report.gaps,
            decode_errors: report.decode_errors,
            underruns: player.map_or(0, |p| p.underruns),
            catch_ups: player.map_or(0, |p| p.catch_ups),
            caught_up_ms: player.map_or(0.0, |p| p.caught_up_ms),
            playback_level_ms: player
                .filter(|p| p.level_max_ms > 0.0)
                .map(|p| (p.level_min_ms, p.level_max_ms)),
            output_drift_ppm: player.and_then(|p| p.device_drift_ppm),
            heard_after_sound_ms,
            video_reference: reference.to_owned(),
            av_offset_ms,
        }
    }
}

/// What the impairment did, for the summary.
#[derive(Clone, Copy, Debug, Serialize)]
struct Impaired {
    received: u64,
    delivered: u64,
    dropped_loss: u64,
    dropped_burst: u64,
    dropped_queue: u64,
    dropped_outage: u64,
    reordered: u64,
}

impl From<ImpairmentStats> for Impaired {
    fn from(s: ImpairmentStats) -> Self {
        Self {
            received: s.received,
            delivered: s.delivered,
            dropped_loss: s.dropped_loss,
            dropped_burst: s.dropped_burst,
            dropped_queue: s.dropped_queue,
            dropped_outage: s.dropped_outage,
            reordered: s.reordered,
        }
    }
}

/// `Err` when the network thread is gone.
fn next_frame(frames: &Receiver<Frame>) -> Result<Option<Frame>, ()> {
    match frames.recv_timeout(Duration::from_millis(50)) {
        Ok(frame) => Ok(Some(frame)),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(RecvTimeoutError::Disconnected) => Err(()),
    }
}

#[derive(Default)]
struct NetworkCounters {
    received: AtomicU64,
    dropped: AtomicU64,
    skipped: AtomicU64,
    keyframe_requests: AtomicU64,
    audio_dropped: AtomicU64,
    impairment: Mutex<Option<ImpairmentStats>>,
    /// The connection's latest statistics.
    link: Mutex<transport::LinkStats>,
    /// Sound dropped on arrival (muted in the app).
    muted: Arc<AtomicBool>,
    /// The media connection opened (ICE and DTLS done).
    connected: AtomicBool,
}

/// Running totals the meter turns into rates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Counts {
    received: u64,
    /// Lost before the decoder (queue full, waiting for a keyframe).
    dropped: u64,
    decoded: u64,
    /// New frames presented (decoded, without a window).
    shown: u64,
    /// Decoded but never presented.
    replaced: u64,
    media_bytes: u64,
    /// Audio: lost packets made up by the decoder, losses too long for
    /// that, late audio dropped to catch up, device periods without sound.
    audio_concealed: u64,
    audio_gaps: u64,
    audio_catch_ups: u64,
    audio_underruns: u64,
    audio_packets: u64,
}

/// Turns counters into per-second rates, once a second, each over the last
/// two seconds: frames come unevenly, so a one-second window read 59 or 61
/// for a steady 60 fps (frames landing just before or after its edge).
/// Every [`LOG_EVERY`] it also gives a line for the log over that span.
struct LiveMeter {
    /// (when, totals): the last three samples.
    samples: std::collections::VecDeque<(Instant, Counts)>,
    /// Spacing on screen, one entry per second (the last [`LOG_EVERY`]).
    cadence: std::collections::VecDeque<Cadence>,
    /// Audio arrival-to-heard delays (µs), one entry per second, like `cadence`.
    heard: std::collections::VecDeque<Vec<u32>>,
    /// Start of the span the next log line covers.
    logged: (Instant, Counts),
}

/// How far back the rates look.
const LIVE_WINDOW: Duration = Duration::from_secs(2);
/// Seconds of spacing the live numbers describe (more than the rates:
/// a second holds too few frames for a 95th percentile).
const LIVE_CADENCE_SECONDS: usize = 3;
/// How often the rates go to the log (only while watching): ~300 bytes per
/// line, about 110 KB an hour per watched screen.
const LOG_EVERY: Duration = Duration::from_secs(10);

impl LiveMeter {
    fn new() -> Self {
        let start = (Instant::now(), Counts::default());
        Self {
            samples: std::collections::VecDeque::from([start]),
            cadence: std::collections::VecDeque::new(),
            heard: std::collections::VecDeque::new(),
            logged: start,
        }
    }

    /// Once a second: the rates over the last two seconds (one at the
    /// start) and, every [`LOG_EVERY`], the same over that span for the
    /// log. `read` gives the totals and the spacing since its last call.
    fn tick(
        &mut self,
        read: impl FnOnce() -> (Counts, Cadence, Vec<u32>),
        link: transport::LinkStats,
        size: Option<(u32, u32)>,
    ) -> Option<(LiveStats, Option<LiveStats>)> {
        let now = Instant::now();
        let &(last, _) = self.samples.back().expect("never empty");
        if now.duration_since(last) < Duration::from_secs(1) {
            return None;
        }
        let (mut counts, cadence, heard) = read();
        counts.media_bytes = link.media_bytes;
        self.samples.push_back((now, counts));
        // Keep the oldest sample within the window (and the newest).
        while self.samples.len() > 2
            && now.duration_since(self.samples[1].0) >= LIVE_WINDOW - Duration::from_millis(100)
        {
            self.samples.pop_front();
        }
        self.cadence.push_back(cadence);
        self.heard.push_back(heard);
        while self.heard.len() > LOG_EVERY.as_secs() as usize {
            self.heard.pop_front();
        }
        while self.cadence.len() > LOG_EVERY.as_secs() as usize {
            self.cadence.pop_front();
        }
        let live = self.stats(
            *self.samples.front().expect("never empty"),
            (now, counts),
            LIVE_CADENCE_SECONDS,
            link,
            size,
        );
        let log = (now.duration_since(self.logged.0) >= LOG_EVERY - Duration::from_millis(100))
            .then(|| {
                let stats = self.stats(self.logged, (now, counts), self.cadence.len(), link, size);
                self.logged = (now, counts);
                stats
            });
        Some((live, log))
    }

    /// Rates between two samples, with the spacing of the last `seconds`.
    fn stats(
        &self,
        (since, then): (Instant, Counts),
        (now, counts): (Instant, Counts),
        seconds: usize,
        link: transport::LinkStats,
        size: Option<(u32, u32)>,
    ) -> LiveStats {
        let span = now.duration_since(since).as_secs_f32().max(f32::EPSILON);
        let rate = |a: u64, b: u64| a.saturating_sub(b) as f32 / span;
        let mut cadence = Cadence::default();
        for window in self.cadence.iter().rev().take(seconds) {
            cadence.intervals_us.extend_from_slice(&window.intervals_us);
        }
        let mut heard: Vec<u32> = self
            .heard
            .iter()
            .rev()
            .take(seconds)
            .flatten()
            .copied()
            .collect();
        heard.sort_unstable();
        let audio = (counts.audio_packets > then.audio_packets).then(|| AudioStats {
            heard_p50_ms: heard.get(heard.len() / 2).map(|&us| us as f32 / 1000.0),
            heard_max_ms: heard.last().map(|&us| us as f32 / 1000.0),
            concealed: counts.audio_concealed.saturating_sub(then.audio_concealed),
            gaps: counts.audio_gaps.saturating_sub(then.audio_gaps),
            catch_ups: counts.audio_catch_ups.saturating_sub(then.audio_catch_ups),
            underruns: counts.audio_underruns.saturating_sub(then.audio_underruns),
        });
        LiveStats {
            fps: rate(counts.shown, then.shown),
            received_fps: rate(counts.received, then.received),
            decoded_fps: rate(counts.decoded, then.decoded),
            dropped_per_s: rate(counts.dropped, then.dropped),
            replaced_per_s: rate(counts.replaced, then.replaced),
            cadence: cadence.summary(),
            audio,
            hold_ms: None,
            size,
            mbps: rate(counts.media_bytes, then.media_bytes) * 8.0 / 1_000_000.0,
            loss: link.loss,
            rtt: link.rtt,
            route: link.route,
        }
    }
}

/// The signaling channel of a local session, kept to say goodbye and to
/// notice the host's.
struct LocalLink {
    channel: LocalChannel,
    secure: Secure,
    host_key: PublicKey,
}

/// Local signaling: our offer sealed for `host_key`, its signed answer back.
fn connect_local(
    profile: &Profile,
    network: &NetworkConfig,
    connect: SocketAddr,
    host_key: PublicKey,
) -> Result<(ViewerSession, LocalLink), Box<dyn Error>> {
    let mut channel = LocalChannel::connect(connect)?;
    let (mut session, offer) = ViewerSession::offer_network(network)?;
    let secure = Secure::new(Arc::clone(&profile.identity), Trust::Keys(vec![host_key]));
    let offer_nonce = secure.send(
        &mut channel,
        &host_key,
        Message::Offer {
            sdp: offer,
            profile: profile.name.clone(),
        },
        None,
    )?;
    // A host that cannot verify the offer (e.g. --host-key names someone
    // else) closes without answering: it cannot sign a reason for us.
    let reply = secure.recv(&mut channel).map_err(|error| match error {
        RecvError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            "the host closed the connection without answering (is --host-key right?)".to_owned()
        }
        other => other.to_string(),
    })?;
    match reply.message {
        Message::Answer { sdp, .. } if reply.reply_to.as_deref() == Some(offer_nonce.as_str()) => {
            session.accept_answer(&sdp)?;
        }
        Message::Answer { .. } => return Err("the answer is for another offer".into()),
        Message::Bye { reason } => return Err(format!("host refused: {reason}").into()),
        Message::Offer { .. } => return Err("unexpected offer from the host".into()),
    }
    channel.wait_forever()?;
    Ok((
        session,
        LocalLink {
            channel,
            secure,
            host_key,
        },
    ))
}

/// Manual invite: takes the host's invite, checks it (signature, expiry,
/// and a key we trust or the user accepts), and hands over our answer.
/// Returns the session and the host's name.
fn join(
    profile: &Profile,
    network: &NetworkConfig,
    manual: &ManualArgs,
    host_key: Option<PublicKey>,
) -> Result<(ViewerSession, String), Box<dyn Error>> {
    let envelope = manual::take(manual.invite_file.as_deref(), "invite")?;
    // Anyone may have sent it; who it is from is decided below.
    let mut verifier = Verifier::new(*profile.identity.public_key(), Trust::Anyone);
    let opened = verifier.open(&envelope, signed::now())?;
    let Message::Offer {
        sdp,
        profile: host_name,
    } = opened.message
    else {
        return Err("this text is not an invite (it is an answer or a goodbye)".into());
    };
    let host_name = manual::clean_name(&host_name);
    if let Some(expected) = host_key
        && expected != opened.from
    {
        return Err(format!(
            "the invite is from key {}, not the --host-key given",
            opened.from
        )
        .into());
    }
    let mut contacts = Contacts::load(&profile.dir.join("contacts.json"))?;
    let preset: Vec<PublicKey> = host_key.into_iter().collect();
    if !manual::trust(&mut contacts, &preset, &opened.from, &host_name)? {
        return Err("invite declined".into());
    }
    let (session, answer) = ViewerSession::answer_network(network, &sdp)?;
    let (reply, _) = signed::seal_manual(
        &profile.identity,
        Some(&opened.from),
        Message::Answer {
            sdp: answer,
            profile: profile.name.clone(),
        },
        Some(&opened.nonce),
        signed::now(),
    )?;
    manual::hand_over(&reply, manual.answer_file.as_deref(), "answer")?;
    println!("waiting for '{host_name}' to paste the answer...");
    Ok((session, host_name))
}

fn network(
    mut session: ViewerSession,
    frames: &SyncSender<Frame>,
    audio: &SyncSender<AudioPacket>,
    stop: &AtomicBool,
    keyframe_wanted: &AtomicBool,
    counters: &NetworkCounters,
    host_gone: &AtomicBool,
) {
    // The decoder can only start (or restart after a loss) at a keyframe.
    let mut need_keyframe = true;
    let mut last_request: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) && session.is_alive() {
        if let Err(error) = session.poll(Instant::now() + transport::net::MAX_WAIT) {
            tracing::warn!(%error, "network failed");
            break;
        }
        if keyframe_wanted.swap(false, Ordering::Relaxed) {
            need_keyframe = true;
        }
        while let Some(event) = session.next_event() {
            match event {
                ViewerEvent::Connected => {
                    tracing::info!("media connected");
                    counters.connected.store(true, Ordering::Relaxed);
                }
                ViewerEvent::Audio(_) if counters.muted.load(Ordering::Relaxed) => {}
                ViewerEvent::Audio(packet) => {
                    if !audio_out::offer(audio, packet) {
                        counters.audio_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                ViewerEvent::Disconnected => {
                    tracing::info!("media disconnected");
                    host_gone.store(true, Ordering::Relaxed);
                }
                ViewerEvent::Frame(frame) => {
                    counters.received.fetch_add(1, Ordering::Relaxed);
                    if !frame.contiguous {
                        need_keyframe = true;
                    }
                    let keyframe = encode::annexb::is_keyframe(&frame.data);
                    if need_keyframe && !keyframe {
                        counters.skipped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    match frames.try_send(frame) {
                        Ok(()) => need_keyframe = false,
                        Err(TrySendError::Full(_)) => {
                            counters.dropped.fetch_add(1, Ordering::Relaxed);
                            need_keyframe = true;
                        }
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
            }
        }
        *counters.link.lock().unwrap() = session.link_stats();
        if need_keyframe
            && last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY)
            && session.request_keyframe()
        {
            counters.keyframe_requests.fetch_add(1, Ordering::Relaxed);
            last_request = Some(Instant::now());
        }
    }
    *counters.impairment.lock().unwrap() = session.impairment_stats();
    session.close();
}

#[derive(Default)]
struct Decoding<'a> {
    device: Option<&'a D3d11Device>,
    stamp_every: u32,
    submitted: u64,
    decoded: u64,
    to_decoded: Vec<QpcDuration>,
    /// Decoder: submit to hand-off.
    decode_times: Vec<QpcDuration>,
    /// Network hand-off to the decoder thread picking the frame up.
    queue_waits: Vec<std::time::Duration>,
    /// Render time of the sampled frames, by decoder frame id (bounded).
    stamps: HashMap<u64, QpcTimestamp>,
    /// Media time (90 kHz) of the frames in the decoder, by frame id.
    media_times: HashMap<u64, u64>,
    /// The host's media-time origin as QPC ticks, one estimate per stamp
    /// read (stamp minus media time): lines audio up with the video's clock.
    origins: Vec<i64>,
    /// Picture size of the latest decoded frame.
    size: Option<(u32, u32)>,
}

impl<'a> Decoding<'a> {
    fn decode(
        &mut self,
        decoder: &mut MediaFoundationDecoder,
        frame: &Frame,
        mut presenter: Option<&mut VideoPresenter>,
    ) -> Result<(), Box<dyn Error>> {
        let id = self.submitted;
        self.submitted += 1;
        // Bounded: entries leave when their frame comes out; a decoder that
        // swallowed frames cannot grow this past a few hundred.
        if self.media_times.len() < 512 {
            self.media_times.insert(id, frame.rtp_time);
        }
        while decoder.submit(&frame.data, id, qpc_now())? == DecodeSubmit::Busy {
            while let Some(decoded) = decoder.poll()? {
                self.show(decoded, presenter.as_deref_mut())?;
            }
        }
        while let Some(decoded) = decoder.poll()? {
            self.show(decoded, presenter.as_deref_mut())?;
        }
        Ok(())
    }

    fn show(
        &mut self,
        frame: DecodedFrame,
        presenter: Option<&mut VideoPresenter>,
    ) -> Result<(), Box<dyn Error>> {
        self.decoded += 1;
        self.size = Some((frame.width, frame.height));
        self.decode_times
            .push(frame.decoded_at.duration_since(frame.submitted_at));
        let media_time = self.media_times.remove(&frame.frame_id);
        if let Some(device) = self.device
            && self.stamp_every > 0
            && self.decoded.is_multiple_of(u64::from(self.stamp_every))
        {
            let width = frame_code::CODE_WIDTH;
            let luma = read_nv12_luma_region(
                device,
                &frame.texture,
                frame.slice,
                width,
                frame_code::CODE_HEIGHT,
            )?;
            if let Ok(stamp) = read_frame_stamp(|x, y| luma[(y * width + x) as usize]) {
                self.to_decoded
                    .push(frame.decoded_at.duration_since(stamp.timestamp));
                if self.stamps.len() < MAX_STAMPS {
                    self.stamps.insert(frame.frame_id, stamp.timestamp);
                }
                if let Some(media_time) = media_time
                    && self.origins.len() < MAX_STAMPS
                {
                    let ticks = media_time as i128 * qpc_frequency().ticks_per_second() as i128
                        / i128::from(transport::VIDEO_CLOCK_HZ as u32);
                    self.origins.push(stamp.timestamp.ticks() - ticks as i64);
                }
            }
        }
        let Some(presenter) = presenter else {
            return Ok(());
        };
        presenter.present(
            &frame.texture,
            frame.slice,
            frame.width,
            frame.height,
            FrameMeta {
                frame_id: frame.frame_id,
                captured_at: frame.captured_at,
                decoded_at: frame.decoded_at,
                media_time,
            },
        )?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct LatencyMs {
    count: usize,
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn summary_ms(samples: &[QpcDuration]) -> Option<LatencyMs> {
    let s = Summary::from_durations(samples, qpc_frequency())?;
    Some(LatencyMs {
        count: samples.len(),
        mean: s.mean_us / 1000.0,
        p50: s.p50_us / 1000.0,
        p95: s.p95_us / 1000.0,
        p99: s.p99_us / 1000.0,
        max: s.max_us / 1000.0,
    })
}

fn summary_durations(samples: &[std::time::Duration]) -> Option<LatencyMs> {
    if samples.is_empty() {
        return None;
    }
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q) as usize];
    Some(LatencyMs {
        count: ms.len(),
        mean: ms.iter().sum::<f64>() / ms.len() as f64,
        p50: at(0.5),
        p95: at(0.95),
        p99: at(0.99),
        max: ms[ms.len() - 1],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_line_names_every_stage_and_the_spacing() {
        let stats = LiveStats {
            fps: 52.3,
            received_fps: 60.0,
            decoded_fps: 60.0,
            dropped_per_s: 0.0,
            replaced_per_s: 7.7,
            cadence: Some(CadenceSummary {
                frames: 520,
                p50_ms: 13.9,
                p95_ms: 20.8,
                max_ms: 34.7,
                long_share: 0.048,
            }),
            audio: Some(AudioStats {
                heard_p50_ms: Some(85.2),
                heard_max_ms: Some(97.0),
                concealed: 2,
                gaps: 0,
                catch_ups: 1,
                underruns: 0,
            }),
            hold_ms: Some(12.4),
            size: Some((1920, 1080)),
            mbps: 6.1,
            loss: Some(0.0),
            rtt: Some(Duration::from_millis(58)),
            route: None,
        };
        assert_eq!(
            stats.log_line(),
            "received 60.0 fps, decoded 60.0, shown 52.3, dropped 0.0/s, replaced 7.7/s, hold 12 ms; \
             on screen p50 13.9 ms p95 20.8 ms max 34.7 ms, long 4.8%; \
             audio heard after p50 85.2 ms max 97.0 ms, catch-ups 1, concealed 2, gaps 0, \
             underruns 0; 1920x1080, 6.1 Mbps, loss 0.0%, rtt 58 ms"
        );
        assert_eq!(
            stats.metrics_lines()[3],
            "áudio: ouvido 85 ms após chegar · ajustes 1 · falhas 2"
        );
    }
}
