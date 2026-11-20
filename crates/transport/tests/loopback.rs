//! Host and viewer sessions on 127.0.0.1 in one thread, driven alternately.

mod common;

use std::time::{Duration, Instant};

use common::{FPS, access_unit, host, is_keyframe, localhost};
use transport::{HostEvent, HostSession, VIDEO_CLOCK_HZ, ViewerEvent, ViewerSession};

struct Pair {
    host: HostSession,
    viewer: ViewerSession,
}

fn connect(host: &mut HostSession) -> (ViewerSession, transport::PeerId) {
    let (mut viewer, offer) = ViewerSession::offer(localhost()).unwrap();
    let (peer, answer) = host.accept(&offer).unwrap();
    viewer.accept_answer(&answer).unwrap();
    (viewer, peer)
}

fn step(host: &mut HostSession, viewer: &mut ViewerSession) {
    host.poll(Instant::now() + Duration::from_millis(1))
        .unwrap();
    viewer
        .poll(Instant::now() + Duration::from_millis(1))
        .unwrap();
}

fn wait_connected(pair: &mut Pair) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut host_ok, mut viewer_ok) = (false, false);
    while !(host_ok && viewer_ok) {
        assert!(Instant::now() < deadline, "no connection");
        step(&mut pair.host, &mut pair.viewer);
        while let Some(e) = pair.host.next_event() {
            host_ok |= matches!(e, HostEvent::Connected(_));
        }
        while let Some(e) = pair.viewer.next_event() {
            viewer_ok |= matches!(e, ViewerEvent::Connected);
        }
    }
}

/// Sends `count` frames from `first` at 60 fps; returns the indices received intact.
fn stream(pair: &mut Pair, first: u64, count: u64) -> Vec<u64> {
    let period = Duration::from_nanos(1_000_000_000 / FPS);
    let mut next = Instant::now();
    let mut received = Vec::new();
    let end = first + count;
    let mut index = first;
    let finish = Instant::now() + period * count as u32 + Duration::from_millis(500);
    while Instant::now() < finish {
        if index < end && Instant::now() >= next {
            pair.host
                .send_frame(
                    &access_unit(index),
                    index * VIDEO_CLOCK_HZ / FPS,
                    is_keyframe(index),
                )
                .unwrap();
            index += 1;
            next += period;
        }
        step(&mut pair.host, &mut pair.viewer);
        while let Some(event) = pair.viewer.next_event() {
            if let ViewerEvent::Frame(frame) = event {
                let id = frame.rtp_time * FPS / VIDEO_CLOCK_HZ;
                if *frame.data == *access_unit(id) {
                    received.push(id);
                }
            }
        }
        while pair.host.next_event().is_some() {}
    }
    received
}

#[test]
fn frames_arrive_intact() {
    let mut host = host();
    let (viewer, _) = connect(&mut host);
    let mut pair = Pair { host, viewer };
    wait_connected(&mut pair);
    let received = stream(&mut pair, 0, 120);
    assert!(
        received.len() >= 115,
        "only {} of 120 frames intact",
        received.len()
    );
}

#[test]
fn keyframe_request_reaches_the_host() {
    let mut host = host();
    let (viewer, peer) = connect(&mut host);
    let mut pair = Pair { host, viewer };
    wait_connected(&mut pair);
    stream(&mut pair, 0, 10);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !pair.viewer.request_keyframe() {
        assert!(Instant::now() < deadline, "keyframe request not possible");
        step(&mut pair.host, &mut pair.viewer);
    }
    let mut got = false;
    while !got {
        assert!(Instant::now() < deadline, "no keyframe request at the host");
        step(&mut pair.host, &mut pair.viewer);
        while let Some(e) = pair.host.next_event() {
            got |= e == HostEvent::KeyframeRequested(peer);
        }
    }
}

#[test]
fn a_viewer_can_leave_and_another_join() {
    let mut host = host();
    let (viewer, first) = connect(&mut host);
    let mut pair = Pair { host, viewer };
    wait_connected(&mut pair);
    assert!(stream(&mut pair, 0, 30).len() >= 28);
    pair.viewer.close();
    pair.host.remove(first);
    assert_eq!(pair.host.connected(), 0);

    let (viewer, second) = connect(&mut pair.host);
    assert_ne!(first, second);
    pair.viewer = viewer;
    wait_connected(&mut pair);
    assert!(stream(&mut pair, 30, 60).len() >= 57);
}

#[test]
fn a_frame_ends_the_viewer_poll_right_away() {
    let mut host = host();
    let (viewer, _) = connect(&mut host);
    let mut pair = Pair { host, viewer };
    wait_connected(&mut pair);
    // Drain whatever is pending, then send one frame and wait with a long deadline.
    stream(&mut pair, 0, 5);
    while pair.viewer.next_event().is_some() {}
    pair.host
        .send_frame(&access_unit(1), VIDEO_CLOCK_HZ / FPS, false)
        .unwrap();
    let started = Instant::now();
    let mut got = false;
    while !got && started.elapsed() < Duration::from_secs(1) {
        pair.host
            .poll(Instant::now() + Duration::from_millis(1))
            .unwrap();
        pair.viewer
            .poll(Instant::now() + Duration::from_secs(1))
            .unwrap();
        while let Some(event) = pair.viewer.next_event() {
            got |= matches!(event, ViewerEvent::Frame(_));
        }
    }
    assert!(got, "no frame");
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "frame held for {:?}",
        started.elapsed()
    );
}
