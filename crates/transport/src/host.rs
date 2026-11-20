//! The host side: one UDP socket, one str0m `Rtc` per viewer, the same
//! access units written to every connected viewer (one encoder, N peers).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::media::{MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

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
    /// New send-side bandwidth estimate for this viewer.
    BitrateEstimate {
        peer: PeerId,
        bps: u64,
    },
}

struct Peer {
    id: PeerId,
    rtc: Rtc,
    mid: Option<Mid>,
    timeout: Instant,
    connected: bool,
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

    /// Sends one access unit (Annex B) to every connected viewer.
    /// `rtp_time` is its media time at 90 kHz.
    pub fn send_frame(&mut self, data: &[u8], rtp_time: u64) -> Result<(), TransportError> {
        let now = Instant::now();
        let data: std::sync::Arc<[u8]> = data.into();
        for peer in &mut self.peers {
            let (Some(mid), true) = (peer.mid, peer.connected) else {
                continue;
            };
            let Some(writer) = peer.rtc.writer(mid) else {
                continue;
            };
            let Some(pt) = writer.payload_params().next().map(|p| p.pt()) else {
                continue;
            };
            writer.write(pt, now, MediaTime::from_90khz(rtp_time), data.clone())?;
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
                    let receive = Receive::new(
                        Protocol::Udp,
                        from,
                        self.socket.local_addr(),
                        &self.buf[..n],
                    )
                    .map_err(TransportError::from_display)?;
                    let input = Input::Receive(now, receive);
                    // Unknown senders (stale peers, strays) are ignored.
                    if let Some(peer) = self.peers.iter_mut().find(|p| p.rtc.accepts(&input)) {
                        peer.rtc.handle_input(input)?;
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
