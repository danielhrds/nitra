//! Session scenarios on 127.0.0.1 with impaired networks:
//! an idle host, viewers coming and going, peers vanishing, an outage,
//! garbage datagrams, loss, bursts, delay, jitter and reordering.
//!
//! The host runs on the test thread and answers keyframe requests like the
//! app (at most one keyframe per 500 ms); each viewer has its own thread and
//! restarts from a keyframe after a loss, like the app. Every frame a viewer
//! accepts is checked byte for byte.

mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::{FPS, host, localhost};
use transport::impair::{ImpairmentConfig, ImpairmentStats};
use transport::{HostEvent, HostSession, PeerId, VIDEO_CLOCK_HZ, ViewerEvent, ViewerSession};

/// Frames between scheduled keyframes (10 s): losses must be recovered by
/// retransmission or a requested keyframe, not by the schedule.
const GOP: u64 = 600;
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);
const MS: Duration = Duration::from_millis(1);

/// Access unit `index`: an IDR slice (~120 KB) or a delta slice (8–14 KB),
/// bytes derived from (index, keyframe) and free of start codes.
fn unit(index: u64, keyframe: bool) -> Vec<u8> {
    let (nal_type, size) = if keyframe {
        (5_u8, 120_000)
    } else {
        (1_u8, 8_000 + (index as usize % 7) * 900)
    };
    let mut data = vec![0, 0, 0, 1, 0x60 | nal_type];
    data.extend((0..size).map(|i| 0x10 + ((i as u64 * 31 + index) % 0xE0) as u8));
    data
}

/// The frame's index and kind if its bytes are exactly what was sent.
fn check(data: &[u8], rtp_time: u64) -> Option<(u64, bool)> {
    let index = rtp_time * FPS / VIDEO_CLOCK_HZ;
    let keyframe = data.get(4)? & 0x1F == 5;
    (*data == *unit(index, keyframe)).then_some((index, keyframe))
}

#[derive(Debug, Default)]
struct Report {
    /// Frames handed to the "decoder": index and arrival time.
    accepted: Vec<(u64, Instant)>,
    corrupt: u64,
    /// Complete frames skipped while waiting for a keyframe.
    skipped: u64,
    keyframe_requests: u64,
    impairment: Option<ImpairmentStats>,
}

struct Viewer {
    peer: PeerId,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    /// The viewer's session ended (host gone or connection lost).
    ended: Arc<AtomicBool>,
    thread: JoinHandle<Report>,
}

impl Viewer {
    /// Joins `host`; `close` says whether stopping says goodbye (false: the
    /// viewer just vanishes).
    fn join(host: &mut HostSession, impairment: Option<ImpairmentConfig>, close: bool) -> Self {
        let (mut session, offer) = ViewerSession::offer(localhost()).unwrap();
        session.set_impairment(impairment);
        let (peer, answer) = host.accept(&offer).unwrap();
        session.accept_answer(&answer).unwrap();
        let addr = session.local_addr();
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, ended) = (Arc::clone(&stop), Arc::clone(&ended));
            std::thread::spawn(move || watch(session, &stop, &ended, close))
        };
        Self {
            peer,
            addr,
            stop,
            ended,
            thread,
        }
    }

    fn finish(self) -> Report {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap()
    }
}

/// The app's viewer loop without the decoder.
fn watch(mut session: ViewerSession, stop: &AtomicBool, ended: &AtomicBool, close: bool) -> Report {
    let mut report = Report::default();
    let mut need_keyframe = true;
    let mut last_request: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        if session
            .poll(Instant::now() + Duration::from_millis(5))
            .is_err()
            || !session.is_alive()
        {
            ended.store(true, Ordering::Relaxed);
            break;
        }
        while let Some(event) = session.next_event() {
            match event {
                ViewerEvent::Connected | ViewerEvent::Audio(_) => {}
                ViewerEvent::Disconnected => ended.store(true, Ordering::Relaxed),
                ViewerEvent::Frame(frame) => {
                    if !frame.contiguous {
                        need_keyframe = true;
                    }
                    let Some((index, keyframe)) = check(&frame.data, frame.rtp_time) else {
                        report.corrupt += 1;
                        need_keyframe = true;
                        continue;
                    };
                    if need_keyframe && !keyframe {
                        report.skipped += 1;
                        continue;
                    }
                    need_keyframe = false;
                    report.accepted.push((index, frame.received_at));
                }
            }
        }
        if need_keyframe
            && last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY)
            && session.request_keyframe()
        {
            report.keyframe_requests += 1;
            last_request = Some(Instant::now());
        }
    }
    report.impairment = session.impairment_stats();
    if close {
        session.close();
    }
    report
}

/// The host side: sends 60 fps and answers keyframe requests.
struct Sender {
    host: HostSession,
    index: u64,
    keyframe_due: bool,
    last_keyframe: Option<Instant>,
    events: Vec<HostEvent>,
}

impl Sender {
    fn new(host: HostSession) -> Self {
        Self {
            host,
            index: 0,
            keyframe_due: true,
            last_keyframe: None,
            events: Vec::new(),
        }
    }

    fn wait_connected(&mut self, viewers: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.host.connected() < viewers {
            assert!(Instant::now() < deadline, "viewers not connected");
            self.poll(Instant::now() + 5 * MS);
        }
    }

    fn poll(&mut self, deadline: Instant) {
        self.host.poll(deadline).unwrap();
        while let Some(event) = self.host.next_event() {
            if matches!(
                event,
                HostEvent::Connected(_) | HostEvent::KeyframeRequested(_)
            ) {
                self.keyframe_due = true;
            }
            self.events.push(event);
        }
    }

    /// Runs for `duration`, sending frames if `sending`; returns how many
    /// frames were sent.
    fn run(&mut self, duration: Duration, sending: bool) -> u64 {
        let period = Duration::from_nanos(1_000_000_000 / FPS);
        let end = Instant::now() + duration;
        let mut next = Instant::now();
        let mut sent = 0;
        while Instant::now() < end {
            if sending && Instant::now() >= next {
                let ready = self
                    .last_keyframe
                    .is_none_or(|t| t.elapsed() >= KEYFRAME_MIN_INTERVAL);
                let keyframe = self.index.is_multiple_of(GOP) || (self.keyframe_due && ready);
                if keyframe {
                    self.keyframe_due = false;
                    self.last_keyframe = Some(Instant::now());
                }
                let rtp_time = self.index * VIDEO_CLOCK_HZ / FPS;
                self.host
                    .send_frame(&unit(self.index, keyframe), rtp_time, keyframe)
                    .unwrap();
                self.index += 1;
                sent += 1;
                next += period;
            }
            let deadline = if sending { next.min(end) } else { end };
            self.poll(deadline);
        }
        sent
    }
}

/// Streams `seconds` to one viewer with `impairment` on its side (and
/// `feedback` on the host's); returns the report and frames sent.
fn stream_through(
    impairment: Option<ImpairmentConfig>,
    feedback: Option<ImpairmentConfig>,
    seconds: u64,
) -> (Report, u64) {
    let mut sender = Sender::new(host());
    sender.host.set_impairment(feedback);
    let viewer = Viewer::join(&mut sender.host, impairment, true);
    sender.wait_connected(1);
    let sent = sender.run(Duration::from_secs(seconds), true);
    sender.run(Duration::from_millis(500), false);
    (viewer.finish(), sent)
}

fn summary(name: &str, report: &Report, sent: u64) -> f64 {
    let share = report.accepted.len() as f64 / sent as f64;
    println!(
        "{name}: accepted {}/{sent} ({:.1} %), skipped {}, corrupt {}, keyframe requests {}, impairment {:?}",
        report.accepted.len(),
        share * 100.0,
        report.skipped,
        report.corrupt,
        report.keyframe_requests,
        report.impairment
    );
    share
}

/// Longest gap between consecutive accepted frames.
fn longest_freeze(report: &Report) -> Duration {
    report
        .accepted
        .windows(2)
        .map(|w| w[1].1.saturating_duration_since(w[0].1))
        .max()
        .unwrap_or_default()
}

// Scenario 1: no frames for a while.
#[test]
fn an_idle_host_keeps_the_session_without_spinning() {
    let mut sender = Sender::new(host());
    let viewer = Viewer::join(&mut sender.host, None, true);
    sender.wait_connected(1);
    sender.run(Duration::from_secs(1), true);
    let before = sender.host.wakeups();
    sender.run(Duration::from_secs(5), false);
    let idle_wakeups = sender.host.wakeups() - before;
    sender.run(Duration::from_secs(1), true);
    let ended = viewer.ended.load(Ordering::Relaxed);
    let report = viewer.finish();
    println!("idle: {idle_wakeups} host wake-ups in 5 s");
    // Wake-ups come from str0m's timers and the 100 ms cap, not a busy loop.
    assert!(idle_wakeups < 1000, "{idle_wakeups} wake-ups in 5 s");
    assert!(!ended, "the viewer lost the session while idle");
    let after = report.accepted.iter().filter(|(i, _)| *i >= 60).count();
    assert!(after >= 50, "only {after} frames after the pause");
}

// Scenario 3: viewers come and go; nothing is left behind.
#[test]
fn viewers_come_and_go_without_leftovers() {
    let mut sender = Sender::new(host());
    for round in 0..20 {
        let viewer = Viewer::join(&mut sender.host, None, true);
        sender.wait_connected(1);
        sender.run(Duration::from_millis(800), true);
        let peer = viewer.peer;
        let report = viewer.finish();
        sender.host.remove(peer);
        assert!(
            report.accepted.len() >= 15,
            "round {round}: {} frames",
            report.accepted.len()
        );
        assert_eq!(report.corrupt, 0);
    }
    assert!(sender.host.peer_stats().is_empty());
    assert_eq!(sender.host.connected(), 0);
}

// Scenario 5: a viewer that vanishes without a goodbye.
#[test]
fn the_host_drops_a_viewer_that_vanished() {
    let mut sender = Sender::new(host());
    let viewer = Viewer::join(&mut sender.host, None, false);
    sender.wait_connected(1);
    sender.run(Duration::from_secs(1), true);
    let peer = viewer.peer;
    viewer.finish(); // the session is dropped without closing
    let gone = Instant::now();
    while !sender.events.contains(&HostEvent::Disconnected(peer)) {
        assert!(gone.elapsed() < Duration::from_secs(30), "never noticed");
        sender.run(Duration::from_millis(100), true);
    }
    println!("vanished viewer dropped after {:?}", gone.elapsed());
    assert!(sender.host.peer_stats().is_empty());
}

// Scenario 5: the host goes away.
#[test]
fn a_viewer_notices_when_the_host_goes_away() {
    let mut sender = Sender::new(host());
    let viewer = Viewer::join(&mut sender.host, None, true);
    sender.wait_connected(1);
    sender.run(Duration::from_secs(1), true);
    drop(sender);
    let gone = Instant::now();
    while !viewer.ended.load(Ordering::Relaxed) {
        assert!(gone.elapsed() < Duration::from_secs(30), "never noticed");
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("host loss noticed after {:?}", gone.elapsed());
    viewer.finish();
}

// Scenario 6: the network drops out for 2 s and comes back.
#[test]
fn a_two_second_outage_recovers_with_a_keyframe() {
    let outage = ImpairmentConfig {
        outage: Some((Duration::from_secs(2), Duration::from_secs(2))),
        ..Default::default()
    };
    let mut sender = Sender::new(host());
    let joined = Instant::now();
    let viewer = Viewer::join(&mut sender.host, Some(outage), true);
    sender.wait_connected(1);
    let sent = sender.run(Duration::from_secs(7), true);
    sender.run(Duration::from_millis(500), false);
    let ended = viewer.ended.load(Ordering::Relaxed);
    let report = viewer.finish();
    summary("outage 2 s", &report, sent);
    assert!(!ended, "the session did not survive the outage");
    assert_eq!(report.corrupt, 0);
    let back = joined + Duration::from_secs(4);
    let first_after = report
        .accepted
        .iter()
        .find(|(_, at)| *at > back)
        .map(|(_, at)| at.saturating_duration_since(back))
        .expect("nothing after the outage");
    println!("first frame {first_after:?} after the network came back");
    assert!(first_after < Duration::from_millis(1500));
}

// Scenario 7: garbage datagrams to both sockets.
#[test]
fn garbage_datagrams_are_ignored() {
    let mut sender = Sender::new(host());
    let viewer = Viewer::join(&mut sender.host, None, true);
    sender.wait_connected(1);
    let garbage = UdpSocket::bind(localhost()).unwrap();
    let targets = [sender.host.local_addr(), viewer.addr];
    let mut seed = 0x1234_5678_u64;
    let mut sent = 0;
    for _ in 0..30 {
        // First bytes cover the STUN, DTLS and RTP/RTCP ranges.
        for i in 0..100_usize {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let len = 1 + (seed >> 33) as usize % 1400;
            let mut data: Vec<u8> = (0..len).map(|j| (seed >> (j % 56)) as u8).collect();
            data[0] = [0x00, 0x01, 0x16, 0x17, 0x80, 0x90, 0xC8, 0xFF][i % 8];
            let _ = garbage.send_to(&data, targets[i % 2]);
        }
        sent += sender.run(Duration::from_millis(100), true);
    }
    let report = viewer.finish();
    let share = summary("garbage", &report, sent);
    assert_eq!(report.corrupt, 0);
    assert!(share >= 0.95);
}

#[test]
fn random_loss_is_recovered() {
    // The share at 5 % varies a lot between runs (49–92 % measured alone,
    // less with the other tests running): it depends on whether a keyframe
    // loses a packet that retransmission cannot recover. The floor only
    // catches a collapse; the stream must still be flowing at the end.
    for (loss, floor) in [(0.01, 0.95), (0.05, 0.25)] {
        let config = ImpairmentConfig {
            loss,
            seed: 1,
            ..Default::default()
        };
        let (report, sent) = stream_through(Some(config), None, 6);
        let share = summary(&format!("loss {loss}"), &report, sent);
        assert_eq!(report.corrupt, 0);
        assert!(share >= floor, "loss {loss}: {share}");
        let last = report
            .accepted
            .iter()
            .filter(|(i, _)| *i >= sent - 60)
            .count();
        assert!(last >= 20, "loss {loss}: only {last} of the last 60 frames");
    }
}

#[test]
fn heavy_loss_keeps_the_session_alive_and_correct() {
    let config = ImpairmentConfig {
        loss: 0.10,
        seed: 2,
        ..Default::default()
    };
    let (report, sent) = stream_through(Some(config), None, 6);
    let share = summary("loss 0.1", &report, sent);
    assert_eq!(report.corrupt, 0);
    // 32–83 % across runs: only a collapse fails this.
    assert!(share >= 0.1, "{share}");
}

#[test]
fn loss_bursts_are_recovered() {
    let config = ImpairmentConfig {
        burst: Some((0.002, 40)),
        seed: 3,
        ..Default::default()
    };
    let (report, sent) = stream_through(Some(config), None, 6);
    let share = summary("bursts", &report, sent);
    println!("longest freeze {:?}", longest_freeze(&report));
    assert_eq!(report.corrupt, 0);
    assert!(share >= 0.85, "{share}");
}

#[test]
fn delay_jitter_and_reordering_keep_frames_whole_and_in_order() {
    let config = ImpairmentConfig {
        delay: 50 * MS,
        jitter: 20 * MS,
        reorder: 0.02,
        seed: 4,
        ..Default::default()
    };
    let (report, sent) = stream_through(Some(config), None, 6);
    let share = summary("delay 50 ± 20 ms, reorder 2 %", &report, sent);
    assert_eq!(report.corrupt, 0);
    assert!(share >= 0.95, "{share}");
    assert!(report.accepted.windows(2).all(|w| w[0].0 < w[1].0));
}

#[test]
fn lost_feedback_does_not_break_the_stream() {
    let feedback = ImpairmentConfig {
        loss: 0.05,
        seed: 5,
        ..Default::default()
    };
    let (report, sent) = stream_through(None, Some(feedback), 6);
    let share = summary("feedback loss 0.05", &report, sent);
    assert_eq!(report.corrupt, 0);
    assert!(share >= 0.95, "{share}");
}
