//! WebRTC spike: the shared scenarios on rtc (webrtc-rs sans-IO core, RTP level
//! API). One host thread serves every viewer from one UDP socket; each
//! viewer is a thread with its own socket. Signaling is an in-process channel.
//! The application packetizes H.264 itself (rtc's packetizer) and rebuilds
//! frames on the viewer (rtc's sample builder).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use clap::Parser;
use rtc::interceptor::{BandwidthEstimator, EstimatorStats, Gcc, PacketReport, Registry};
use rtc::interceptor::{Attribute, Interceptor, Packet, Slot, StreamInfo, TaggedPacket};
use rtc::rtp::packetizer::Depacketizer;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::interceptor_registry::{
    CongestionFeedback, configure_congestion_control, register_default_interceptors,
};
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::peer_connection::event::{RTCPeerConnectionEvent, RTCTrackEvent};
use rtc::peer_connection::message::{RTCMessage, TaggedRTCMessage};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{CandidateConfig, CandidateHostConfig, RTCIceCandidate};
use rtc::peer_connection::{RTCPeerConnection, RTCPeerConnectionBuilder};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Packetizer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpReceiverId, RTCRtpSenderId, RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use webrtc_harness::{
    Args, FRAME_PERIOD, HostReport, Link, Plan, Report, Source, Timeline, ViewerReport,
    ViewerStats, process_cpu_ms, thread_cpu_ms,
};

const MTU: usize = 1200;

/// Passes inbound keyframe requests (PLI, FIR) to the application; rtc
/// delivers no other inbound RTCP to it (the pattern of its rtp-forwarder
/// example).
#[derive(Default)]
struct KeyframeRequestForwarder {
    reads: std::collections::VecDeque<TaggedPacket>,
    writes: std::collections::VecDeque<TaggedPacket>,
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequestForwarder {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = rtc::shared::error::Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            if std::env::var_os("ZIZ_DEBUG").is_some() {
                for p in packets { if p.as_any().is::<rtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack>() { eprintln!("NACK at host"); } }
            }
            let requests: Vec<Box<dyn rtc::rtcp::Packet>> = packets
                .iter()
                .filter(|p| {
                    let any = p.as_any();
                    any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
                })
                .cloned()
                .collect();
            if requests.is_empty() {
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(requests);
            msg.message.add(Attribute::DeliverToApplication);
        }
        self.reads.push_back(msg);
        Ok(())
    }
    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.reads.pop_front()
    }
    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.writes.push_back(msg);
        Ok(())
    }
    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.writes.pop_front()
    }
}

impl Interceptor for KeyframeRequestForwarder {
    fn bind_local_stream(&mut self, _: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _: &StreamInfo) {}
}

/// Rebuilds H.264 frames from RTP as soon as the marker packet completes
/// one (rtc's SampleBuilder waits for the next frame's first packet: one
/// frame of extra latency). Reorders by sequence number (NACK retransmissions
/// arrive late); a frame is complete when every sequence number from the
/// previous marker to its marker is there.
#[derive(Default)]
struct FrameAssembler {
    /// Sequence number after the last completed (or skipped) frame.
    next_seq: Option<u16>,
    pending: std::collections::BTreeMap<u16, rtc::rtp::Packet>,
}

impl FrameAssembler {
    fn push(&mut self, packet: rtc::rtp::Packet) -> Option<(u32, Vec<u8>)> {
        let seq = packet.header.sequence_number;
        if std::env::var_os("ZIZ_DEBUG").is_some() {
            if let Some(next) = self.next_seq { if seq.wrapping_sub(next) > 0x8000 || self.pending.keys().next_back().is_some_and(|&last| seq.wrapping_sub(last) > 0x8000) { eprintln!("LATE seq {seq} next {next}"); } }
        }
        let Some(next) = self.next_seq else {
            // Join mid-stream: start after the first marker seen.
            if packet.header.marker {
                self.next_seq = Some(seq.wrapping_add(1));
            }
            return None;
        };
        if seq.wrapping_sub(next) > 0x8000 {
            return None; // older than what is done
        }
        self.pending.insert(seq, packet);
        if self.pending.len() > 1000 {
            // A gap that retransmission did not fill (well past a keyframe): give
            // up on it and resume after the first frame end past the gap.
            let after_gap = self.pending.iter().find(|(_, p)| p.header.marker).map(|(&s, _)| s);
            if let Some(marker) = after_gap {
                self.pending.retain(|&s, _| s.wrapping_sub(marker) < 0x8000 && s != marker);
                self.next_seq = Some(marker.wrapping_add(1));
            }
        }
        // Complete frame starting at `next`?
        let mut s = next;
        let mut end = None;
        while let Some(p) = self.pending.get(&s) {
            if p.header.marker {
                end = Some(s);
                break;
            }
            s = s.wrapping_add(1);
        }
        let end = end?;
        let mut depacketizer = H264Packet::default();
        let mut data = Vec::new();
        let mut timestamp;
        let mut s = next;
        loop {
            let p = self.pending.remove(&s).expect("contiguous");
            timestamp = p.header.timestamp;
            if let Ok(nal) = depacketizer.depacketize(&p.payload) {
                data.extend_from_slice(&nal);
            }
            if s == end {
                break;
            }
            s = s.wrapping_add(1);
        }
        self.next_seq = Some(end.wrapping_add(1));
        Some((timestamp, data))
    }
}

struct Join {
    offer: String,
    reply: Sender<String>,
}

fn h264() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640033"
                .to_owned(),
            // The NACK responder only buffers streams whose codec lists "nack".
            rtcp_feedback: [("nack", ""), ("nack", "pli"), ("transport-cc", "")]
                .into_iter()
                .map(|(typ, parameter)| RTCPFeedback { typ: typ.to_owned(), parameter: parameter.to_owned() })
                .collect(),
        },
        payload_type: 102,
        ..Default::default()
    }
}

/// Publishes every new estimate (the estimator is the only place that sees it).
struct ReportingEstimator {
    inner: Gcc,
    start: Instant,
    estimates: Arc<Mutex<Vec<(f64, u64)>>>,
}

impl ReportingEstimator {
    fn publish(&self, now: Instant) {
        let value = self.inner.target_bitrate() as u64;
        let mut estimates = self.estimates.lock().unwrap();
        if estimates.last().is_none_or(|&(_, last)| last != value) {
            estimates.push(((now - self.start).as_secs_f64(), value));
        }
    }
}

impl BandwidthEstimator for ReportingEstimator {
    fn on_reports(&mut self, now: Instant, reports: &[PacketReport]) {
        self.inner.on_reports(now, reports);
        self.publish(now);
    }
    fn target_bitrate(&self) -> f64 {
        self.inner.target_bitrate()
    }
    fn handle_timeout(&mut self, now: Instant) {
        self.inner.handle_timeout(now);
        self.publish(now);
    }
    fn poll_timeout(&self) -> Option<Instant> {
        self.inner.poll_timeout()
    }
    fn stats(&self) -> EstimatorStats {
        self.inner.stats()
    }
}

fn candidate(pc: &mut RTCPeerConnection, local: SocketAddr) {
    let host = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: local.ip().to_string(),
            port: local.port(),
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()
    .expect("candidate");
    pc.add_local_candidate(RTCIceCandidate::from(&host).to_json().expect("json"))
        .expect("add candidate");
}

fn tagged(local: SocketAddr, from: SocketAddr, data: &[u8]) -> TaggedBytesMut {
    TaggedBytesMut {
        now: Instant::now(),
        transport: TransportContext {
            local_addr: local,
            peer_addr: from,
            ecn: None,
            transport_protocol: TransportProtocol::UDP,
        },
        message: BytesMut::from(data),
    }
}

fn main() {
    let args = Args::parse();
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
        library: "rtc 0.21.1 (ring)",
        plan,
        host,
        viewers,
        process_cpu_ms: process_cpu_ms() - process_start,
    }
    .print_and_save(args.out.as_deref());
}

struct Peer {
    pc: RTCPeerConnection,
    sender: RTCRtpSenderId,
    packetizer: Box<dyn Packetizer>,
    connected: bool,
    remote: Option<SocketAddr>,
}

/// Sends what the connection queued and handles its events and messages.
fn drain_host(peer: &mut Peer, link: &mut Link, keyframe_requests: &mut u64) {
    while let Some(out) = peer.pc.poll_write() {
        peer.remote.get_or_insert(out.transport.peer_addr);
        link.send(Instant::now(), out.transport.peer_addr, &out.message);
    }
    while let Some(event) = peer.pc.poll_event() {
        if let RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) = event {
            peer.connected = state == RTCPeerConnectionState::Connected;
        }
    }
    while let Some(TaggedRTCMessage { message, .. }) = peer.pc.poll_read() {
        if let RTCMessage::RtcpPacket(_, packets) = message {
            *keyframe_requests += packets
                .iter()
                .filter(|p| {
                    let any = p.as_any();
                    any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
                })
                .count() as u64;
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
    let estimates = Arc::new(Mutex::new(Vec::new()));
    let mut keyframe_requests = 0;
    let mut handled_requests = 0;
    let mut buf = vec![0_u8; 2000];
    let mut next_frame = start;
    let mut frames_sent = 0;
    let cpu_start = thread_cpu_ms();
    let codec = h264();
    while Instant::now() < end {
        while let Ok(join) = joins.try_recv() {
            let mut media_engine = MediaEngine::default();
            media_engine.register_codec(codec.clone(), RtpCodecKind::Video).expect("codec");
            let estimator = ReportingEstimator {
                inner: Gcc::new(12_000_000.0, 300_000.0, 20_000_000.0),
                start,
                estimates: Arc::clone(&estimates),
            };
            let registry = if std::env::var_os("ZIZ_NO_BWE").is_some() { Registry::new() } else { configure_congestion_control(
                Registry::new(),
                estimator,
                CongestionFeedback::Twcc,
                &mut media_engine,
            )
            .expect("congestion control") };
            let registry = register_default_interceptors(registry, &mut media_engine).expect("interceptors");
            // Inbound RTCP stops at the interceptors; keyframe requests must reach us.
            let registry = registry.with(Slot::from(11_500), KeyframeRequestForwarder::default());
            let mut pc = RTCPeerConnectionBuilder::new()
                .with_configuration(RTCConfigurationBuilder::new().build())
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .build(Instant::now())
                .expect("peer connection");
            let ssrc: u32 = 0x5EED_0000 + peers.len() as u32;
            let track = MediaStreamTrack::new(
                "ziz".to_owned(),
                format!("video-{}", peers.len()),
                "video".to_owned(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                    codec: codec.rtp_codec.clone(),
                    ..Default::default()
                }],
            );
            let sender = pc.add_track(track).expect("add track");
            let offer = RTCSessionDescription::offer(join.offer).expect("offer");
            pc.set_remote_description(Instant::now(), offer).expect("remote description");
            candidate(&mut pc, link.local);
            let answer = pc.create_answer(None).expect("answer");
            pc.set_local_description(Instant::now(), answer).expect("local description");
            let sdp = pc.local_description().expect("local description").sdp.clone();
            let packetizer = rtc::rtp::packetizer::new_packetizer(
                Instant::now(),
                MTU,
                codec.payload_type,
                ssrc,
                codec.rtp_codec.payloader().expect("payloader"),
                Box::new(rtc::rtp::sequence::new_random_sequencer()),
                90_000,
            );
            let mut peer = Peer { pc, sender, packetizer: Box::new(packetizer), connected: false, remote: None };
            drain_host(&mut peer, &mut link, &mut keyframe_requests);
            if std::env::var_os("ZIZ_DEBUG").is_some() {
                eprintln!("ANSWER:
{}", sdp.lines().filter(|l| l.contains("rtcp-fb") || l.contains("rtpmap") || l.contains("fmtp")).collect::<Vec<_>>().join("
"));
            }
            let _ = join.reply.send(sdp);
            peers.push(peer);
        }
        let now = Instant::now();
        if now >= next_frame {
            if keyframe_requests > handled_requests {
                handled_requests = keyframe_requests;
                source.request_keyframe();
            }
            let (unit, frame) = source.next();
            timeline.sent(frame.id, unit, frame.keyframe, now);
            frames_sent += 1;
            for peer in &mut peers {
                if !peer.connected {
                    continue;
                }
                let packets = peer
                    .packetizer
                    .packetize(now, &bytes::Bytes::copy_from_slice(frame.data), 1500)
                    .expect("packetize");
                let Some(mut sender) = peer.pc.rtp_sender(peer.sender) else { continue };
                let pt = sender
                    .get_parameters()
                    .rtp_parameters
                    .codecs
                    .first()
                    .map(|c| c.payload_type)
                    .expect("negotiated H.264");
                for mut packet in packets {
                    // Our own media time, so the viewer can name the frame.
                    packet.header.timestamp = frame.rtp_time as u32;
                    packet.header.payload_type = pt;
                    sender.write_rtp(now, packet).expect("write_rtp");
                }
                drain_host(peer, &mut link, &mut keyframe_requests);
            }
            next_frame += FRAME_PERIOD;
        }
        let deadline = peers
            .iter_mut()
            .filter_map(|p| p.pc.poll_timeout())
            .fold(next_frame, Instant::min)
            .min(end);
        if let Some((n, from)) = link.recv(&mut buf, deadline) {
            // Demultiplex by the viewer's address (the one each answer was sent to).
            let index = peers
                .iter()
                .position(|p| p.remote == Some(from))
                .or_else(|| peers.iter().position(|p| p.remote.is_none()));
            if let Some(i) = index {
                let peer = &mut peers[i];
                peer.remote = Some(from);
                peer.pc.handle_read(tagged(link.local, from, &buf[..n])).expect("handle_read");
                drain_host(peer, &mut link, &mut keyframe_requests);
            }
        }
        let now = Instant::now();
        for peer in &mut peers {
            if peer.pc.poll_timeout().is_some_and(|t| t <= now) {
                peer.pc.handle_timeout(now).expect("timeout");
                drain_host(peer, &mut link, &mut keyframe_requests);
            }
        }
    }
    let estimates = estimates.lock().unwrap().clone();
    HostReport {
        frames_sent,
        keyframe_requests_received: keyframe_requests,
        cpu_ms: thread_cpu_ms() - cpu_start,
        link: link.counters,
        bandwidth_estimates: estimates,
        library_stats: serde_json::Value::Null,
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
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(h264(), RtpCodecKind::Video).expect("codec");
    let registry = register_default_interceptors(Registry::new(), &mut media_engine).expect("interceptors");
    let mut pc = RTCPeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build(joined)
        .expect("peer connection");
    pc.add_transceiver_from_kind(
        RtpCodecKind::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            ..Default::default()
        }),
    )
    .expect("transceiver");
    candidate(&mut pc, link.local);
    let offer = pc.create_offer(None).expect("offer");
    pc.set_local_description(Instant::now(), offer.clone()).expect("local description");
    let (reply, answer) = mpsc::channel();
    joins.send(Join { offer: offer.sdp, reply }).expect("host gone");
    let answer = RTCSessionDescription::answer(answer.recv().expect("answer")).expect("answer sdp");
    pc.set_remote_description(Instant::now(), answer).expect("remote description");

    let mut buf = vec![0_u8; 2000];
    let mut assembler = FrameAssembler::default();
    let mut receivers: HashMap<String, RTCRtpReceiverId> = HashMap::new();
    let mut media_ssrc: Option<u32> = None;
    let mut receiver_id: Option<RTCRtpReceiverId> = None;
    let mut keyframe_seen = false;
    let mut last_request: Option<Instant> = None;
    while Instant::now() < end {
        while let Some(out) = pc.poll_write() {
            link.send(Instant::now(), out.transport.peer_addr, &out.message);
        }
        while let Some(event) = pc.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(RTCPeerConnectionState::Connected) => {
                    stats.connected(Instant::now());
                }
                RTCPeerConnectionEvent::OnTrack(RTCTrackEvent::OnOpen(init)) => {
                    receivers.insert(init.track_id.clone(), init.receiver_id);
                }
                _ => {}
            }
        }
        while let Some(TaggedRTCMessage { now, message }) = pc.poll_read() {
            let RTCMessage::RtpPacket(track_id, packet) = message else { continue };
            media_ssrc.get_or_insert(packet.header.ssrc);
            if receiver_id.is_none() {
                receiver_id = receivers.get(&track_id).copied();
            }
            let _ = now;
            if let Some((timestamp, data)) = assembler.push(packet) {
                let arrived = Instant::now();
                stats.frame(arrived, u64::from(timestamp), &data, units, timeline);
                keyframe_seen |= stats.report.first_keyframe_after_ms.is_some();
                // Like a decoder without a reference picture: ask for a keyframe.
                if !keyframe_seen && last_request.is_none_or(|t| arrived - t > Duration::from_millis(300)) {
                    if let (Some(id), Some(ssrc)) = (receiver_id, media_ssrc) {
                        if let Some(mut receiver) = pc.rtp_receiver(id) {
                            let pli = PictureLossIndication { sender_ssrc: 0, media_ssrc: ssrc };
                            if receiver.write_rtcp(arrived, vec![Box::new(pli)]).is_ok() {
                                stats.report.keyframe_requests_sent += 1;
                                last_request = Some(arrived);
                            }
                        }
                    }
                }
            }
        }
        let deadline = pc.poll_timeout().unwrap_or(end).min(end);
        match link.recv(&mut buf, deadline) {
            Some((n, from)) => pc.handle_read(tagged(link.local, from, &buf[..n])).expect("handle_read"),
            None => pc.handle_timeout(Instant::now()).expect("timeout"),
        }
    }
    stats.finish(timeline, thread_cpu_ms() - cpu_start, serde_json::Value::Null)
}
