//! Viewer role: host's stream over WebRTC -> hardware decoder -> player window.
//!
//! Threads:
//! - network: owns the `ViewerSession`; hands complete frames to this thread
//!   through a bounded queue, asks the host for keyframes when needed;
//! - signaling watcher: notices the host's goodbye (or a closed connection);
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use decode::media_foundation::MediaFoundationDecoder;
use decode::{DecodeSubmit, DecodedFrame, VideoDecoder};
use gpu::convert::read_nv12_luma_region;
use gpu::{D3d11Device, DebugLayer};
use present::{FrameMeta, PresenterOptions, VideoPresenter};
use serde::Serialize;
use signaling::Message;
use signaling::local::LocalChannel;
use telemetry::frame_code::{self, read_frame_stamp};
use telemetry::{QpcDuration, QpcTimestamp, Summary, qpc_frequency, qpc_now};
use transport::impair::{ImpairmentConfig, ImpairmentStats};
use transport::{Frame, ViewerEvent, ViewerSession};

use crate::check::StreamCheck;
use crate::profile::Profile;

#[derive(Clone, Debug)]
pub struct ViewerArgs {
    pub connect: SocketAddr,
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
}

/// Frames waiting for the decoder (~250 ms at 60 fps): absorbs a burst
/// (a paced keyframe, then the frames queued behind it) without a drop.
const FRAME_QUEUE: usize = 16;
/// Spacing of repeated keyframe requests.
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);
const MAX_STAMPS: usize = 1 << 15;

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
    decoded: u64,
    presented: u64,
    stamps_read: u64,
    to_decoded_ms: Option<LatencyMs>,
    /// Last packet of a frame received to the decoder thread taking it.
    queue_wait_ms: Option<LatencyMs>,
    decode_ms: Option<LatencyMs>,
    to_displayed_ms: Option<LatencyMs>,
    end: String,
}

pub fn run(profile: &Profile, args: &ViewerArgs, stop: &AtomicBool) -> Result<(), Box<dyn Error>> {
    // Signaling and connection setup.
    let mut channel = LocalChannel::connect(args.connect)?;
    let (mut session, offer) = ViewerSession::offer("127.0.0.1:0".parse()?)?;
    session.set_impairment(args.impairment.clone());
    channel.send(&Message::Offer {
        sdp: offer,
        profile: profile.name.clone(),
    })?;
    match channel.recv()? {
        Message::Answer { sdp } => session.accept_answer(&sdp)?,
        Message::Bye { reason } => return Err(format!("host refused: {reason}").into()),
        Message::Offer { .. } => return Err("unexpected offer from the host".into()),
    }
    channel.wait_forever()?;

    // Headless without decoding needs no GPU at all.
    let device = if args.headless && !args.decode {
        None
    } else {
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
        Some(gpu::create_device(adapter, debug_layer)?)
    };
    let mut decoder = device
        .as_ref()
        .map(MediaFoundationDecoder::new)
        .transpose()?;
    let mut presenter = match (&device, args.headless) {
        (Some(device), false) => Some(VideoPresenter::open(
            device,
            PresenterOptions {
                title: format!("ziz - watching ({})", profile.name),
                width: 1280,
                height: 720,
                borderless: false,
                overlay: false,
                parent: None,
                position: args.position,
                background: args.position.is_some(),
                skip_when_hidden: true,
            },
        )?),
        _ => None,
    };
    let mode = match (args.headless, decoder.is_some()) {
        (false, _) => "window",
        (true, true) => "headless+decode",
        (true, false) => "headless",
    };

    let host_gone = Arc::new(AtomicBool::new(false));
    let network_stop = Arc::new(AtomicBool::new(false));
    let keyframe_wanted = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(NetworkCounters::default());
    let waker = session.waker();
    let (frame_tx, frame_rx) = mpsc::sync_channel::<Frame>(FRAME_QUEUE);
    let network = {
        let (stop, wanted, counters, gone) = (
            Arc::clone(&network_stop),
            Arc::clone(&keyframe_wanted),
            Arc::clone(&counters),
            Arc::clone(&host_gone),
        );
        std::thread::Builder::new()
            .name("network".into())
            .spawn(move || network(session, &frame_tx, &stop, &wanted, &counters, &gone))?
    };
    let watcher = {
        let (mut watch, gone, waker) =
            (channel.try_clone()?, Arc::clone(&host_gone), waker.clone());
        std::thread::Builder::new()
            .name("signaling-watch".into())
            .spawn(move || {
                let message = watch.recv();
                tracing::info!(?message, "signaling ended");
                gone.store(true, Ordering::Relaxed);
                waker.wake();
            })?
    };
    println!(
        "viewer '{}' ({mode}): connected to {} — close the window or Ctrl+C to stop",
        profile.name, args.connect
    );

    let started = Instant::now();
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
        if args.seconds > 0 && started.elapsed() >= Duration::from_secs(args.seconds) {
            break "time up";
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
    if !host_gone.load(Ordering::Relaxed) {
        let _ = channel.send(&Message::Bye { reason: end.into() });
    }
    channel.shutdown();
    network_stop.store(true, Ordering::Relaxed);
    waker.wake();
    let _ = network.join();
    let _ = watcher.join();
    let report = presenter.map(VideoPresenter::close);

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
        decoded: state.decoded,
        presented: report.as_ref().map_or(0, |r| r.counters.presented),
        stamps_read: state.to_decoded.len() as u64,
        to_decoded_ms: summary_ms(&state.to_decoded),
        queue_wait_ms: summary_durations(&state.queue_waits),
        decode_ms: summary_ms(&state.decode_times),
        to_displayed_ms: summary_ms(&to_displayed),
        end: end.to_owned(),
    };
    if summary.stream.problems() > 0 {
        tracing::warn!(stream = ?summary.stream, "the stream had problems");
    }
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
    match report.and_then(|r| r.error) {
        Some(error) => Err(error.into()),
        None => Ok(()),
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
    impairment: Mutex<Option<ImpairmentStats>>,
}

fn network(
    mut session: ViewerSession,
    frames: &SyncSender<Frame>,
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
                ViewerEvent::Connected => tracing::info!("media connected"),
                ViewerEvent::Audio(_) => {}
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
        self.decode_times
            .push(frame.decoded_at.duration_since(frame.submitted_at));
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
