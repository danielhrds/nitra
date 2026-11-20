//! The host side: one UDP socket, one str0m `Rtc` per viewer, the same
//! access units written to every connected viewer (one encoder, N peers).
//! Each viewer has a [`SendBudget`]: a slow one skips frames (and resumes at
//! a keyframe) instead of queueing without bound or holding back the others.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::media::{MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::budget::{Decision, SendBudget};
use crate::net::{MAX_WAIT, NetSocket, Waker, Woke};
use crate::{TransportError, rtc_config};

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerStats {
    pub id: PeerId,
    pub connected: bool,
    pub estimate_bps: u64,
    /// Waiting for a keyframe (new, or fell behind).
    pub waiting_keyframe: bool,
    pub frames_sent: u64,
    pub frames_skipped: u64,
    pub times_behind: u64,
}

struct Peer {
    id: PeerId,
    rtc: Rtc,
    mid: Option<Mid>,
    timeout: Instant,
    connected: bool,
    budget: SendBudget,
    frames_sent: u64,
    frames_skipped: u64,
    times_behind: u64,
}

pub struct HostSession {
    socket: NetSocket,
    config: HostConfig,
    peers: Vec<Peer>,
    next_id: u64,
    events: VecDeque<HostEvent>,
    buf: Vec<u8>,
}

impl HostSession {
    pub fn bind(addr: SocketAddr, config: HostConfig) -> Result<Self, TransportError> {
        Ok(Self {
            socket: NetSocket::bind(addr)?,
            config,
            peers: Vec::new(),
            next_id: 1,
            events: VecDeque::new(),
            buf: vec![0; 2000],
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    /// Wakes `poll` from another thread (e.g. a new access unit is queued).
    pub fn waker(&self) -> Waker {
        self.socket.waker()
    }

    pub fn connected(&self) -> usize {
        self.peers.iter().filter(|p| p.connected).count()
    }

    /// Accepts a viewer's offer; returns its id and the answer to send back.
    pub fn accept(&mut self, offer: &str) -> Result<(PeerId, String), TransportError> {
        let now = Instant::now();
        let start = Bitrate::from(self.config.start_bitrate_bps);
        let mut rtc = rtc_config().enable_bwe(Some(start)).build(now);
        rtc.add_local_candidate(
            Candidate::host(self.socket.local_addr(), "udp")
                .map_err(TransportError::from_display)?,
        );
        rtc.bwe()
            .set_bitrate(Bitrate::ZERO, Bitrate::from(self.config.max_bitrate_bps));
        let offer = SdpOffer::from_sdp_string(offer).map_err(TransportError::from_display)?;
        let answer = rtc.sdp_api().accept_offer(offer)?;
        let id = PeerId(self.next_id);
        self.next_id += 1;
        let mut peer = Peer {
            id,
            rtc,
            mid: None,
            timeout: now,
            connected: false,
            budget: SendBudget::new(self.config.start_bitrate_bps, now),
            frames_sent: 0,
            frames_skipped: 0,
            times_behind: 0,
        };
        drain(&mut peer, &self.socket, &mut self.events);
        self.peers.push(peer);
        Ok((id, answer.to_sdp_string()))
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
            let (Some(mid), true) = (peer.mid, peer.connected) else {
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
                Woke::Packet(n, from) => {
                    let now = Instant::now();
                    // Anyone can send datagrams here: what is not STUN, DTLS
                    // or RTP/RTCP is dropped, as are datagrams from unknown
                    // senders (stale peers, strays).
                    // (No `continue`: a flood must not keep `poll` past its deadline.)
                    let input = Receive::new(
                        Protocol::Udp,
                        from,
                        self.socket.local_addr(),
                        &self.buf[..n],
                    )
                    .ok()
                    .map(|receive| Input::Receive(now, receive));
                    if let Some(input) = input
                        && let Some(peer) = self.peers.iter_mut().find(|p| p.rtc.accepts(&input))
                    {
                        // A malformed datagram from a peer costs that datagram,
                        // not the session.
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
            Output::Transmit(t) => socket.send_to(&t.contents, t.destination),
            Output::Event(event) => match event {
                Event::Connected => {
                    peer.connected = true;
                    events.push_back(HostEvent::Connected(peer.id));
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    peer.rtc.disconnect();
                }
                Event::MediaAdded(added) => peer.mid = Some(added.mid),
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
                _ => {}
            },
        }
    }
}
