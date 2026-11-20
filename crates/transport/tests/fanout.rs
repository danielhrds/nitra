//! One host, several viewers on 127.0.0.1: every viewer gets the same
//! stream, a late one starts at a keyframe, a slow one is isolated, and
//! there is no fixed cap. The host runs on the test thread; each viewer has
//! its own thread, as it would have its own process.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::{FPS, access_unit, host, is_keyframe, localhost};
use transport::{
    HostEvent, HostSession, PeerId, PeerStats, VIDEO_CLOCK_HZ, ViewerEvent, ViewerSession,
};

struct Viewer {
    peer: PeerId,
    stop: Arc<AtomicBool>,
    /// Intact frames received and datagrams dropped by the receive limit.
    thread: JoinHandle<(Vec<u64>, u64)>,
}

struct Room {
    host: HostSession,
    viewers: Vec<Viewer>,
    host_events: Vec<HostEvent>,
}

impl Room {
    fn new() -> Self {
        Self {
            host: host(),
            viewers: Vec::new(),
            host_events: Vec::new(),
        }
    }

    fn join(&mut self, receive_limit_bps: Option<u64>) -> usize {
        let (mut viewer, offer) = ViewerSession::offer(localhost()).unwrap();
        viewer.set_receive_limit(receive_limit_bps);
        let (peer, answer) = self.host.accept(&offer).unwrap();
        viewer.accept_answer(&answer).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut received = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    viewer
                        .poll(Instant::now() + Duration::from_millis(5))
                        .unwrap();
                    while let Some(event) = viewer.next_event() {
                        if let ViewerEvent::Frame(frame) = event {
                            let id = frame.rtp_time * FPS / VIDEO_CLOCK_HZ;
                            if *frame.data == *access_unit(id) {
                                received.push(id);
                            }
                        }
                    }
                }
                (received, viewer.limit_dropped())
            })
        };
        self.viewers.push(Viewer { peer, stop, thread });
        self.viewers.len() - 1
    }

    fn step(&mut self, deadline: Instant) {
        self.host.poll(deadline).unwrap();
        while let Some(event) = self.host.next_event() {
            self.host_events.push(event);
        }
    }

    fn wait_connected(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.host.connected() < self.viewers.len() {
            assert!(Instant::now() < deadline, "not every viewer connected");
            self.step(Instant::now() + Duration::from_millis(5));
        }
    }

    /// Sends frames `range` at 60 fps, then lets the last ones arrive.
    fn stream(&mut self, range: std::ops::Range<u64>) {
        let period = Duration::from_nanos(1_000_000_000 / FPS);
        let mut next = Instant::now();
        for index in range {
            self.host
                .send_frame(
                    &access_unit(index),
                    index * VIDEO_CLOCK_HZ / FPS,
                    is_keyframe(index),
                )
                .unwrap();
            next += period;
            while Instant::now() < next {
                self.step(next);
            }
        }
        let settle = Instant::now() + Duration::from_millis(500);
        while Instant::now() < settle {
            self.step(settle);
        }
    }

    fn stats(&self, peer: PeerId) -> PeerStats {
        self.host
            .peer_stats()
            .into_iter()
            .find(|s| s.id == peer)
            .unwrap()
    }

    /// Stops the viewers; returns what each received and dropped.
    fn finish(self) -> Vec<(Vec<u64>, u64)> {
        for viewer in &self.viewers {
            viewer.stop.store(true, Ordering::Relaxed);
        }
        self.viewers
            .into_iter()
            .map(|v| v.thread.join().unwrap())
            .collect()
    }
}

fn count_in(received: &[u64], range: std::ops::Range<u64>) -> usize {
    received.iter().filter(|id| range.contains(id)).count()
}

#[test]
fn every_viewer_gets_the_same_stream_with_no_fixed_cap() {
    let mut room = Room::new();
    for _ in 0..8 {
        room.join(None);
    }
    room.wait_connected();
    room.stream(0..120);
    let stats = room.host.peer_stats();
    for (viewer, (received, _)) in room.finish().iter().enumerate() {
        let got = count_in(received, 0..120);
        assert!(
            got >= 114,
            "viewer {viewer}: {got} of 120 frames; {stats:?}"
        );
    }
    for s in stats {
        assert_eq!(s.times_behind, 0, "{s:?}");
    }
}

#[test]
fn a_late_viewer_starts_at_the_next_keyframe() {
    let mut room = Room::new();
    room.join(None);
    room.wait_connected();
    room.stream(0..40);
    room.join(None);
    room.wait_connected();
    room.stream(40..120);
    let results = room.finish();
    // Joined mid-GOP: nothing until keyframe 60, then everything.
    let late = &results[1].0;
    assert_eq!(late.first().copied(), Some(60), "late viewer: {late:?}");
    assert!(count_in(late, 60..120) >= 57);
    assert!(count_in(&results[0].0, 0..120) >= 114);
}

#[test]
fn a_slow_viewer_does_not_hold_back_the_others() {
    let mut room = Room::new();
    room.join(None);
    room.join(None);
    // ~7 Mbps stream into a 1.5 Mbps link.
    let slow = room.join(Some(1_500_000));
    room.wait_connected();
    room.stream(0..480);

    let slow_peer = room.viewers[slow].peer;
    let stats = room.stats(slow_peer);
    let fell_behind = room.host_events.contains(&HostEvent::FellBehind(slow_peer));
    let results = room.finish();
    for (viewer, (received, _)) in results.iter().enumerate().take(2) {
        let got = count_in(received, 0..480);
        assert!(got >= 456, "fast viewer {viewer}: {got} of 480 frames");
    }
    assert!(
        stats.frames_skipped > 0,
        "slow viewer never skipped: {stats:?}"
    );
    assert!(fell_behind, "no FellBehind for the slow viewer");
    println!(
        "slow viewer: {stats:?}, {} intact, {} datagrams dropped by the limit",
        results[slow].0.len(),
        results[slow].1
    );
}
