//! The viewer side: one str0m `Rtc` receiving one video stream as complete
//! frames (str0m depacketizes, reorders and recovers losses with NACK/RTX)
//! and one Opus audio stream as packets.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};

use crate::TransportError;
use crate::gather::{self, Gathered, NetworkConfig};
use crate::impair::{ImpairmentConfig, ImpairmentStats};
use crate::net::{MAX_WAIT, NetSocket, Waker, Woke};

/// One complete access unit (Annex B).
#[derive(Clone, Debug)]
pub struct Frame {
    pub data: std::sync::Arc<[u8]>,
    /// Media time at 90 kHz, as the host wrote it.
    pub rtp_time: u64,
    /// When the last packet of the frame arrived.
    pub received_at: Instant,
    /// False when frames were lost before this one: the decoder may lack a
    /// reference and should get a keyframe.
    pub contiguous: bool,
}

/// One Opus packet (20 ms of audio).
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub data: std::sync::Arc<[u8]>,
    /// Media time at 48 kHz, on the same clock as the video's.
    pub rtp_time: u64,
    pub received_at: Instant,
}

#[derive(Debug)]
pub enum ViewerEvent {
    Connected,
    /// ICE lost the host, or it closed.
    Disconnected,
    Frame(Frame),
    Audio(AudioPacket),
}

pub struct ViewerSession {
    socket: NetSocket,
    rtc: Rtc,
    /// Known once negotiated (our offer, or the media in the host's offer).
    mid: Option<Mid>,
    audio_mid: Option<Mid>,
    pending: Option<SdpPendingOffer>,
    timeout: Instant,
    events: VecDeque<ViewerEvent>,
    buf: Vec<u8>,
    gathered: Gathered,
    path: Option<(SocketAddr, SocketAddr)>,
}

impl ViewerSession {
    /// Binds a socket on `addr` and creates the offer (receive-only video
    /// and audio) to send to the host, with that address as its candidate.
    pub fn offer(addr: SocketAddr) -> Result<(Self, String), TransportError> {
        let mut socket = NetSocket::bind(addr)?;
        let gathered = gather::gather(&mut socket, &[], &NetworkConfig::loopback());
        Self::offer_with(socket, gathered)
    }

    /// As [`offer`](Self::offer), on every address of the network scope
    /// (and public ones from STUN for `Internet`; takes up to `STUN_TIMEOUT`).
    pub fn offer_network(network: &NetworkConfig) -> Result<(Self, String), TransportError> {
        let addresses = gather::bind_addresses(network.scope)?;
        let addrs: Vec<SocketAddr> = addresses.iter().map(|a| SocketAddr::new(a.ip, 0)).collect();
        let mut socket = NetSocket::bind_all(&addrs)?;
        let gathered = gather::gather(&mut socket, &addresses, network);
        Self::offer_with(socket, gathered)
    }

    /// The other way round (manual invites): answers a host's offer, on every
    /// address of the network scope. Returns the session and the answer.
    pub fn answer_network(
        network: &NetworkConfig,
        offer: &str,
    ) -> Result<(Self, String), TransportError> {
        let addresses = gather::bind_addresses(network.scope)?;
        let addrs: Vec<SocketAddr> = addresses.iter().map(|a| SocketAddr::new(a.ip, 0)).collect();
        let mut socket = NetSocket::bind_all(&addrs)?;
        let gathered = gather::gather(&mut socket, &addresses, network);
        let offer = SdpOffer::from_sdp_string(offer).map_err(TransportError::from_display)?;
        let now = Instant::now();
        let mut rtc = crate::rtc_config().build(now);
        for candidate in gathered.str0m_candidates() {
            rtc.add_local_candidate(candidate);
        }
        let answer = rtc.sdp_api().accept_offer(offer)?;
        let mut session = Self {
            socket,
            rtc,
            mid: None,
            audio_mid: None,
            pending: None,
            timeout: now,
            events: VecDeque::new(),
            buf: vec![0; 2000],
            gathered,
            path: None,
        };
        session.drain();
        Ok((session, answer.to_sdp_string()))
    }

    /// The candidates offered, the STUN answers and the NAT hint.
    /// (local, remote) addresses the host's datagrams arrive on: the path
    /// ICE chose.
    pub fn path(&self) -> Option<(SocketAddr, SocketAddr)> {
        self.path
    }

    pub fn gathered(&self) -> &Gathered {
        &self.gathered
    }

    fn offer_with(socket: NetSocket, gathered: Gathered) -> Result<(Self, String), TransportError> {
        let now = Instant::now();
        let mut rtc = crate::rtc_config().build(now);
        for candidate in gathered.str0m_candidates() {
            rtc.add_local_candidate(candidate);
        }
        let mut change = rtc.sdp_api();
        let mid = change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let audio_mid = change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| TransportError("no offer".into()))?;
        let session = Self {
            socket,
            rtc,
            mid: Some(mid),
            audio_mid: Some(audio_mid),
            pending: Some(pending),
            timeout: now,
            events: VecDeque::new(),
            buf: vec![0; 2000],
            gathered,
            path: None,
        };
        Ok((session, offer.to_sdp_string()))
    }

    pub fn accept_answer(&mut self, answer: &str) -> Result<(), TransportError> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| TransportError("answer already applied".into()))?;
        let answer = SdpAnswer::from_sdp_string(answer).map_err(TransportError::from_display)?;
        self.rtc.sdp_api().accept_answer(pending, answer)?;
        self.drain();
        Ok(())
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    pub fn waker(&self) -> Waker {
        self.socket.waker()
    }

    /// Testing: impairs what this viewer receives (the host's media), from
    /// now on (`None` stops).
    pub fn set_impairment(&mut self, config: Option<ImpairmentConfig>) {
        self.socket.set_impairment(config);
    }

    pub fn impairment_stats(&self) -> Option<ImpairmentStats> {
        self.socket.impairment_stats()
    }

    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// Asks the host for a keyframe (PLI). False if not possible yet.
    pub fn request_keyframe(&mut self) -> bool {
        let Some(mut writer) = self.mid.and_then(|mid| self.rtc.writer(mid)) else {
            return false;
        };
        if !writer.is_request_keyframe_possible(KeyframeRequestKind::Pli) {
            return false;
        }
        let sent = writer
            .request_keyframe(None, KeyframeRequestKind::Pli)
            .is_ok();
        self.drain();
        sent
    }

    /// Runs the connection until there are events, the waker, or `deadline`.
    pub fn poll(&mut self, deadline: Instant) -> Result<(), TransportError> {
        loop {
            let now = Instant::now();
            if self.timeout <= now {
                self.rtc.handle_input(Input::Timeout(now))?;
                self.drain();
            }
            if !self.events.is_empty() {
                return Ok(());
            }
            let next = self.timeout.min(deadline).min(now + MAX_WAIT);
            match self.socket.wait(&mut self.buf, next)? {
                Woke::Packet { n, from, to } => {
                    // Anyone can send datagrams here: what is not STUN, DTLS
                    // or RTP/RTCP, or not from the host, is dropped.
                    // (No `continue`: a flood must not keep `poll` past its deadline.)
                    let input = Receive::new(Protocol::Udp, from, to, &self.buf[..n])
                        .ok()
                        .map(|receive| Input::Receive(Instant::now(), receive));
                    if let Some(input) = input
                        && self.rtc.accepts(&input)
                    {
                        // A malformed datagram costs that datagram, not the session.
                        self.path = Some((to, from));
                        if let Err(error) = self.rtc.handle_input(input) {
                            tracing::debug!(%error, "datagram rejected");
                        }
                        self.drain();
                    }
                }
                Woke::Waker => return Ok(()),
                Woke::Deadline => {}
            }
            // Hand events out as soon as there are any (a frame must not
            // wait for the deadline).
            if Instant::now() >= deadline || !self.rtc.is_alive() || !self.events.is_empty() {
                return Ok(());
            }
        }
    }

    pub fn next_event(&mut self) -> Option<ViewerEvent> {
        self.events.pop_front()
    }

    /// Closes the connection.
    pub fn close(&mut self) {
        self.rtc.disconnect();
    }

    fn drain(&mut self) {
        loop {
            let output = match self.rtc.poll_output() {
                Ok(output) => output,
                Err(error) => {
                    tracing::warn!(%error, "connection failed");
                    self.rtc.disconnect();
                    self.events.push_back(ViewerEvent::Disconnected);
                    return;
                }
            };
            match output {
                Output::Timeout(t) => {
                    self.timeout = t;
                    return;
                }
                Output::Transmit(t) => self.socket.send_to(&t.contents, t.source, t.destination),
                Output::Event(event) => match event {
                    Event::Connected => self.events.push_back(ViewerEvent::Connected),
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        self.rtc.disconnect();
                        self.events.push_back(ViewerEvent::Disconnected);
                    }
                    Event::MediaAdded(added) => match added.kind {
                        MediaKind::Video => self.mid = self.mid.or(Some(added.mid)),
                        MediaKind::Audio => self.audio_mid = self.audio_mid.or(Some(added.mid)),
                    },
                    Event::MediaData(data) if Some(data.mid) == self.audio_mid => {
                        self.events.push_back(ViewerEvent::Audio(AudioPacket {
                            rtp_time: data.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer(),
                            received_at: data.network_time,
                            data: data.data,
                        }))
                    }
                    Event::MediaData(data) => self.events.push_back(ViewerEvent::Frame(Frame {
                        rtp_time: data.time.rebase(Frequency::NINETY_KHZ).numer(),
                        received_at: data.network_time,
                        contiguous: data.contiguous,
                        data: data.data,
                    })),
                    _ => {}
                },
            }
        }
    }
}
