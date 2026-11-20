//! The viewer side: one str0m `Rtc` receiving one video stream as complete
//! frames (str0m depacketizes, reorders and recovers losses with NACK/RTX).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::TransportError;
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

#[derive(Debug)]
pub enum ViewerEvent {
    Connected,
    /// ICE lost the host, or it closed.
    Disconnected,
    Frame(Frame),
}

pub struct ViewerSession {
    socket: NetSocket,
    rtc: Rtc,
    mid: Mid,
    pending: Option<SdpPendingOffer>,
    timeout: Instant,
    events: VecDeque<ViewerEvent>,
    buf: Vec<u8>,
    limit: Option<ReceiveLimit>,
}

/// Development aid: drops datagrams above a rate, like a slow link's
/// bottleneck (token bucket ~100 ms deep). Not a full impairment model.
struct ReceiveLimit {
    bytes_per_second: f64,
    depth: f64,
    tokens: f64,
    updated: Instant,
    dropped: u64,
}

impl ReceiveLimit {
    fn new(bps: u64, now: Instant) -> Self {
        let bytes_per_second = bps as f64 / 8.0;
        let depth = (bytes_per_second * 0.1).max(16_000.0);
        Self {
            bytes_per_second,
            depth,
            tokens: depth,
            updated: now,
            dropped: 0,
        }
    }

    fn admit(&mut self, bytes: usize, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.updated = now;
        self.tokens = (self.tokens + elapsed * self.bytes_per_second).min(self.depth);
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            true
        } else {
            self.dropped += 1;
            false
        }
    }
}

impl ViewerSession {
    /// Binds a socket and creates the offer (receive-only video) to send to the host.
    pub fn offer(addr: SocketAddr) -> Result<(Self, String), TransportError> {
        let socket = NetSocket::bind(addr)?;
        let now = Instant::now();
        let mut rtc = crate::rtc_config().build(now);
        rtc.add_local_candidate(
            Candidate::host(socket.local_addr(), "udp").map_err(TransportError::from_display)?,
        );
        let mut change = rtc.sdp_api();
        let mid = change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| TransportError("no offer".into()))?;
        let session = Self {
            socket,
            rtc,
            mid,
            pending: Some(pending),
            timeout: now,
            events: VecDeque::new(),
            buf: vec![0; 2000],
            limit: None,
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

    pub fn waker(&self) -> Waker {
        self.socket.waker()
    }

    /// Development aid: simulates a link of `bps` by dropping what arrives
    /// above it (`None` removes the limit).
    pub fn set_receive_limit(&mut self, bps: Option<u64>) {
        self.limit = bps.map(|bps| ReceiveLimit::new(bps, Instant::now()));
    }

    /// Datagrams dropped by the receive limit.
    pub fn limit_dropped(&self) -> u64 {
        self.limit.as_ref().map_or(0, |l| l.dropped)
    }

    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// Asks the host for a keyframe (PLI). False if not possible yet.
    pub fn request_keyframe(&mut self) -> bool {
        let Some(mut writer) = self.rtc.writer(self.mid) else {
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
                Woke::Packet(n, _)
                    if self
                        .limit
                        .as_mut()
                        .is_some_and(|l| !l.admit(n, Instant::now())) => {}
                Woke::Packet(n, from) => {
                    // Anyone can send datagrams here: what is not STUN, DTLS
                    // or RTP/RTCP, or not from the host, is dropped.
                    // (No `continue`: a flood must not keep `poll` past its deadline.)
                    let input = Receive::new(
                        Protocol::Udp,
                        from,
                        self.socket.local_addr(),
                        &self.buf[..n],
                    )
                    .ok()
                    .map(|receive| Input::Receive(Instant::now(), receive));
                    if let Some(input) = input
                        && self.rtc.accepts(&input)
                    {
                        // A malformed datagram costs that datagram, not the session.
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
                Output::Transmit(t) => self.socket.send_to(&t.contents, t.destination),
                Output::Event(event) => match event {
                    Event::Connected => self.events.push_back(ViewerEvent::Connected),
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        self.rtc.disconnect();
                        self.events.push_back(ViewerEvent::Disconnected);
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
