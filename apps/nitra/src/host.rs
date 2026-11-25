//! Host role: capture -> encode (one encoder) -> every viewer over WebRTC.
//!
//! Threads:
//! - encode relay: owns the `HostPipeline`; forwards access units into a
//!   bounded queue (wakes the session) and applies keyframe/bitrate commands;
//! - signaling: accepts viewers on 127.0.0.1, moves signed offers/answers
//!   (only from trusted viewer keys), notices when a viewer leaves;
//! - this thread: owns the `HostSession` (network), the only place peers
//!   are added or removed.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audio::capture::{AudioCapture, CaptureReport, CaptureSource};
use audio::opus::{MAX_PACKET, OpusEncoder};
use capture::CaptureOptions;
use capture::wgc::CaptureTarget;
use encode::{EncodedChunk, EncoderConfig, EncoderPreference};
use gpu::DebugLayer;
use identity::PublicKey;
use pipeline::{HostPipeline, PipelineConfig, PipelineCounters, PipelineReport, Received, Until};
use rooms::{ScreenInfo, ScreenKind};
use signaling::Message;
use signaling::local::{LocalChannel, LocalListener};
use signaling::signed::{Rejected, Trust};
use telemetry::{QpcTimestamp, qpc_frequency, qpc_now};
use transport::gather::NetworkConfig;
use transport::{
    AUDIO_CLOCK_HZ, HostConfig, HostEvent, HostSession, PeerId, VIDEO_CLOCK_HZ, net::Waker,
};

use crate::contacts::Contacts;
use crate::fanout::{
    BEHIND_HOLD, BitrateFollower, KeyframeLimiter, Recovery, ViewerRate, encoder_target,
};
use crate::manual::{self, ManualArgs};
use crate::profile::Profile;
use crate::room::{self, RoomArgs};
use crate::secure::{RecvError, Secure};
use crate::source::SourceWindow;

#[derive(Clone, Debug)]
pub enum Source {
    /// Monitor `index` (bench probe order), scaled to `size` (None: its own
    /// size).
    Monitor {
        index: usize,
        size: Option<(u32, u32)>,
    },
    /// A window, scaled to `width` x `height` (picked with its proportions;
    /// if it changes shape later, black bars keep them).
    Window {
        target: WindowTarget,
        width: u32,
        height: u32,
    },
    Synthetic {
        width: u32,
        height: u32,
    },
}

/// Which window to share.
#[derive(Clone, Debug)]
pub enum WindowTarget {
    /// The first visible window whose title contains this (command line).
    Title(String),
    /// This exact window (picked in the app's window).
    Handle(isize),
}

/// Which sound goes with the video.
#[derive(Clone, Debug)]
pub enum AudioPlan {
    Off,
    /// What the source calls for: a window's own app (its process tree);
    /// a monitor: everything except Discord, any nitra and `also_excluded`
    /// (friends in the voice chat must not hear themselves; no echo of
    /// streams this PC watches); the synthetic window: the synthetic tone.
    Auto {
        also_excluded: Vec<String>,
    },
    Fixed(CaptureSource),
}

/// Apps a whole-desktop share never captures, besides Discord: other nitra
/// processes, the window or the command line (a viewer on the same PC plays
/// streams).
const ALWAYS_EXCLUDED: [&str; 2] = ["nitra.exe", "nitra-cli.exe"];

#[derive(Clone, Debug)]
pub struct HostArgs {
    pub port: u16,
    pub source: Source,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Stop after this long (0: until Ctrl+C).
    pub seconds: u64,
    /// Sound to share with the video.
    pub audio: AudioPlan,
    pub audio_bitrate_bps: u32,
    /// Which addresses to offer (and whether to ask STUN servers).
    pub network: NetworkConfig,
    /// Which viewer keys may watch.
    pub trust: Trust,
    /// Also invite one viewer manually (texts copied and pasted).
    pub invite: Option<ManualArgs>,
    /// Also show the screen in a room and answer viewers through it.
    pub room: Option<RoomArgs>,
    /// Who is watching, refreshed every second (the app's window shows it).
    pub audience: Option<Arc<Mutex<Vec<Watcher>>>>,
    /// How often the room gets a preview of the screen (None: never).
    pub preview_interval: Option<Duration>,
    /// Requests to share another source without ending the share.
    pub switches: Option<Arc<Mutex<Receiver<Switch>>>>,
}

/// Someone watching this host, and how the link to them is doing.
#[derive(Clone, Debug, PartialEq)]
pub struct Watcher {
    pub key: PublicKey,
    pub name: String,
    pub link: transport::LinkStats,
}

/// Lowest bitrate the congestion estimate may push the encoder to.
const MIN_BITRATE_BPS: u32 = 500_000;
/// Access units waiting for the network thread (~130 ms at 60 fps).
const UNIT_QUEUE: usize = 8;
/// At most one keyframe per this, however many viewers ask: keyframes are
/// large, and every viewer receives them.
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// How often the per-viewer counters are logged.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);
/// Opus packets waiting for the network thread (1 s).
const AUDIO_QUEUE: usize = 50;

enum Command {
    Keyframe,
    Bitrate(u32),
    /// Stop the pipeline (another source is coming) and send its report.
    Release(SyncSender<PipelineReport>),
    /// Encode from this pipeline from now on.
    Take(Box<HostPipeline>),
}

/// A request to share another source; the outcome goes to `done`.
#[derive(Debug)]
pub struct Switch {
    pub source: Source,
    pub done: Sender<Result<(), String>>,
}

enum Request {
    Offer {
        sdp: String,
        profile: String,
        key: PublicKey,
        reply: SyncSender<Result<(PeerId, String), String>>,
    },
    /// The answer to our manual invite to `peer`, checked and trusted.
    InviteAnswered {
        peer: PeerId,
        sdp: String,
        profile: String,
        key: PublicKey,
    },
    Left(PeerId),
}

pub fn run(profile: &Profile, args: &HostArgs, stop: &AtomicBool) -> Result<(), Box<dyn Error>> {
    // One origin for the media time of video and audio, taken before any
    // capture starts, so viewers can line the two up.
    let origin = qpc_now();
    let adapters = gpu::enumerate_adapters()?;
    let adapter = crate::hardware::main_adapter(&adapters).ok_or("no hardware adapter")?;
    let debug_layer = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };
    let device = gpu::create_device(adapter, debug_layer)?;
    // What is shared now: a switch replaces all of these.
    let Started {
        capture,
        mut size,
        mut window_app,
        mut source_window,
    } = start_capture(&device, &adapters, &args.source, args.fps)?;
    let mut source = args.source.clone();
    let pipeline = start_pipeline(&device, adapter, capture, size, args.fps, args.bitrate_bps)?;
    let encoder = pipeline
        .selection()
        .map_or("?".to_owned(), |s| format!("{} ({})", s.kind, s.detail));

    let mut session = HostSession::bind_network(
        &args.network,
        HostConfig {
            // Headroom above the encoder: the pacer sends at ~1.1x the
            // estimate, so an estimate pinned at the encoder rate queues
            // every frame behind a keyframe. The encoder is capped apart.
            start_bitrate_bps: u64::from(args.bitrate_bps) * 2,
            max_bitrate_bps: u64::from(args.bitrate_bps) * 3,
        },
    )?;
    let gathered = session.gathered();
    // Kinds only: a public address identifies the user's connection.
    tracing::info!(candidates = %gathered.summary(), "gathered");
    println!(
        "host network: {:?}, {} candidate(s), NAT hint {:?}",
        args.network.scope,
        gathered.candidates.len(),
        gathered.nat
    );
    let waker = session.waker();

    // Encode relay.
    let (unit_tx, unit_rx) = mpsc::sync_channel::<EncodedChunk>(UNIT_QUEUE);
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    let relay_stop = Arc::new(AtomicBool::new(false));
    // Still pictures of the capture, for the room's previews.
    let mut snapshots = pipeline.snapshots();
    let mut pipeline_counters = pipeline.counter_reader();
    let relay = {
        let (waker, relay_stop) = (waker.clone(), Arc::clone(&relay_stop));
        std::thread::Builder::new()
            .name("encode-relay".into())
            .spawn(move || relay(pipeline, &unit_tx, &command_rx, &waker, &relay_stop))?
    };

    // Audio relay: capture -> Opus -> bounded queue (wakes the session).
    let (audio_tx, audio_rx) = mpsc::sync_channel::<(Vec<u8>, QpcTimestamp)>(AUDIO_QUEUE);
    let mut audio = match audio_source(&args.audio, &source, window_app)? {
        Some(sound) => Some(AudioRelay::start(
            sound,
            args.audio_bitrate_bps,
            audio_tx.clone(),
            waker.clone(),
        )?),
        None => None,
    };

    // Signaling.
    let listener = LocalListener::bind(args.port)?;
    let signaling_addr = listener.local_addr()?;
    let (request_tx, request_rx) = mpsc::channel::<Request>();
    let secure = Secure::new(Arc::clone(&profile.identity), args.trust.clone());
    let channels: Arc<Mutex<HashMap<PeerId, Viewer>>> = Arc::default();
    let signaling_stop = Arc::new(AtomicBool::new(false));
    let invite_requests = request_tx.clone();
    let room_requests = request_tx.clone();
    let signaling = {
        let name = profile.name.clone();
        let (waker, channels, signaling_stop, secure) = (
            waker.clone(),
            Arc::clone(&channels),
            Arc::clone(&signaling_stop),
            secure.clone(),
        );
        std::thread::Builder::new()
            .name("signaling".into())
            .spawn(move || {
                accept_viewers(
                    &listener,
                    &name,
                    &secure,
                    &request_tx,
                    &waker,
                    &channels,
                    &signaling_stop,
                )
            })?
    };

    println!(
        "host '{}': {}x{} @ {} fps, {} kbps, {encoder}; viewers connect to {signaling_addr} (media on {})",
        profile.name,
        size.0,
        size.1,
        args.fps,
        args.bitrate_bps / 1000,
        session.local_addr()
    );
    println!("host key: {}", profile.identity.public_key());
    if matches!(&args.trust, Trust::Keys(keys) if keys.is_empty()) {
        println!(
            "no --trust given: viewers on local signaling will be refused (their keys are shown)"
        );
    }
    if let Some(manual) = &args.invite {
        let (peer, offer) = session.invite()?;
        start_invite(
            profile,
            manual,
            &args.trust,
            peer,
            offer,
            invite_requests,
            waker.clone(),
        )?;
    } else {
        drop(invite_requests);
    }
    let room_stop = Arc::new(AtomicBool::new(false));
    // Previews go to the room thread, which makes the JPEG, seals and sends.
    let (preview_tx, preview_rx) = mpsc::sync_channel::<gpu::convert::Nv12Image>(1);
    let preview_wanted = Arc::new(AtomicBool::new(false));
    let mut preview_size = crate::preview::size_for(size.0, size.1);
    // A switch announces the new screen to the room.
    let (screen_tx, screen_rx) = mpsc::channel::<ScreenInfo>();
    let room = match &args.room {
        None => {
            drop(room_requests);
            None
        }
        Some(room_args) => {
            let screen = screen_info(&source, window_app, size, args.fps);
            // Room viewers by key, to find their peer when they say goodbye.
            let peers: Arc<Mutex<HashMap<PublicKey, PeerId>>> = Arc::default();
            let accept = {
                let (requests, waker, peers) =
                    (room_requests.clone(), waker.clone(), Arc::clone(&peers));
                move |sdp: String, profile: String, key: PublicKey| {
                    let (reply, answer) = mpsc::sync_channel(1);
                    requests
                        .send(Request::Offer {
                            sdp,
                            profile,
                            key,
                            reply,
                        })
                        .map_err(|_| "the host is stopping".to_owned())?;
                    waker.wake();
                    let (peer, sdp) = answer
                        .recv_timeout(Duration::from_secs(10))
                        .map_err(|_| "the host did not answer".to_owned())??;
                    peers.lock().unwrap().insert(key, peer);
                    Ok(sdp)
                }
            };
            let left = {
                let (requests, waker) = (room_requests, waker.clone());
                move |key: PublicKey| {
                    if let Some(peer) = peers.lock().unwrap().remove(&key) {
                        let _ = requests.send(Request::Left(peer));
                        waker.wake();
                    }
                }
            };
            Some(room::serve(
                Arc::clone(&profile.identity),
                profile.name.clone(),
                screen,
                room_args.clone(),
                Arc::clone(&room_stop),
                room::Previews {
                    pictures: preview_rx,
                    wanted: Arc::clone(&preview_wanted),
                },
                screen_rx,
                accept,
                left,
            )?)
        }
    };
    tracing::info!(
        signaling_port = signaling_addr.port(),
        media_port = session.local_addr().port(),
        %encoder,
        "host started"
    );

    let started = Instant::now();
    let mut estimates: HashMap<PeerId, u64> = HashMap::new();
    let mut bitrate = args.bitrate_bps;
    let mut follower = BitrateFollower::new(bitrate);
    let mut stats = HostStats::default();
    let mut keyframes = KeyframeLimiter::new(KEYFRAME_MIN_INTERVAL);
    let mut recovery = Recovery::new();
    let mut asked: HashSet<PeerId> = HashSet::new();
    let mut behind_at: HashMap<PeerId, Instant> = HashMap::new();
    let mut next_report = started + REPORT_INTERVAL;
    let mut share_meter = ShareMeter::new(started);
    // Who each viewer is, for the audience list.
    let mut watchers: HashMap<PeerId, (PublicKey, String)> = HashMap::new();
    let mut next_audience = started;
    // Previews: a first one shortly after starting (the capture has frames
    // by then), then every `preview::INTERVAL`, and one for someone who came
    // online (at most every `preview::MIN_INTERVAL`).
    let mut previews = crate::preview::Schedule::new(
        started,
        args.preview_interval.filter(|_| args.room.is_some()),
    );
    // An error that ends the share after the cleanup below.
    let mut fatal: Option<Box<dyn Error>> = None;
    while !stop.load(Ordering::Relaxed)
        && (args.seconds == 0 || started.elapsed() < Duration::from_secs(args.seconds))
    {
        // Another source: a new capture first (a window that is gone is
        // refused and the share goes on), then the old pipeline out (its
        // encoder too) and a new one in. The connections stay: viewers see
        // the last picture until the new source's first keyframe.
        let next = args
            .switches
            .as_ref()
            .and_then(|switches| switches.lock().unwrap().try_recv().ok());
        if let Some(Switch { source: next, done }) = next {
            let begun = Instant::now();
            let started = match start_capture(&device, &adapters, &next, args.fps) {
                Ok(started) => started,
                Err(error) => {
                    tracing::warn!(%error, "switch refused");
                    let _ = done.send(Err(error.to_string()));
                    continue;
                }
            };
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            let _ = command_tx.send(Command::Release(reply_tx));
            if let Ok(report) = reply_rx.recv_timeout(Duration::from_secs(5))
                && let Some(error) = report.error
            {
                tracing::warn!(%error, "the previous source's pipeline ended with an error");
            }
            let pipeline = match start_pipeline(
                &device,
                adapter,
                started.capture,
                started.size,
                args.fps,
                bitrate,
            ) {
                Ok(pipeline) => pipeline,
                Err(error) => {
                    let _ = done.send(Err(error.to_string()));
                    fatal = Some(error.into());
                    break;
                }
            };
            snapshots = pipeline.snapshots();
            pipeline_counters = pipeline.counter_reader();
            let _ = command_tx.send(Command::Take(Box::new(pipeline)));
            if let Some(window) = source_window.take()
                && let Err(error) = window.close()
            {
                tracing::warn!(%error, "closing the synthetic window");
            }
            (source, size, window_app, source_window) = (
                next,
                started.size,
                started.window_app,
                started.source_window,
            );
            // The sound follows the source; a failure leaves the video on.
            if let Some(relay) = audio.take() {
                let _ = relay.finish();
            }
            audio = match audio_source(&args.audio, &source, window_app) {
                Ok(Some(sound)) => {
                    match AudioRelay::start(
                        sound,
                        args.audio_bitrate_bps,
                        audio_tx.clone(),
                        waker.clone(),
                    ) {
                        Ok(relay) => Some(relay),
                        Err(error) => {
                            tracing::warn!(%error, "no sound for the new source");
                            None
                        }
                    }
                }
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(%error, "no sound for the new source");
                    None
                }
            };
            preview_size = crate::preview::size_for(size.0, size.1);
            let _ = screen_tx.send(screen_info(&source, window_app, size, args.fps));
            previews.soon(Instant::now());
            share_meter = ShareMeter::new(Instant::now());
            tracing::info!(
                width = size.0,
                height = size.1,
                ms = begun.elapsed().as_millis() as u64,
                "switched the shared source"
            );
            println!("host: switched source ({}x{})", size.0, size.1);
            let _ = done.send(Ok(()));
        }
        // The source window was closed or the capture ended: stop too.
        if relay.is_finished() || source_window.as_ref().is_some_and(SourceWindow::is_closed) {
            println!("host: the source or the capture ended; stopping");
            break;
        }
        session.poll(Instant::now() + transport::net::MAX_WAIT)?;
        while let Ok(request) = request_rx.try_recv() {
            match request {
                Request::Offer {
                    sdp,
                    profile,
                    key,
                    reply,
                } => {
                    let result = session.accept(&sdp).map_err(|e| e.to_string());
                    if let Ok((peer, _)) = &result {
                        tracing::info!(peer = peer.0, viewer = %profile, %key, "viewer joining");
                        stats.viewers_joined += 1;
                        watchers.insert(*peer, (key, profile));
                    }
                    let _ = reply.send(result);
                }
                Request::InviteAnswered {
                    peer,
                    sdp,
                    profile,
                    key,
                } => match session.accept_answer(peer, &sdp) {
                    Ok(()) => {
                        tracing::info!(peer = peer.0, viewer = %profile, %key, "invited viewer joining");
                        stats.viewers_joined += 1;
                        watchers.insert(peer, (key, profile));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "invite answer rejected");
                        println!("the answer could not be used: {error}");
                        session.remove(peer);
                    }
                },
                Request::Left(peer) => {
                    log_peer(&session, peer, "viewer left");
                    session.remove(peer);
                    watchers.remove(&peer);
                    recovery.forget(peer);
                    behind_at.remove(&peer);
                    estimates.remove(&peer);
                }
            }
        }
        while let Ok(unit) = unit_rx.try_recv() {
            // Media time from the capture clock, so pacing follows the source.
            let rtp_time = media_time(unit.captured_at, origin, VIDEO_CLOCK_HZ);
            session.send_frame(&unit.data, rtp_time, unit.keyframe)?;
            stats.units_sent += 1;
        }
        while let Ok((packet, captured_at)) = audio_rx.try_recv() {
            session.send_audio(&packet, media_time(captured_at, origin, AUDIO_CLOCK_HZ))?;
            stats.audio_sent += 1;
        }
        while let Some(event) = session.next_event() {
            match event {
                HostEvent::Connected(peer) => {
                    tracing::info!(peer = peer.0, "viewer connected");
                    // A new viewer needs a keyframe to start decoding.
                    keyframes.request();
                }
                HostEvent::Disconnected(peer) => {
                    tracing::info!(peer = peer.0, "viewer lost");
                    watchers.remove(&peer);
                    estimates.remove(&peer);
                    recovery.forget(peer);
                    behind_at.remove(&peer);
                    if let Some(viewer) = channels.lock().unwrap().remove(&peer) {
                        viewer.channel.shutdown();
                    }
                }
                HostEvent::KeyframeRequested(peer) => {
                    // Served through `recovery`, so a lossy link cannot fill
                    // everyone's stream with keyframes.
                    stats.keyframe_requests += 1;
                    asked.insert(peer);
                }
                HostEvent::FellBehind(peer) => {
                    stats.fell_behind += 1;
                    // Its keyframe comes from `recovery` below.
                    behind_at.insert(peer, Instant::now());
                    log_peer(&session, peer, "viewer fell behind; skipping to a keyframe");
                }
                HostEvent::BitrateEstimate { peer, bps } => {
                    estimates.insert(peer, bps);
                }
            }
        }
        let now = Instant::now();
        // Viewers that asked, or that the transport holds until a keyframe
        // (fell behind, or one went out but did not fit their link).
        let mut needing: Vec<PeerId> = session
            .peer_stats()
            .iter()
            .filter(|p| p.connected && p.waiting_keyframe && p.frames_sent > 0)
            .map(|p| p.id)
            .collect();
        for peer in asked.drain() {
            if !needing.contains(&peer) {
                needing.push(peer);
            }
        }
        // The backoff protects the other viewers from one viewer's
        // keyframes; a lone viewer has nobody to protect, so it is served
        // at once (still at most one keyframe per KEYFRAME_MIN_INTERVAL).
        let alone = session.connected() <= 1;
        let due = if alone {
            !needing.is_empty() && !keyframes.is_pending()
        } else {
            recovery.poll(now, &needing)
        };
        if due {
            stats.recovery_requests += 1;
            keyframes.request();
        }
        if now >= next_report {
            next_report = now + REPORT_INTERVAL;
            for peer in session.peer_stats() {
                log_peer(&session, peer.id, "viewer");
            }
        }
        if session.connected() > 0
            && let Some(line) =
                share_meter.tick(now, pipeline_counters.read(), stats.units_sent, bitrate)
        {
            tracing::info!("sharing: {line}");
        }
        if previews.due(now, preview_wanted.load(Ordering::Relaxed)) {
            preview_wanted.store(false, Ordering::Relaxed);
            snapshots.request(preview_size.0, preview_size.1);
        }
        match snapshots.take() {
            Some(Ok(picture)) => {
                // The room thread may still be busy with the last one: skip.
                let _ = preview_tx.try_send(picture);
            }
            Some(Err(error)) => tracing::warn!(%error, "preview picture failed"),
            None => {}
        }
        if let Some(audience) = &args.audience
            && now >= next_audience
        {
            next_audience = now + transport::STATS_INTERVAL;
            *audience.lock().unwrap() = session
                .peer_stats()
                .iter()
                .filter(|p| p.connected)
                .filter_map(|p| {
                    let (key, name) = watchers.get(&p.id)?;
                    Some(Watcher {
                        key: *key,
                        name: name.clone(),
                        link: p.link,
                    })
                })
                .collect();
        }
        if keyframes.poll(now) {
            stats.keyframes_forced += 1;
            let _ = command_tx.send(Command::Keyframe);
        }
        // One encoder for everyone: follow the slowest viewer that keeps up
        // (those that do not are isolated by the transport).
        let viewers: Vec<ViewerRate> = estimates
            .iter()
            .map(|(peer, &estimate_bps)| ViewerRate {
                estimate_bps,
                behind: behind_at
                    .get(peer)
                    .is_some_and(|&t| now.saturating_duration_since(t) < BEHIND_HOLD),
            })
            .collect();
        if let Some(target) = encoder_target(&viewers, args.bitrate_bps, MIN_BITRATE_BPS)
            && let Some(next) = follower.update(target, now)
        {
            bitrate = next;
            stats.bitrate_changes += 1;
            tracing::info!(bitrate, "encoder bitrate follows the estimates");
            let _ = command_tx.send(Command::Bitrate(bitrate));
        }
    }
    for peer in session.peer_stats() {
        log_peer(&session, peer.id, "viewer at the end");
    }
    if let Some(audience) = &args.audience {
        audience.lock().unwrap().clear();
    }

    // Shutdown: tell the viewers, stop accepting, stop encoding.
    for (_, mut viewer) in channels.lock().unwrap().drain() {
        let bye = Message::Bye {
            reason: "host stopped".into(),
        };
        let _ = secure.send(&mut viewer.channel, &viewer.key, bye, None);
        viewer.channel.shutdown();
    }
    signaling_stop.store(true, Ordering::Relaxed);
    let _ = std::net::TcpStream::connect(signaling_addr); // unblocks accept
    let _ = signaling.join();
    room_stop.store(true, Ordering::Relaxed);
    if let Some(room) = room {
        let _ = room.join();
    }
    relay_stop.store(true, Ordering::Relaxed);
    let report = relay.join().map_err(|_| "encode relay panicked")?;
    if let Some(relay) = audio {
        let audio = relay.finish()?;
        println!(
            "host audio: {} packet(s) sent, capture {:?}",
            stats.audio_sent, audio
        );
    }
    if let Some(window) = source_window {
        window.close()?;
    }
    if let Some(error) = fatal {
        return Err(error);
    }
    let report = report.ok_or("the encoder pipeline was not running at the end")?;
    let counters = report.counters;
    println!(
        "host done in {:.1} s: {} access units sent, {} viewer(s) joined, {} keyframe request(s), {} fell behind, {} recovery request(s), {} keyframe(s) forced, {} bitrate change(s); pipeline {counters:?}",
        started.elapsed().as_secs_f64(),
        stats.units_sent,
        stats.viewers_joined,
        stats.keyframe_requests,
        stats.fell_behind,
        stats.recovery_requests,
        stats.keyframes_forced,
        stats.bitrate_changes
    );
    let problems: Vec<_> = device
        .take_debug_messages()
        .into_iter()
        .filter(|m| m.severity.is_problem())
        .collect();
    if device.debug_layer_enabled() {
        println!("debug layer: {} warning/error", problems.len());
    }
    if let Some(error) = report.error {
        return Err(error.into());
    }
    // The window was closed, or the monitor went away (unplugged, turned
    // off): not an error of ours, but the share is over and says why.
    if !stop.load(Ordering::Relaxed)
        && report.capture_end == Some(capture::CaptureEnd::SourceClosed)
    {
        return Err(SourceGone {
            monitor: matches!(source, Source::Monitor { .. }),
        }
        .into());
    }
    Ok(())
}

/// A capture of a source, and what goes with it.
struct Started {
    capture: capture::Capture,
    /// The size it is encoded at.
    size: (u32, u32),
    /// The process whose sound goes with a shared window.
    window_app: Option<u32>,
    /// The synthetic test window, owned here.
    source_window: Option<SourceWindow>,
}

fn start_capture(
    device: &gpu::D3d11Device,
    adapters: &[gpu::Adapter],
    source: &Source,
    fps: u32,
) -> Result<Started, Box<dyn Error>> {
    let adapter = crate::hardware::main_adapter(adapters).ok_or("no hardware adapter")?;
    let options = CaptureOptions {
        fps,
        cursor: false,
        hide_border: true,
        ..CaptureOptions::default()
    };
    Ok(match source {
        &Source::Synthetic { width, height } => {
            let window = SourceWindow::open(adapter, width, height, fps)?;
            std::thread::sleep(Duration::from_millis(300));
            let capture = capture::wgc::start(
                device,
                CaptureTarget::Window(windows::Win32::Foundation::HWND(window.hwnd as *mut _)),
                options,
            )?;
            Started {
                capture,
                size: (width, height),
                window_app: None,
                source_window: Some(window),
            }
        }
        Source::Window {
            target,
            width,
            height,
        } => {
            let (window, full_title) = match target {
                WindowTarget::Title(title) => capture::wgc::find_window_by_title(title)
                    .ok_or_else(|| format!("no visible window title contains {title:?}"))?,
                &WindowTarget::Handle(handle) => {
                    let window = windows::Win32::Foundation::HWND(handle as *mut _);
                    // SAFETY: a plain query; a stale handle answers false.
                    if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(window)) }
                        .as_bool()
                    {
                        return Err("the window to share was closed".into());
                    }
                    (window, String::new())
                }
            };
            let window_app =
                audio::process::window_process(window.0 as isize, &audio::process::processes()?);
            println!("sharing window {full_title:?} (process {window_app:?})");
            Started {
                capture: capture::wgc::start(device, CaptureTarget::Window(window), options)?,
                size: (*width, *height),
                window_app,
                source_window: None,
            }
        }
        &Source::Monitor { index, size } => {
            let outputs: Vec<_> = adapters
                .iter()
                .filter(|a| !a.info.is_software)
                .flat_map(|a| a.outputs.iter())
                .collect();
            let output = outputs
                .get(index)
                .ok_or_else(|| format!("no monitor {index}; {} found", outputs.len()))?;
            let desktop = output.info.desktop;
            // The asked size (scaled on the GPU), or the monitor's own.
            let size = match size {
                Some(size) => size,
                None => (
                    u32::try_from(desktop.right - desktop.left)?,
                    u32::try_from(desktop.bottom - desktop.top)?,
                ),
            };
            let (capture, _) = capture::select::start_monitor(
                device,
                output,
                options,
                capture::select::CapturePreference::Auto,
            )?;
            Started {
                capture,
                size,
                window_app: None,
                source_window: None,
            }
        }
    })
}

fn start_pipeline(
    device: &gpu::D3d11Device,
    adapter: &gpu::Adapter,
    capture: capture::Capture,
    size: (u32, u32),
    fps: u32,
    bitrate_bps: u32,
) -> Result<HostPipeline, pipeline::StartError> {
    HostPipeline::start(
        device,
        &adapter.info,
        capture,
        PipelineConfig {
            encoder: EncoderConfig {
                width: size.0,
                height: size.1,
                fps,
                bitrate_bps,
                keyframe_interval: fps * 10,
                max_in_flight: 4,
            },
            encoder_preference: EncoderPreference::Auto,
            until: Until::Encode,
            output_capacity: 8,
            low_priority: false,
        },
    )
}

/// The sound that goes with `source` (`window_app`: a window's process).
fn audio_source(
    plan: &AudioPlan,
    source: &Source,
    window_app: Option<u32>,
) -> Result<Option<CaptureSource>, Box<dyn Error>> {
    Ok(match plan {
        AudioPlan::Off => None,
        AudioPlan::Fixed(source) => Some(source.clone()),
        AudioPlan::Auto { also_excluded } => Some(match source {
            Source::Window { .. } => {
                CaptureSource::App(window_app.ok_or("no process found for the shared window")?)
            }
            Source::Monitor { .. } => CaptureSource::SystemExcept(
                audio::process::DEFAULT_EXCLUDED
                    .iter()
                    .chain(ALWAYS_EXCLUDED.iter())
                    .map(|s| (*s).to_owned())
                    .chain(also_excluded.iter().cloned())
                    .collect(),
            ),
            Source::Synthetic { .. } => CaptureSource::Synthetic,
        }),
    })
}

/// A running audio relay and its own stop flag (a switch replaces it).
struct AudioRelay {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<CaptureReport>,
}

impl AudioRelay {
    fn start(
        source: CaptureSource,
        bitrate_bps: u32,
        packets: SyncSender<(Vec<u8>, QpcTimestamp)>,
        waker: Waker,
    ) -> Result<Self, Box<dyn Error>> {
        println!("audio: {source:?}");
        let capture = AudioCapture::start(source)?;
        let encoder = OpusEncoder::new(bitrate_bps)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("audio-relay".into())
                .spawn(move || audio_relay(capture, encoder, &packets, &waker, &stop))?
        };
        Ok(Self { stop, thread })
    }

    fn finish(self) -> Result<CaptureReport, Box<dyn Error>> {
        self.stop.store(true, Ordering::Relaxed);
        Ok(self.thread.join().map_err(|_| "audio relay panicked")?)
    }
}

/// The shared window was closed, or the shared monitor went away.
#[derive(Debug)]
pub struct SourceGone {
    pub monitor: bool,
}

impl std::fmt::Display for SourceGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.monitor {
            "the shared monitor went away"
        } else {
            "the shared window was closed"
        })
    }
}

impl Error for SourceGone {}

#[derive(Default)]
struct HostStats {
    units_sent: u64,
    audio_sent: u64,
    viewers_joined: u64,
    keyframe_requests: u64,
    fell_behind: u64,
    recovery_requests: u64,
    keyframes_forced: u64,
    bitrate_changes: u64,
}

fn log_peer(session: &HostSession, peer: PeerId, what: &str) {
    let Some(s) = session.peer_stats().into_iter().find(|s| s.id == peer) else {
        tracing::info!(peer = peer.0, "{what}");
        return;
    };
    tracing::info!(
        peer = peer.0,
        estimate_kbps = s.estimate_bps / 1000,
        sent = s.frames_sent,
        skipped = s.frames_skipped,
        behind = s.times_behind,
        waiting = s.waiting_keyframe,
        route = ?s.link.route,
        "{what}"
    );
}

/// Media time of `at` since `origin` in `clock_hz` units (0 before it).
fn media_time(at: QpcTimestamp, origin: QpcTimestamp, clock_hz: u64) -> u64 {
    let ticks = at.ticks().saturating_sub(origin.ticks()).max(0) as u128;
    (ticks * u128::from(clock_hz) / u128::from(qpc_frequency().ticks_per_second())) as u64
}

/// Captured audio -> Opus packets for the network thread. A full queue
/// drops the packet: audio must never hold anything up.
fn audio_relay(
    capture: AudioCapture,
    mut encoder: OpusEncoder,
    packets: &SyncSender<(Vec<u8>, QpcTimestamp)>,
    waker: &Waker,
    stop: &AtomicBool,
) -> CaptureReport {
    let mut buf = [0_u8; MAX_PACKET];
    let mut dropped = 0_u64;
    while !stop.load(Ordering::Relaxed) {
        let Some(frame) = capture.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        let n = match encoder.encode(&frame.samples, &mut buf) {
            Ok(n) => n,
            Err(error) => {
                tracing::warn!(%error, "audio encode failed");
                continue;
            }
        };
        match packets.try_send((buf[..n].to_vec(), frame.captured_at)) {
            Ok(()) => waker.wake(),
            Err(TrySendError::Full(_)) => dropped += 1,
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    if dropped > 0 {
        tracing::warn!(dropped, "audio packets dropped before the network");
    }
    capture.stop()
}

/// Forwards the pipeline's access units; ends with the last pipeline's
/// report (None if it ended between a release and the next pipeline).
fn relay(
    pipeline: HostPipeline,
    units: &SyncSender<EncodedChunk>,
    commands: &Receiver<Command>,
    waker: &Waker,
    stop: &AtomicBool,
) -> Option<PipelineReport> {
    let mut dropped = 0_u64;
    let mut current = Some(pipeline);
    while !stop.load(Ordering::Relaxed) {
        while let Ok(command) = commands.try_recv() {
            match (command, &current) {
                (Command::Keyframe, Some(pipeline)) => pipeline.request_keyframe(),
                (Command::Bitrate(bps), Some(pipeline)) => pipeline.set_bitrate(bps),
                (Command::Keyframe | Command::Bitrate(_), None) => {}
                (Command::Release(reply), _) => {
                    if let Some(pipeline) = current.take() {
                        let _ = reply.send(pipeline.stop().0);
                    }
                }
                (Command::Take(pipeline), _) => current = Some(*pipeline),
            }
        }
        let Some(pipeline) = &current else {
            // Between two sources: wait for the next pipeline.
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        match pipeline.recv(Duration::from_millis(10)) {
            Received::Chunk(unit) => {
                match units.try_send(unit) {
                    Ok(()) => {}
                    // The network thread is behind: drop, and restart the
                    // viewers' decoding from a keyframe.
                    Err(TrySendError::Full(_)) => {
                        dropped += 1;
                        pipeline.request_keyframe();
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
                waker.wake();
            }
            Received::Timeout => {}
            Received::Finished => break,
        }
    }
    if dropped > 0 {
        tracing::warn!(dropped, "access units dropped before the network");
    }
    current.map(|pipeline| pipeline.stop().0)
}

/// A connected viewer's signaling channel and the key its messages are
/// signed with (ours are sealed for it).
struct Viewer {
    channel: LocalChannel,
    key: PublicKey,
}

/// Why a viewer was refused, and its key when its message proved it (so the
/// refusal can be signed for it).
struct Refusal {
    reason: String,
    key: Option<PublicKey>,
}

fn accept_viewers(
    listener: &LocalListener,
    name: &str,
    secure: &Secure,
    requests: &Sender<Request>,
    waker: &Waker,
    channels: &Arc<Mutex<HashMap<PeerId, Viewer>>>,
    stop: &AtomicBool,
) {
    loop {
        let channel = listener.accept();
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut channel = match channel {
            Ok(channel) => channel,
            Err(error) => {
                tracing::warn!(%error, "signaling accept failed");
                continue;
            }
        };
        let name = name.to_owned();
        let (secure, requests, waker, channels) = (
            secure.clone(),
            requests.clone(),
            waker.clone(),
            Arc::clone(channels),
        );
        // One short-lived thread per viewer: setup, then wait for its goodbye.
        let _ = std::thread::Builder::new()
            .name("signaling-peer".into())
            .spawn(move || {
                let (peer, key) =
                    match setup_viewer(&mut channel, &name, &secure, &requests, &waker) {
                        Ok(joined) => joined,
                        Err(Refusal { reason, key }) => {
                            tracing::warn!(%reason, "viewer refused");
                            println!("viewer refused: {reason}");
                            // Only a viewer whose signature checked out gets a
                            // (signed) reason; anything else is just closed.
                            if let Some(key) = key {
                                let _ =
                                    secure.send(&mut channel, &key, Message::Bye { reason }, None);
                            }
                            channel.shutdown();
                            return;
                        }
                    };
                if let Ok(clone) = channel.try_clone() {
                    channels.lock().unwrap().insert(
                        peer,
                        Viewer {
                            channel: clone,
                            key,
                        },
                    );
                }
                let _ = channel.wait_forever();
                // Bye, or the connection closed: either way the viewer is gone.
                // An unverifiable message ends it too: the channel is not
                // trustworthy any more.
                if let Err(error) = secure.recv(&mut channel)
                    && matches!(error, RecvError::Rejected(_))
                {
                    tracing::warn!(%error, peer = peer.0, "ending viewer");
                }
                channels.lock().unwrap().remove(&peer);
                let _ = requests.send(Request::Left(peer));
                waker.wake();
            });
    }
}

fn setup_viewer(
    channel: &mut LocalChannel,
    name: &str,
    secure: &Secure,
    requests: &Sender<Request>,
    waker: &Waker,
) -> Result<(PeerId, PublicKey), Refusal> {
    let opened = secure.recv(channel).map_err(|error| Refusal {
        key: match &error {
            RecvError::Rejected(Rejected::Untrusted(code)) => code.parse().ok(),
            _ => None,
        },
        reason: match &error {
            RecvError::Rejected(Rejected::Untrusted(code)) => {
                format!("key {code} is not trusted (start the host with --trust {code})")
            }
            other => other.to_string(),
        },
    })?;
    let key = opened.from;
    let refuse = |reason: &str| Refusal {
        reason: reason.to_owned(),
        key: Some(key),
    };
    let Message::Offer { sdp, profile } = opened.message else {
        return Err(refuse("expected an offer"));
    };
    let (reply, answer) = mpsc::sync_channel(1);
    requests
        .send(Request::Offer {
            sdp,
            profile,
            key,
            reply,
        })
        .map_err(|_| refuse("host stopping"))?;
    waker.wake();
    let (peer, sdp) = answer
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| refuse("host busy"))?
        .map_err(|e| refuse(&e))?;
    // The answer names the offer it answers, so it cannot be replayed onto
    // another one.
    secure
        .send(
            channel,
            &key,
            Message::Answer {
                sdp,
                profile: name.to_owned(),
            },
            Some(&opened.nonce),
        )
        .map_err(|e| refuse(&e.to_string()))?;
    Ok((peer, key))
}

/// Hands over a manual invite for `peer` (our signed offer) and waits, on
/// its own thread, for the one answer to it. A checked answer from a
/// trusted key (or one the user accepts) goes to the network thread;
/// anything else drops the invited peer.
fn start_invite(
    profile: &Profile,
    manual: &ManualArgs,
    trust: &Trust,
    peer: PeerId,
    offer: String,
    requests: Sender<Request>,
    waker: Waker,
) -> Result<(), Box<dyn Error>> {
    let (invite, nonce) = signaling::signed::seal_manual(
        &profile.identity,
        None,
        Message::Offer {
            sdp: offer,
            profile: profile.name.clone(),
        },
        None,
        signaling::signed::now(),
    )?;
    manual::hand_over(&invite, manual.invite_file.as_deref(), "invite")?;
    let identity = Arc::clone(&profile.identity);
    let contacts_path = profile.dir.join("contacts.json");
    let (manual, trust) = (manual.clone(), trust.clone());
    std::thread::Builder::new()
        .name("invite".into())
        .spawn(move || {
            let request = match take_answer(&identity, &contacts_path, &manual, &trust, &nonce) {
                Ok((sdp, profile, key)) => Request::InviteAnswered {
                    peer,
                    sdp,
                    profile,
                    key,
                },
                Err(reason) => {
                    tracing::warn!(%reason, "invite ended");
                    println!("invite ended: {reason}");
                    Request::Left(peer)
                }
            };
            let _ = requests.send(request);
            waker.wake();
        })?;
    Ok(())
}

/// The answer to the invite `nonce`: (SDP, sender's name, sender's key).
fn take_answer(
    identity: &Arc<identity::Identity>,
    contacts_path: &std::path::Path,
    manual: &ManualArgs,
    trust: &Trust,
    nonce: &str,
) -> Result<(String, String, PublicKey), String> {
    let mut verifier = signaling::signed::Verifier::new(*identity.public_key(), Trust::Anyone);
    loop {
        let envelope =
            manual::take(manual.answer_file.as_deref(), "answer").map_err(|e| e.to_string())?;
        let opened = match verifier.open(&envelope, signaling::signed::now()) {
            Ok(opened) => opened,
            // From a file nobody fixes it; on the clipboard the user can retry.
            Err(error) if manual.answer_file.is_some() => return Err(error.to_string()),
            Err(error) => {
                println!("answer refused: {error}");
                continue;
            }
        };
        let Message::Answer { sdp, profile } = opened.message else {
            println!("that text is not an answer");
            if manual.answer_file.is_some() {
                return Err("not an answer".into());
            }
            continue;
        };
        // Each invite is answered once: an answer must name this one.
        if opened.reply_to.as_deref() != Some(nonce) {
            return Err("the answer is for another invite".into());
        }
        let name = manual::clean_name(&profile);
        let trusted = match trust {
            Trust::Anyone => true,
            Trust::Keys(keys) => {
                let mut contacts = Contacts::load(contacts_path).map_err(|e| e.to_string())?;
                manual::trust(&mut contacts, keys, &opened.from, &name)
                    .map_err(|e| e.to_string())?
            }
        };
        if !trusted {
            return Err(format!("'{name}' was not trusted"));
        }
        return Ok((sdp, name, opened.from));
    }
}

/// The shared screen as the room shows it.
fn screen_info(
    source: &Source,
    window_app: Option<u32>,
    (width, height): (u32, u32),
    fps: u32,
) -> ScreenInfo {
    ScreenInfo {
        kind: if matches!(source, Source::Monitor { .. }) {
            ScreenKind::Desktop
        } else {
            ScreenKind::Window
        },
        name: screen_name(source, window_app),
        width,
        height,
        fps,
    }
}

/// A short name for the shared screen as the room shows it: never a window
/// title (it can show private things), but the app's name.
fn screen_name(source: &Source, window_app: Option<u32>) -> String {
    match source {
        Source::Monitor { index, .. } => format!("Monitor {}", index + 1),
        Source::Synthetic { .. } => "Synthetic test window".into(),
        Source::Window { .. } => window_app
            .and_then(|pid| audio::process::processes().ok()?.remove(&pid))
            .map(|p| p.name.trim_end_matches(".exe").to_owned())
            .unwrap_or_else(|| "Window".into()),
    }
}

/// How often the host's stage rates go to the log (while someone watches):
/// ~200 bytes per line, about 70 KB an hour.
const SHARE_LOG_EVERY: Duration = Duration::from_secs(10);

/// Turns the pipeline's counters into per-second rates for the log, every
/// [`SHARE_LOG_EVERY`]: where frames are lost before they leave this PC.
struct ShareMeter {
    since: Instant,
    counters: PipelineCounters,
    units_sent: u64,
}

impl ShareMeter {
    fn new(now: Instant) -> Self {
        Self {
            since: now,
            counters: PipelineCounters::default(),
            units_sent: 0,
        }
    }

    /// A log line once [`SHARE_LOG_EVERY`] has passed since the last one.
    fn tick(
        &mut self,
        now: Instant,
        counters: PipelineCounters,
        units_sent: u64,
        bitrate_bps: u32,
    ) -> Option<String> {
        let span = now.duration_since(self.since);
        if span < SHARE_LOG_EVERY {
            return None;
        }
        let seconds = span.as_secs_f64();
        let then = &self.counters;
        let rate = |a: u64, b: u64| a.saturating_sub(b) as f64 / seconds;
        let line = format!(
            "captured {:.1} fps, encoded {:.1}, sent {:.1}; replaced before encode {:.1}/s, encoder busy {:.1}/s, lost before send {:.1}/s; {} kbps",
            rate(counters.frames_taken, then.frames_taken),
            rate(counters.access_units, then.access_units),
            rate(units_sent, self.units_sent),
            rate(counters.held_replaced, then.held_replaced),
            rate(counters.busy, then.busy),
            rate(
                counters.output_full_dropped
                    + counters.dropped_until_keyframe
                    + counters.convert_dropped,
                then.output_full_dropped + then.dropped_until_keyframe + then.convert_dropped,
            ),
            bitrate_bps / 1000,
        );
        *self = Self {
            since: now,
            counters,
            units_sent,
        };
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_share_meter_logs_rates_every_ten_seconds() {
        let start = Instant::now();
        let mut meter = ShareMeter::new(start);
        let counters = |taken: u64, units: u64, replaced: u64| PipelineCounters {
            frames_taken: taken,
            access_units: units,
            held_replaced: replaced,
            ..PipelineCounters::default()
        };
        assert_eq!(
            meter.tick(
                start + Duration::from_secs(5),
                counters(300, 300, 0),
                300,
                6_000_000
            ),
            None
        );
        let line = meter
            .tick(
                start + Duration::from_secs(10),
                counters(600, 580, 20),
                570,
                6_000_000,
            )
            .unwrap();
        assert_eq!(
            line,
            "captured 60.0 fps, encoded 58.0, sent 57.0; replaced before encode 2.0/s, \
             encoder busy 0.0/s, lost before send 0.0/s; 6000 kbps"
        );
        // The next span starts where this one ended.
        let line = meter
            .tick(
                start + Duration::from_secs(20),
                counters(1200, 1180, 20),
                1170,
                6_000_000,
            )
            .unwrap();
        assert!(
            line.starts_with("captured 60.0 fps, encoded 60.0, sent 60.0;"),
            "{line}"
        );
    }
}
