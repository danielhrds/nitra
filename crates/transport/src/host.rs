//! The host side: one UDP socket, one str0m `Rtc` per viewer, the same
//! access units written to every connected viewer (one encoder, N peers).
//! Each viewer has a [`SendBudget`]: a slow one skips frames (and resumes at
//! a keyframe) instead of queueing without bound or holding back the others.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};

use crate::budget::{Decision, SendBudget};
use crate::gather::{self, Gathered, NetworkConfig};
use crate::impair::{ImpairmentConfig, ImpairmentStats};
use crate::net::{MAX_WAIT, NetSocket, Waker, Woke};
use crate::{LinkStats, TransportError, rtc_config};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(pub u64);

#[derive(Clone, Copy, Debug)]
pub struct HostConfig {
    /// Where the congestion controller starts and how high it may probe.
    pub start_bitrate_bps: u64,
    pub max_bitrate_bps: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostEvent {
    Connected(PeerId),
    /// ICE lost the viewer, or it closed; the peer is gone.
    Disconnected(PeerId),
    /// The viewer asked for a keyframe (PLI/FIR).
    KeyframeRequested(PeerId),
    /// The viewer's link could not keep up: frames are skipped for it until
    /// a keyframe fits (the caller should ask the encoder for one).
    FellBehind(PeerId),
    /// New send-side bandwidth estimate for this viewer.
    BitrateEstimate {
        peer: PeerId,
        bps: u64,
    },
}

/// What the host has done for one viewer so far.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeerStats {
    pub id: PeerId,
    pub connected: bool,
    pub estimate_bps: u64,
    /// Waiting for a keyframe (new, or fell behind).
    pub waiting_keyframe: bool,
    pub frames_sent: u64,
    pub frames_skipped: u64,
    pub times_behind: u64,
    /// (local, remote) addresses the viewer's datagrams arrive on: the
    /// path ICE chose (LAN, public...).
    pub path: Option<(SocketAddr, SocketAddr)>,
    /// Loss this viewer reports for what we send, and the round trip.
    pub link: LinkStats,
}

struct Peer {
    id: PeerId,
    rtc: Rtc,
    /// Our offer (`invite`), until the viewer's answer is applied.
    pending: Option<SdpPendingOffer>,
    video_mid: Option<Mid>,
    audio_mid: Option<Mid>,
    timeout: Instant,
    connected: bool,
    budget: SendBudget,
    frames_sent: u64,
    frames_skipped: u64,
    times_behind: u64,
    /// (local, remote) addresses the last datagram from this viewer used.
    path: Option<(SocketAddr, SocketAddr)>,
    link: LinkStats,
}

pub struct HostSession {
    socket: NetSocket,
    config: HostConfig,
    peers: Vec<Peer>,
    next_id: u64,
    events: VecDeque<HostEvent>,
    buf: Vec<u8>,
    /// Offered to every viewer (gathered once, at bind).
    gathered: Gathered,
}

impl HostSession {
    /// One socket on `addr`, offered as the only (host) candidate.
    pub fn bind(addr: SocketAddr, config: HostConfig) -> Result<Self, TransportError> {
        let mut socket = NetSocket::bind(addr)?;
        let gathered = gather::gather(&mut socket, &[], &NetworkConfig::loopback());
        Ok(Self::with(socket, config, gathered))
    }

    /// Sockets on every address of the scope, with their candidates (and
    /// public ones from STUN for `Internet`; takes up to `STUN_TIMEOUT`).
    pub fn bind_network(
        network: &NetworkConfig,
        config: HostConfig,
    ) -> Result<Self, TransportError> {
        let addresses = gather::bind_addresses(network.scope)?;
        let addrs: Vec<SocketAddr> = addresses.iter().map(|a| SocketAddr::new(a.ip, 0)).collect();
        let mut socket = NetSocket::bind_all(&addrs)?;
        let gathered = gather::gather(&mut socket, &addresses, network);
        Ok(Self::with(socket, config, gathered))
    }

    fn with(socket: NetSocket, config: HostConfig, gathered: Gathered) -> Self {
        Self {
            socket,
            config,
            peers: Vec::new(),
            next_id: 1,
            events: VecDeque::new(),
            buf: vec![0; 2000],
            gathered,
        }
    }

    /// The candidates offered, the STUN answers and the NAT hint.
    pub fn gathered(&self) -> &Gathered {
        &self.gathered
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    /// Wakes `poll` from another thread (e.g. a new access unit is queued).
    pub fn waker(&self) -> Waker {
        self.socket.waker()
    }

    /// Testing: impairs what the host receives from every viewer (feedback:
    /// acknowledgements, retransmission and keyframe requests), from now on.
    pub fn set_impairment(&mut self, config: Option<ImpairmentConfig>) {
        self.socket.set_impairment(config);
    }

    pub fn impairment_stats(&self) -> Option<ImpairmentStats> {
        self.socket.impairment_stats()
    }

    /// How many times the network wait woke up so far.
    pub fn wakeups(&self) -> u64 {
        self.socket.wakeups()
    }

    pub fn connected(&self) -> usize {
        self.peers.iter().filter(|p| p.connected).count()
    }

    /// Accepts a viewer's offer; returns its id and the answer to send back.
    pub fn accept(&mut self, offer: &str) -> Result<(PeerId, String), TransportError> {
        let now = Instant::now();
        let mut rtc = self.new_rtc(now);
        let offer = SdpOffer::from_sdp_string(offer).map_err(TransportError::from_display)?;
        let answer = rtc.sdp_api().accept_offer(offer)?;
        let id = self.add_peer(rtc, None, (None, None), now);
        Ok((id, answer.to_sdp_string()))
    }

    /// The other way round (manual invites): makes an offer to send video
    /// and audio to a viewer not met yet. Apply its answer with
    /// [`accept_answer`](Self::accept_answer); until then the peer waits.
    pub fn invite(&mut self) -> Result<(PeerId, String), TransportError> {
        let now = Instant::now();
        let mut rtc = self.new_rtc(now);
        let mut change = rtc.sdp_api();
        let video = change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let audio = change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| TransportError("no offer".into()))?;
        let id = self.add_peer(rtc, Some(pending), (Some(video), Some(audio)), now);
        Ok((id, offer.to_sdp_string()))
    }

    /// Applies the viewer's answer to our [`invite`](Self::invite).
    pub fn accept_answer(&mut self, id: PeerId, answer: &str) -> Result<(), TransportError> {
        let peer = self
            .peers
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| TransportError("no such invite".into()))?;
        let pending = peer
            .pending
            .take()
            .ok_or_else(|| TransportError("answer already applied".into()))?;
        let answer = SdpAnswer::from_sdp_string(answer).map_err(TransportError::from_display)?;
        peer.rtc.sdp_api().accept_answer(pending, answer)?;
        drain(peer, &self.socket, &mut self.events);
        Ok(())
    }

    fn new_rtc(&self, now: Instant) -> Rtc {
        let start = Bitrate::from(self.config.start_bitrate_bps);
        let mut rtc = rtc_config().enable_bwe(Some(start)).build(now);
        for candidate in self.gathered.str0m_candidates() {
            rtc.add_local_candidate(candidate);
        }
        rtc.bwe()
            .set_bitrate(Bitrate::ZERO, Bitrate::from(self.config.max_bitrate_bps));
        rtc
    }

    fn add_peer(
        &mut self,
        rtc: Rtc,
        pending: Option<SdpPendingOffer>,
        (video_mid, audio_mid): (Option<Mid>, Option<Mid>),
        now: Instant,
    ) -> PeerId {
        let id = PeerId(self.next_id);
        self.next_id += 1;
        let mut peer = Peer {
            id,
            rtc,
            pending,
            video_mid,
            audio_mid,
            timeout: now,
            connected: false,
            budget: SendBudget::new(self.config.start_bitrate_bps, now),
            frames_sent: 0,
            frames_skipped: 0,
            times_behind: 0,
            path: None,
            link: LinkStats::default(),
        };
        drain(&mut peer, &self.socket, &mut self.events);
        self.peers.push(peer);
        id
    }

    /// Drops a viewer (it said goodbye, or it is gone).
    pub fn remove(&mut self, id: PeerId) {
        if let Some(index) = self.peers.iter().position(|p| p.id == id) {
            let mut peer = self.peers.swap_remove(index);
            peer.rtc.disconnect();
        }
    }

    pub fn peer_stats(&self) -> Vec<PeerStats> {
        self.peers
            .iter()
            .map(|p| PeerStats {
                id: p.id,
                connected: p.connected,
                estimate_bps: p.budget.estimate_bps(),
                waiting_keyframe: p.budget.waiting_keyframe(),
                frames_sent: p.frames_sent,
                frames_skipped: p.frames_skipped,
                times_behind: p.times_behind,
                path: p.path,
                link: p.link,
            })
            .collect()
    }

    /// Sends one access unit (Annex B) to every connected viewer whose
    /// budget allows it. `rtp_time` is its media time at 90 kHz.
    pub fn send_frame(
        &mut self,
        data: &[u8],
        rtp_time: u64,
        keyframe: bool,
    ) -> Result<(), TransportError> {
        let now = Instant::now();
        let data: std::sync::Arc<[u8]> = data.into();
        for peer in &mut self.peers {
            let (Some(mid), true) = (peer.video_mid, peer.connected) else {
                continue;
            };
            if let Decision::Skip { fell_behind } = peer.budget.offer(data.len(), keyframe, now) {
                peer.frames_skipped += 1;
                if fell_behind {
                    peer.times_behind += 1;
                    self.events.push_back(HostEvent::FellBehind(peer.id));
                }
                continue;
            }
            let Some(writer) = peer.rtc.writer(mid) else {
                continue;
            };
            let Some(pt) = writer.payload_params().next().map(|p| p.pt()) else {
                continue;
            };
            writer.write(pt, now, MediaTime::from_90khz(rtp_time), data.clone())?;
            peer.frames_sent += 1;
            drain(peer, &self.socket, &mut self.events);
        }
        Ok(())
    }

    /// Sends one Opus packet (20 ms) to every connected viewer that accepted
    /// audio. `rtp_time` is its media time at 48 kHz, on the same clock as
    /// the video's (so viewers can line them up). Audio is not subject to
    /// the video budget: it is small and must not stop when video skips.
    pub fn send_audio(&mut self, data: &[u8], rtp_time: u64) -> Result<(), TransportError> {
        let now = Instant::now();
        let data: std::sync::Arc<[u8]> = data.into();
        for peer in &mut self.peers {
            let (Some(mid), true) = (peer.audio_mid, peer.connected) else {
                continue;
            };
            let Some(writer) = peer.rtc.writer(mid) else {
                continue;
            };
            let Some(pt) = writer.payload_params().next().map(|p| p.pt()) else {
                continue;
            };
            let time = MediaTime::new(rtp_time, Frequency::FORTY_EIGHT_KHZ);
            writer.write(pt, now, time, data.clone())?;
            drain(peer, &self.socket, &mut self.events);
        }
        Ok(())
    }

    /// Runs the connections until there are events, the waker, or `deadline`:
    /// feeds packets and timeouts, sends what they produce, collects events.
    pub fn poll(&mut self, deadline: Instant) -> Result<(), TransportError> {
        loop {
            let now = Instant::now();
            for peer in &mut self.peers {
                if peer.timeout <= now {
                    peer.rtc.handle_input(Input::Timeout(now))?;
                    drain(peer, &self.socket, &mut self.events);
                }
            }
            self.forget_dead();
            if !self.events.is_empty() {
                return Ok(());
            }
            let next = self
                .peers
                .iter()
                .map(|p| p.timeout)
                .fold(deadline.min(now + MAX_WAIT), Instant::min);
            match self.socket.wait(&mut self.buf, next)? {
                Woke::Packet { n, from, to } => {
                    let now = Instant::now();
                    // Anyone can send datagrams here: what is not STUN, DTLS
                    // or RTP/RTCP is dropped, as are datagrams from unknown
                    // senders (stale peers, strays).
                    // (No `continue`: a flood must not keep `poll` past its deadline.)
                    let input = Receive::new(Protocol::Udp, from, to, &self.buf[..n])
                        .ok()
                        .map(|receive| Input::Receive(now, receive));
                    if let Some(input) = input
                        && let Some(peer) = self.peers.iter_mut().find(|p| p.rtc.accepts(&input))
                    {
                        // A malformed datagram from a peer costs that datagram,
                        // not the session.
                        peer.path = Some((to, from));
                        if let Err(error) = peer.rtc.handle_input(input) {
                            tracing::debug!(peer = peer.id.0, %error, "datagram rejected");
                        }
                        drain(peer, &self.socket, &mut self.events);
                    }
                }
                Woke::Waker => return Ok(()),
                Woke::Deadline => {}
            }
            // Hand events out as soon as there are any (a frame must not
            // wait for the deadline).
            if Instant::now() >= deadline || !self.events.is_empty() {
                return Ok(());
            }
        }
    }

    pub fn next_event(&mut self) -> Option<HostEvent> {
        self.events.pop_front()
    }

    fn forget_dead(&mut self) {
        let events = &mut self.events;
        self.peers.retain(|p| {
            let alive = p.rtc.is_alive();
            if !alive {
                events.push_back(HostEvent::Disconnected(p.id));
            }
            alive
        });
    }
}

/// Drains one `Rtc` to its next timeout (str0m's single-mutation rule).
fn drain(peer: &mut Peer, socket: &NetSocket, events: &mut VecDeque<HostEvent>) {
    loop {
        let output = match peer.rtc.poll_output() {
            Ok(output) => output,
            Err(error) => {
                tracing::warn!(peer = peer.id.0, %error, "peer failed; disconnecting");
                peer.rtc.disconnect();
                return;
            }
        };
        match output {
            Output::Timeout(t) => {
                peer.timeout = t;
                return;
            }
            Output::Transmit(t) => socket.send_to(&t.contents, t.source, t.destination),
            Output::Event(event) => match event {
                Event::Connected => {
                    peer.connected = true;
                    events.push_back(HostEvent::Connected(peer.id));
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    peer.rtc.disconnect();
                }
                Event::MediaAdded(added) => match added.kind {
                    MediaKind::Video => peer.video_mid = Some(added.mid),
                    MediaKind::Audio => peer.audio_mid = Some(added.mid),
                },
                Event::KeyframeRequest(_) => {
                    events.push_back(HostEvent::KeyframeRequested(peer.id))
                }
                Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                    peer.budget.set_estimate(estimate.as_u64(), Instant::now());
                    events.push_back(HostEvent::BitrateEstimate {
                        peer: peer.id,
                        bps: estimate.as_u64(),
                    });
                }
                Event::PeerStats(stats) => {
                    peer.link = LinkStats {
                        loss: stats.egress_loss_fraction.or(peer.link.loss),
                        rtt: stats.rtt.or(peer.link.rtt),
                        media_bytes: stats.bytes_tx,
                    };
                }
                _ => {}
            },
        }
    }
}
