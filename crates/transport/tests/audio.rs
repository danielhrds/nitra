//! Audio alongside video, host on the test thread and the viewer on its
//! own: packets arrive intact with their media time on the video's clock,
//! and audio keeps flowing while video skips frames on a slow link.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{FPS, access_unit, host, is_keyframe, localhost};
use transport::impair::ImpairmentConfig;
use transport::{AUDIO_CLOCK_HZ, HostSession, VIDEO_CLOCK_HZ, ViewerEvent, ViewerSession};

/// A stand-in Opus packet for 20 ms block `index` (~320 bytes, 128 kbps).
fn packet(index: u64) -> Vec<u8> {
    (0..320).map(|i| (index as usize * 7 + i) as u8).collect()
}

#[derive(Default)]
struct Received {
    /// (block index, intact) for each audio packet.
    audio: Vec<(u64, bool)>,
    /// (block index, when it arrived).
    arrived: Vec<(u64, Instant)>,
    video: usize,
}

/// Streams `seconds` of 60 fps video and 50 audio packets per second, both
/// stamped from the same start; returns what the viewer got.
/// `feedback` impairs what the host receives (retransmission requests).
fn stream(
    impairment: Option<ImpairmentConfig>,
    feedback: Option<ImpairmentConfig>,
    seconds: u64,
) -> (Received, u64, u64, Instant) {
    let mut host: HostSession = host();
    host.set_impairment(feedback);
    let (mut viewer, offer) = ViewerSession::offer(localhost()).unwrap();
    viewer.set_impairment(impairment);
    let (_, answer) = host.accept(&offer).unwrap();
    viewer.accept_answer(&answer).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut got = Received::default();
            while !stop.load(Ordering::Relaxed) {
                viewer
                    .poll(Instant::now() + Duration::from_millis(5))
                    .unwrap();
                while let Some(event) = viewer.next_event() {
                    match event {
                        ViewerEvent::Audio(a) => {
                            let index = a.rtp_time / (AUDIO_CLOCK_HZ / 50);
                            got.audio.push((index, *a.data == *packet(index)));
                            got.arrived.push((index, a.received_at));
                        }
                        ViewerEvent::Frame(_) => got.video += 1,
                        _ => {}
                    }
                }
            }
            got
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.connected() < 1 {
        assert!(Instant::now() < deadline, "no connection");
        host.poll(Instant::now() + Duration::from_millis(5))
            .unwrap();
        while host.next_event().is_some() {}
    }
    let start = Instant::now();
    let (mut video, mut audio) = (0_u64, 0_u64);
    let end = start + Duration::from_secs(seconds);
    while Instant::now() < end {
        let elapsed = start.elapsed();
        // Both clocks count from `start`, as the app counts from one origin.
        while Duration::from_nanos(video * 1_000_000_000 / FPS) <= elapsed {
            host.send_frame(
                &access_unit(video),
                video * VIDEO_CLOCK_HZ / FPS,
                is_keyframe(video),
            )
            .unwrap();
            video += 1;
        }
        while Duration::from_millis(audio * 20) <= elapsed {
            host.send_audio(&packet(audio), audio * AUDIO_CLOCK_HZ / 50)
                .unwrap();
            audio += 1;
        }
        host.poll(Instant::now() + Duration::from_millis(2))
            .unwrap();
        while host.next_event().is_some() {}
    }
    let settle = Instant::now() + Duration::from_millis(500);
    while Instant::now() < settle {
        host.poll(settle).unwrap();
        while host.next_event().is_some() {}
    }
    stop.store(true, Ordering::Relaxed);
    (thread.join().unwrap(), video, audio, start)
}

#[test]
fn audio_arrives_intact_with_its_media_time() {
    let (got, video, audio, _) = stream(None, None, 3);
    assert!(
        got.audio.len() as u64 >= audio - 2,
        "{} of {audio} packets",
        got.audio.len()
    );
    assert!(
        got.audio.iter().all(|(_, intact)| *intact),
        "a packet was damaged"
    );
    assert!(
        got.audio.windows(2).all(|w| w[1].0 == w[0].0 + 1),
        "gaps or reordering"
    );
    assert!(got.video as u64 >= video - 3);
}

#[test]
fn audio_keeps_flowing_when_video_skips_on_a_slow_link() {
    // ~7 Mbps of video into 2 Mbps: the video budget skips frames, audio
    // (128 kbps) must keep coming.
    let slow = ImpairmentConfig {
        rate_bps: Some(2_000_000),
        queue: ImpairmentConfig::DEFAULT_QUEUE,
        ..Default::default()
    };
    let (got, video, audio, _) = stream(Some(slow), None, 6);
    let share = got.audio.len() as f64 / audio as f64;
    println!(
        "slow link: audio {}/{audio} ({:.0} %), video {}/{video}",
        got.audio.len(),
        share * 100.0,
        got.video
    );
    assert!(got.audio.iter().all(|(_, intact)| *intact));
    assert!((got.video as u64) < video / 2, "video was not limited");
    assert!(share >= 0.8, "audio starved: {share}");
}

#[test]
fn lost_audio_packets_do_not_hold_the_others_back() {
    // At 5 % loss, with retransmission requests lost too, audio still
    // arrives promptly: Opus conceals the gaps. (This does not reproduce
    // the app case where str0m's default 1 s audio wait, next to 10 Mbps of
    // video, delayed packets by ~300 ms; that was measured in the app.)
    let lossy = ImpairmentConfig {
        loss: 0.05,
        seed: 3,
        ..Default::default()
    };
    // Retransmission requests are lost too, so missing packets stay missing.
    let no_nacks = ImpairmentConfig {
        loss: 0.5,
        seed: 4,
        ..Default::default()
    };
    let (got, _, audio, start) = stream(Some(lossy), Some(no_nacks), 6);
    let mut delays: Vec<Duration> = got
        .arrived
        .iter()
        .map(|(index, at)| at.saturating_duration_since(start + Duration::from_millis(index * 20)))
        .collect();
    delays.sort();
    let p95 = delays[delays.len() * 95 / 100];
    let max = *delays.last().unwrap();
    println!(
        "5 % loss: audio {}/{audio}, delay p95 {p95:?}, max {max:?}",
        got.audio.len()
    );
    assert!(got.audio.iter().all(|(_, intact)| *intact));
    assert!(max < Duration::from_millis(200), "audio held back {max:?}");
}
