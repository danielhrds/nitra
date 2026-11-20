//! Host role: capture -> encode (one encoder) -> every viewer over WebRTC.
//!
//! Threads:
//! - encode relay: owns the `HostPipeline`; forwards access units into a
//!   bounded queue (wakes the session) and applies keyframe/bitrate commands;
//! - signaling: accepts viewers on 127.0.0.1, moves offers/answers, notices
//!   when a viewer leaves;
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
use pipeline::{HostPipeline, PipelineConfig, PipelineReport, Received, Until};
use signaling::Message;
use signaling::local::{LocalChannel, LocalListener};
use telemetry::{QpcTimestamp, qpc_frequency, qpc_now};
use transport::{
    AUDIO_CLOCK_HZ, HostConfig, HostEvent, HostSession, PeerId, VIDEO_CLOCK_HZ, net::Waker,
};

use crate::fanout::{BEHIND_HOLD, KeyframeLimiter, Recovery, ViewerRate, encoder_target};
use crate::profile::Profile;
use crate::source::SourceWindow;

#[derive(Clone, Copy, Debug)]
pub enum Source {
    Monitor(usize),
    Synthetic { width: u32, height: u32 },
}

#[derive(Clone, Debug)]
pub struct HostArgs {
    pub port: u16,
    pub source: Source,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Stop after this long (0: until Ctrl+C).
    pub seconds: u64,
    /// Sound to share with the video (`None`: video only).
    pub audio: Option<CaptureSource>,
    pub audio_bitrate_bps: u32,
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
}

enum Request {
    Offer {
        sdp: String,
        profile: String,
        reply: SyncSender<Result<(PeerId, String), String>>,
    },
    Left(PeerId),
}

pub fn run(profile: &Profile, args: &HostArgs, stop: &AtomicBool) -> Result<(), Box<dyn Error>> {
    // One origin for the media time of video and audio, taken before any
    // capture starts, so viewers can line the two up.
    let origin = qpc_now();
    let adapters = gpu::enumerate_adapters()?;
    let adapter = adapters
        .iter()
        .find(|a| !a.info.is_software)
        .ok_or("no hardware adapter")?;
    let debug_layer = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };
    let device = gpu::create_device(adapter, debug_layer)?;
    let capture_options = CaptureOptions {
        fps: args.fps,
        cursor: false,
        hide_border: true,
        ..CaptureOptions::default()
    };

    let mut source_window = None;
    let (capture, size) = match args.source {
        Source::Synthetic { width, height } => {
            let window = SourceWindow::open(adapter, width, height, args.fps)?;
            std::thread::sleep(Duration::from_millis(300));
            let capture = capture::wgc::start(
                &device,
                CaptureTarget::Window(windows::Win32::Foundation::HWND(window.hwnd as *mut _)),
                capture_options,
            )?;
            source_window = Some(window);
            (capture, (width, height))
        }
        Source::Monitor(index) => {
            let outputs: Vec<_> = adapters
                .iter()
                .filter(|a| !a.info.is_software)
                .flat_map(|a| a.outputs.iter())
                .collect();
            let output = outputs
                .get(index)
                .ok_or_else(|| format!("no monitor {index}; {} found", outputs.len()))?;
            let desktop = output.info.desktop;
            let size = (
                u32::try_from(desktop.right - desktop.left)?,
                u32::try_from(desktop.bottom - desktop.top)?,
            );
            let (capture, _) = capture::select::start_monitor(
                &device,
                output,
                capture_options,
                capture::select::CapturePreference::Auto,
            )?;
            (capture, size)
        }
    };
    let pipeline = HostPipeline::start(
        &device,
        &adapter.info,
        capture,
        PipelineConfig {
            encoder: EncoderConfig {
                width: size.0,
                height: size.1,
                fps: args.fps,
                bitrate_bps: args.bitrate_bps,
                keyframe_interval: args.fps * 10,
                max_in_flight: 4,
            },
            encoder_preference: EncoderPreference::Auto,
            until: Until::Encode,
            output_capacity: 8,
            low_priority: false,
        },
    )?;
    let encoder = pipeline
        .selection()
        .map_or("?".to_owned(), |s| format!("{} ({})", s.kind, s.detail));

    let mut session = HostSession::bind(
        "127.0.0.1:0".parse()?,
        HostConfig {
            // Headroom above the encoder: the pacer sends at ~1.1x the
            // estimate, so an estimate pinned at the encoder rate queues
            // every frame behind a keyframe. The encoder is capped apart.
            start_bitrate_bps: u64::from(args.bitrate_bps) * 2,
            max_bitrate_bps: u64::from(args.bitrate_bps) * 3,
        },
    )?;
    let waker = session.waker();

    // Encode relay.
    let (unit_tx, unit_rx) = mpsc::sync_channel::<EncodedChunk>(UNIT_QUEUE);
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    let relay_stop = Arc::new(AtomicBool::new(false));
    let relay = {
        let (waker, relay_stop) = (waker.clone(), Arc::clone(&relay_stop));
        std::thread::Builder::new()
            .name("encode-relay".into())
            .spawn(move || relay(pipeline, &unit_tx, &command_rx, &waker, &relay_stop))?
    };

    // Audio relay: capture -> Opus -> bounded queue (wakes the session).
    let (audio_tx, audio_rx) = mpsc::sync_channel::<(Vec<u8>, QpcTimestamp)>(AUDIO_QUEUE);
    let audio = match args.audio {
        Some(source) => {
            let capture = AudioCapture::start(source)?;
            let encoder = OpusEncoder::new(args.audio_bitrate_bps)?;
            let (waker, relay_stop) = (waker.clone(), Arc::clone(&relay_stop));
            Some(
                std::thread::Builder::new()
                    .name("audio-relay".into())
                    .spawn(move || audio_relay(capture, encoder, &audio_tx, &waker, &relay_stop))?,
            )
        }
        None => None,
    };

    // Signaling.
    let listener = LocalListener::bind(args.port)?;
    let signaling_addr = listener.local_addr()?;
    let (request_tx, request_rx) = mpsc::channel::<Request>();
    let channels: Arc<Mutex<HashMap<PeerId, LocalChannel>>> = Arc::default();
    let signaling_stop = Arc::new(AtomicBool::new(false));
    let signaling = {
        let (waker, channels, signaling_stop) = (
            waker.clone(),
            Arc::clone(&channels),
            Arc::clone(&signaling_stop),
        );
        std::thread::Builder::new()
            .name("signaling".into())
            .spawn(move || {
                accept_viewers(&listener, &request_tx, &waker, &channels, &signaling_stop)
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
    tracing::info!(%signaling_addr, media = %session.local_addr(), %encoder, "host started");

    let started = Instant::now();
    let mut estimates: HashMap<PeerId, u64> = HashMap::new();
    let mut bitrate = args.bitrate_bps;
    let mut stats = HostStats::default();
    let mut keyframes = KeyframeLimiter::new(KEYFRAME_MIN_INTERVAL);
    let mut recovery = Recovery::new();
    let mut asked: HashSet<PeerId> = HashSet::new();
    let mut behind_at: HashMap<PeerId, Instant> = HashMap::new();
    let mut next_report = started + REPORT_INTERVAL;
    while !stop.load(Ordering::Relaxed)
        && (args.seconds == 0 || started.elapsed() < Duration::from_secs(args.seconds))
    {
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
                    reply,
                } => {
                    let result = session.accept(&sdp).map_err(|e| e.to_string());
                    if let Ok((peer, _)) = &result {
                        tracing::info!(peer = peer.0, viewer = %profile, "viewer joining");
                        stats.viewers_joined += 1;
                    }
                    let _ = reply.send(result);
                }
                Request::Left(peer) => {
                    log_peer(&session, peer, "viewer left");
                    session.remove(peer);
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
                    estimates.remove(&peer);
                    recovery.forget(peer);
                    behind_at.remove(&peer);
                    if let Some(channel) = channels.lock().unwrap().remove(&peer) {
                        channel.shutdown();
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
            && target.abs_diff(bitrate) > bitrate / 10
        {
            bitrate = target;
            stats.bitrate_changes += 1;
            tracing::info!(bitrate, "encoder bitrate follows the estimates");
            let _ = command_tx.send(Command::Bitrate(bitrate));
        }
    }
    for peer in session.peer_stats() {
        log_peer(&session, peer.id, "viewer at the end");
    }

    // Shutdown: tell the viewers, stop accepting, stop encoding.
    for (_, mut channel) in channels.lock().unwrap().drain() {
        let _ = channel.send(&Message::Bye {
            reason: "host stopped".into(),
        });
        channel.shutdown();
    }
    signaling_stop.store(true, Ordering::Relaxed);
    let _ = std::net::TcpStream::connect(signaling_addr); // unblocks accept
    let _ = signaling.join();
    relay_stop.store(true, Ordering::Relaxed);
    let report = relay.join().map_err(|_| "encode relay panicked")?;
    if let Some(thread) = audio {
        let audio = thread.join().map_err(|_| "audio relay panicked")?;
        println!(
            "host audio: {} packet(s) sent, capture {:?}",
            stats.audio_sent, audio
        );
    }
    if let Some(window) = source_window {
        window.close()?;
    }
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
    report.error.map_or(Ok(()), |e| Err(e.into()))
}

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

fn relay(
    pipeline: HostPipeline,
    units: &SyncSender<EncodedChunk>,
    commands: &Receiver<Command>,
    waker: &Waker,
    stop: &AtomicBool,
) -> PipelineReport {
    let mut dropped = 0_u64;
    while !stop.load(Ordering::Relaxed) {
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Keyframe => pipeline.request_keyframe(),
                Command::Bitrate(bps) => pipeline.set_bitrate(bps),
            }
        }
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
    pipeline.stop().0
}

fn accept_viewers(
    listener: &LocalListener,
    requests: &Sender<Request>,
    waker: &Waker,
    channels: &Arc<Mutex<HashMap<PeerId, LocalChannel>>>,
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
        let (requests, waker, channels) = (requests.clone(), waker.clone(), Arc::clone(channels));
        // One short-lived thread per viewer: setup, then wait for its goodbye.
        let _ = std::thread::Builder::new()
            .name("signaling-peer".into())
            .spawn(move || {
                let peer = match setup_viewer(&mut channel, &requests, &waker) {
                    Ok(peer) => peer,
                    Err(reason) => {
                        tracing::warn!(%reason, "viewer refused");
                        let _ = channel.send(&Message::Bye { reason });
                        return;
                    }
                };
                if let Ok(clone) = channel.try_clone() {
                    channels.lock().unwrap().insert(peer, clone);
                }
                let _ = channel.wait_forever();
                // Bye, or the connection closed: either way the viewer is gone.
                let _ = channel.recv();
                channels.lock().unwrap().remove(&peer);
                let _ = requests.send(Request::Left(peer));
                waker.wake();
            });
    }
}

fn setup_viewer(
    channel: &mut LocalChannel,
    requests: &Sender<Request>,
    waker: &Waker,
) -> Result<PeerId, String> {
    let Message::Offer { sdp, profile } = channel.recv().map_err(|e| e.to_string())? else {
        return Err("expected an offer".into());
    };
    let (reply, answer) = mpsc::sync_channel(1);
    requests
        .send(Request::Offer {
            sdp,
            profile,
            reply,
        })
        .map_err(|_| "host stopping".to_owned())?;
    waker.wake();
    let (peer, sdp) = answer
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "host busy".to_owned())??;
    channel
        .send(&Message::Answer { sdp })
        .map_err(|e| e.to_string())?;
    Ok(peer)
}
