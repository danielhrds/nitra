//! Host role: capture -> encode (one encoder) -> every viewer over WebRTC.
//!
//! Threads:
//! - encode relay: owns the `HostPipeline`; forwards access units into a
//!   bounded queue (wakes the session) and applies keyframe/bitrate commands;
//! - signaling: accepts viewers on 127.0.0.1, moves offers/answers, notices
//!   when a viewer leaves;
//! - this thread: owns the `HostSession` (network), the only place peers
//!   are added or removed.

use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capture::CaptureOptions;
use capture::wgc::CaptureTarget;
use encode::{EncodedChunk, EncoderConfig, EncoderPreference};
use gpu::DebugLayer;
use pipeline::{HostPipeline, PipelineConfig, PipelineReport, Received, Until};
use signaling::Message;
use signaling::local::{LocalChannel, LocalListener};
use telemetry::{QpcTimestamp, qpc_frequency};
use transport::{HostConfig, HostEvent, HostSession, PeerId, VIDEO_CLOCK_HZ, net::Waker};

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
}

/// Lowest bitrate the congestion estimate may push the encoder to.
const MIN_BITRATE_BPS: u32 = 500_000;
/// Access units waiting for the network thread (~130 ms at 60 fps).
const UNIT_QUEUE: usize = 8;

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
    let mut first_capture: Option<QpcTimestamp> = None;
    let mut estimates: HashMap<PeerId, u64> = HashMap::new();
    let mut bitrate = args.bitrate_bps;
    let mut stats = HostStats::default();
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
                    tracing::info!(peer = peer.0, "viewer left");
                    session.remove(peer);
                    estimates.remove(&peer);
                }
            }
        }
        while let Ok(unit) = unit_rx.try_recv() {
            let first = *first_capture.get_or_insert(unit.captured_at);
            let ticks = unit.captured_at.ticks().saturating_sub(first.ticks());
            // Media time from the capture clock, so pacing follows the source.
            let rtp_time = (ticks.max(0) as u128 * u128::from(VIDEO_CLOCK_HZ)
                / qpc_frequency().ticks_per_second() as u128) as u64;
            session.send_frame(&unit.data, rtp_time, unit.keyframe)?;
            stats.units_sent += 1;
        }
        while let Some(event) = session.next_event() {
            match event {
                HostEvent::Connected(peer) => {
                    tracing::info!(peer = peer.0, "viewer connected");
                    // A new viewer needs a keyframe to start decoding.
                    let _ = command_tx.send(Command::Keyframe);
                }
                HostEvent::Disconnected(peer) => {
                    tracing::info!(peer = peer.0, "viewer lost");
                    estimates.remove(&peer);
                    if let Some(channel) = channels.lock().unwrap().remove(&peer) {
                        channel.shutdown();
                    }
                }
                HostEvent::KeyframeRequested(_) => {
                    stats.keyframe_requests += 1;
                    let _ = command_tx.send(Command::Keyframe);
                }
                HostEvent::FellBehind(_) => {}
                HostEvent::BitrateEstimate { peer, bps } => {
                    estimates.insert(peer, bps);
                }
            }
        }
        // One encoder for everyone: follow the slowest viewer's estimate.
        if let Some(&slowest) = estimates.values().min() {
            let target = (slowest.min(u64::from(args.bitrate_bps)) as u32).max(MIN_BITRATE_BPS);
            if target.abs_diff(bitrate) > bitrate / 10 {
                bitrate = target;
                stats.bitrate_changes += 1;
                tracing::info!(bitrate, "encoder bitrate follows the estimate");
                let _ = command_tx.send(Command::Bitrate(bitrate));
            }
        }
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
    if let Some(window) = source_window {
        window.close()?;
    }
    let counters = report.counters;
    println!(
        "host done in {:.1} s: {} access units sent, {} viewer(s) joined, {} keyframe request(s), {} bitrate change(s); pipeline {counters:?}",
        started.elapsed().as_secs_f64(),
        stats.units_sent,
        stats.viewers_joined,
        stats.keyframe_requests,
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
    viewers_joined: u64,
    keyframe_requests: u64,
    bitrate_changes: u64,
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
