//! WebRTC spike: the shared scenarios on str0m (frame level API). One host
//! thread serves every viewer from one UDP socket; each viewer is a thread
//! with its own socket. Signaling is an in-process channel.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use clap::Parser;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::{Direction, KeyframeRequestKind, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use webrtc_harness::{
    Args, FRAME_PERIOD, HostReport, Link, Plan, Report, Source, Timeline, ViewerReport,
    ViewerStats, process_cpu_ms, thread_cpu_ms,
};

struct Join {
    offer: String,
    reply: Sender<String>,
}

fn config() -> str0m::RtcConfig {
    Rtc::builder()
        .clear_codecs()
        .enable_h264(true)
        .set_stats_interval(Some(Duration::from_secs(1)))
}

fn main() {
    let args = Args::parse();
    eprintln!("1 ms read timeout waits {:.2} ms", webrtc_harness::timeout_granularity_ms());
    if std::env::var_os("ZIZ_FINE_TIMERS").is_some() {
        webrtc_harness::fine_timers();
        eprintln!("with timeBeginPeriod(1): {:.2} ms", webrtc_harness::timeout_granularity_ms());
    }
    let plan = Plan::from_args(&args);
    let source = Source::open(&args.file);
    let units = source.units();
    let timeline = Arc::new(Timeline::default());
    let start = Instant::now() + Duration::from_millis(200);
    let end = start + Duration::from_secs(plan.seconds);
    let (join_tx, join_rx) = mpsc::channel();
    let process_start = process_cpu_ms();

    let host = {
        let (timeline, plan) = (Arc::clone(&timeline), plan.clone());
        std::thread::Builder::new()
            .name("host".into())
            .spawn(move || host(source, &plan, &timeline, join_rx, start, end))
            .expect("host thread")
    };
    let viewers: Vec<_> = plan
        .joins
        .iter()
        .enumerate()
        .map(|(index, &join)| {
            let (timeline, units, join_tx) = (Arc::clone(&timeline), Arc::clone(&units), join_tx.clone());
            std::thread::Builder::new()
                .name(format!("viewer-{index}"))
                .spawn(move || viewer(index, &units, &timeline, &join_tx, start, start + join, end))
                .expect("viewer thread")
        })
        .collect();
    drop(join_tx);
    let host = host.join().expect("host");
    let viewers = viewers.into_iter().map(|v| v.join().expect("viewer")).collect();
    Report {
        library: "str0m 0.25.0 (rust-crypto)",
        plan,
        host,
        viewers,
        process_cpu_ms: process_cpu_ms() - process_start,
    }
    .print_and_save(args.out.as_deref());
}

struct Peer {
    rtc: Rtc,
    mid: Option<Mid>,
    timeout: Instant,
}

#[derive(Default)]
struct HostEvents {
    keyframe_requests: u64,
    estimates: Vec<(f64, u64)>,
    egress_stats: Vec<String>,
}

/// Drains one Rtc to its next timeout (the single-mutation invariant).
fn drain_host(peer: &mut Peer, link: &mut Link, events: &mut HostEvents, start: Instant) {
    loop {
        match peer.rtc.poll_output().expect("poll_output") {
            Output::Timeout(t) => {
                peer.timeout = t;
                return;
            }
            Output::Transmit(t) => link.send(Instant::now(), t.destination, &t.contents),
            Output::Event(event) => match event {
                Event::MediaAdded(added) => peer.mid = Some(added.mid),
                Event::KeyframeRequest(_) => events.keyframe_requests += 1,
                Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => events
                    .estimates
                    .push(((Instant::now() - start).as_secs_f64(), estimate.as_u64())),
                Event::MediaEgressStats(stats) => events.egress_stats.push(format!("{stats:?}")),
                _ => {}
            },
        }
    }
}

fn host(
    mut source: Source,
    plan: &Plan,
    timeline: &Timeline,
    joins: Receiver<Join>,
    start: Instant,
    end: Instant,
) -> HostReport {
    let mut link = Link::bind(plan.impairment);
    let mut peers: Vec<Peer> = Vec::new();
    let mut events = HostEvents::default();
    let mut buf = vec![0_u8; 2000];
    let mut next_frame = start;
    let mut frames_sent = 0;
    let cpu_start = thread_cpu_ms();
    let mut handled_requests = 0;
    while Instant::now() < end {
        while let Ok(join) = joins.try_recv() {
            let bwe = std::env::var_os("ZIZ_NO_BWE").is_none();
            let config = if bwe { config().enable_bwe(Some(Bitrate::kbps(12_000))) } else { config() };
            let mut rtc = config.build(Instant::now());
            rtc.add_local_candidate(Candidate::host(link.local, "udp").expect("candidate"));
            if bwe {
                rtc.bwe().set_bitrate(Bitrate::ZERO, Bitrate::kbps(12_000));
            }
            let offer = SdpOffer::from_sdp_string(&join.offer).expect("offer");
            let answer = rtc.sdp_api().accept_offer(offer).expect("accept offer");
            let mut peer = Peer { rtc, mid: None, timeout: Instant::now() };
            drain_host(&mut peer, &mut link, &mut events, start);
            let _ = join.reply.send(answer.to_sdp_string());
            peers.push(peer);
        }
        let now = Instant::now();
        if now >= next_frame {
            // A keyframe request from any viewer makes the "encoder" send one.
            if events.keyframe_requests > handled_requests {
                handled_requests = events.keyframe_requests;
                source.request_keyframe();
            }
            let (unit, frame) = source.next();
            timeline.sent(frame.id, unit, frame.keyframe, now);
            frames_sent += 1;
            for peer in &mut peers {
                let Some(mid) = peer.mid else { continue };
                if !peer.rtc.is_connected() {
                    continue;
                }
                let Some(writer) = peer.rtc.writer(mid) else { continue };
                let pt = writer.payload_params().next().expect("negotiated H.264").pt();
                writer
                    .write(pt, now, MediaTime::from_90khz(frame.rtp_time), frame.data)
                    .expect("write");
                drain_host(peer, &mut link, &mut events, start);
            }
            next_frame += FRAME_PERIOD;
        }
        let deadline = peers.iter().map(|p| p.timeout).fold(next_frame, Instant::min).min(end);
        if let Some((n, from)) = link.recv(&mut buf, deadline) {
            let now = Instant::now();
            let input = Input::Receive(
                now,
                Receive::new(Protocol::Udp, from, link.local, &buf[..n]).expect("receive"),
            );
            if let Some(peer) = peers.iter_mut().find(|p| p.rtc.accepts(&input)) {
                peer.rtc.handle_input(input).expect("handle_input");
                drain_host(peer, &mut link, &mut events, start);
            }
        }
        let now = Instant::now();
        for peer in &mut peers {
            if now >= peer.timeout {
                peer.rtc.handle_input(Input::Timeout(now)).expect("timeout");
                drain_host(peer, &mut link, &mut events, start);
            }
        }
    }
    HostReport {
        frames_sent,
        keyframe_requests_received: events.keyframe_requests,
        cpu_ms: thread_cpu_ms() - cpu_start,
        link: link.counters,
        bandwidth_estimates: events.estimates,
        library_stats: serde_json::json!({ "last_egress_stats": events.egress_stats.last() }),
    }
}

fn viewer(
    index: usize,
    units: &[Vec<u8>],
    timeline: &Timeline,
    joins: &Sender<Join>,
    start: Instant,
    join_at: Instant,
    end: Instant,
) -> ViewerReport {
    std::thread::sleep(join_at.saturating_duration_since(Instant::now()));
    let cpu_start = thread_cpu_ms();
    let joined = Instant::now();
    let mut stats = ViewerStats::new(index, start, joined);
    let mut link = Link::bind(Default::default());
    let mut rtc = config().build(joined);
    rtc.add_local_candidate(Candidate::host(link.local, "udp").expect("candidate"));
    let mut change = rtc.sdp_api();
    let mid = change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    let (offer, pending) = change.apply().expect("offer");
    let (reply, answer) = mpsc::channel();
    joins.send(Join { offer: offer.to_sdp_string(), reply }).expect("host gone");
    let answer = SdpAnswer::from_sdp_string(&answer.recv().expect("answer")).expect("answer sdp");
    rtc.sdp_api().accept_answer(pending, answer).expect("accept answer");

    let mut buf = vec![0_u8; 2000];
    let mut keyframe_seen = false;
    let mut last_request: Option<Instant> = None;
    let mut ingress_stats = String::new();
    let mut peer_stats = String::new();
    let local: SocketAddr = link.local;
    while Instant::now() < end {
        let timeout = loop {
            match rtc.poll_output().expect("poll_output") {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => link.send(Instant::now(), t.destination, &t.contents),
                Output::Event(event) => match event {
                    Event::Connected => stats.connected(Instant::now()),
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {}
                    Event::MediaData(data) => {
                        let now = Instant::now();
                        let before = stats.report.first_keyframe_after_ms.is_some();
                        stats.frame(now, data.time.rebase(str0m::media::Frequency::NINETY_KHZ).numer(), &data.data, units, timeline);
                        keyframe_seen |= before || stats.report.first_keyframe_after_ms.is_some();
                        // Like a decoder without a reference picture: ask for a keyframe.
                        if !keyframe_seen && last_request.is_none_or(|t| now - t > Duration::from_millis(300)) {
                            if let Some(mut writer) = rtc.writer(mid) {
                                if writer.is_request_keyframe_possible(KeyframeRequestKind::Pli) {
                                    let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
                                    stats.report.keyframe_requests_sent += 1;
                                    last_request = Some(now);
                                }
                            }
                        }
                    }
                    Event::MediaIngressStats(s) => ingress_stats = format!("{s:?}"),
                    Event::PeerStats(s) => peer_stats = format!("{s:?}"),
                    _ => {}
                },
            }
        };
        let input = match link.recv(&mut buf, timeout.min(end)) {
            Some((n, from)) => Input::Receive(
                Instant::now(),
                Receive::new(Protocol::Udp, from, local, &buf[..n]).expect("receive"),
            ),
            None => Input::Timeout(Instant::now()),
        };
        rtc.handle_input(input).expect("handle_input");
    }
    stats.finish(
        timeline,
        thread_cpu_ms() - cpu_start,
        serde_json::json!({ "last_ingress_stats": ingress_stats, "last_peer_stats": peer_stats }),
    )
}
