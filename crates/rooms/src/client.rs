//! A connection to one room that keeps itself up: it signs in with the
//! device key, keeps the connection alive, reconnects with backoff when it
//! drops (and announces again that we are in the room and sharing a
//! screen), and stops for good when the room refuses us. It runs on its own
//! threads and talks to the app through bounded queues, so the app (and its
//! UI) never waits on the network.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use identity::{Identity, PublicKey};
use serde_json::json;

use crate::invite::RoomInvite;
use crate::protocol::{self, Member, Presence, ScreenInfo, ServerMessage, Welcome};
use crate::ws::{Received, WebSocket, WsUrl};

/// The room server answers our keepalive at once; we send one this often.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Nothing received for this long (not even a keepalive answer): the
/// connection is gone.
pub const RECEIVE_TIMEOUT: Duration = Duration::from_secs(45);
/// Each step of connecting (resolve, connect, upgrade).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a polite close waits for the room's answer before aborting.
const CLOSE_WAIT: Duration = Duration::from_secs(1);
/// Signing in must finish within this.
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
/// Commands and events waiting in each direction.
const QUEUE: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Join {
    /// We are already a participant.
    Member,
    /// Create the room with this name (fails if it exists).
    Create { name: String },
    /// Join with an invite token.
    Invite(String),
}

#[derive(Clone, Debug)]
pub struct RoomConfig {
    /// `ws://` or `wss://` origin of the room server, e.g.
    /// `ws://127.0.0.1:8787`.
    pub server: String,
    pub room: String,
    /// Our display name.
    pub name: String,
    pub join: Join,
}

#[derive(Clone, Debug)]
pub enum Command {
    Enter,
    Exit,
    StartScreen(ScreenInfo),
    StopScreen,
    /// Relays a signed envelope (its JSON text) to a participant.
    Signal {
        to: PublicKey,
        envelope: String,
    },
    CreateInvite {
        ttl_secs: u32,
        max_uses: u32,
    },
    /// Rename the room (owner only).
    Rename(String),
    /// Remove a participant by key code (owner only).
    RemoveMember(String),
    /// Leave the room for good.
    Leave,
    /// Our screen's still image, sealed for the others (base64), relayed to
    /// everyone else in the room while we share a screen.
    Preview(String),
}

#[derive(Clone, Debug)]
pub enum RoomEvent {
    /// Signed in (also after each reconnect).
    Connected(Welcome),
    MemberJoined(Member),
    MemberLeft(String),
    MemberRemoved {
        key: String,
        by: String,
    },
    OwnerChanged(String),
    RoomRenamed(String),
    Presence(Presence),
    Signal {
        from: PublicKey,
        envelope: String,
    },
    SignalFailed {
        to: String,
        reason: String,
    },
    /// A sharer's sealed still image (base64), as `from` sent it.
    Preview {
        from: PublicKey,
        data: String,
    },
    InviteCreated {
        invite: RoomInvite,
        expires_at: u64,
        uses_left: u32,
    },
    /// The server refused one request (the connection stays).
    ServerError(String),
    /// The connection dropped; we try again after `retry_in`.
    Disconnected {
        reason: String,
        retry_in: Duration,
    },
    /// Over for good: refused by the room, removed, left, or stopped.
    Ended {
        reason: String,
    },
}

/// The command queue was full or the client is gone.
#[derive(Debug, PartialEq, Eq)]
pub struct NotSent;

enum Input {
    Command(Command),
    /// A receive result from the reader of connection `.0`.
    Frame(u64, std::io::Result<Received>),
    Stop,
}

pub struct RoomClient {
    inputs: SyncSender<Input>,
    thread: Option<JoinHandle<()>>,
}

impl RoomClient {
    pub fn start(config: RoomConfig, identity: Arc<Identity>) -> (Self, Receiver<RoomEvent>) {
        let (inputs, input_rx) = sync_channel(QUEUE);
        let (events, event_rx) = sync_channel(QUEUE);
        let loop_inputs = inputs.clone();
        let thread = std::thread::Builder::new()
            .name("room-client".into())
            .spawn(move || {
                let mut run = Run {
                    config,
                    identity,
                    inputs: loop_inputs,
                    input_rx,
                    events,
                    entered: false,
                    screen: None,
                    connection: 0,
                };
                run.run();
            })
            .expect("spawning a thread");
        (
            Self {
                inputs,
                thread: Some(thread),
            },
            event_rx,
        )
    }

    /// Queues a command; never waits.
    pub fn send(&self, command: Command) -> Result<(), NotSent> {
        self.inputs
            .try_send(Input::Command(command))
            .map_err(|_| NotSent)
    }

    /// Closes the connection and waits for the client thread (not for a
    /// dead network: the reader is left to time out on its own).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.inputs.try_send(Input::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RoomClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// How a connection ended.
enum End {
    /// Try again (network, server restart, timeouts).
    Retry(String),
    /// Do not try again.
    Final(String),
    Stopped,
}

struct Run {
    config: RoomConfig,
    identity: Arc<Identity>,
    inputs: SyncSender<Input>,
    input_rx: Receiver<Input>,
    events: SyncSender<RoomEvent>,
    /// What we want the room to know, restored after a reconnect.
    entered: bool,
    screen: Option<ScreenInfo>,
    /// Numbers connections, so a late frame from an old reader is ignored.
    connection: u64,
}

impl Run {
    fn run(&mut self) {
        let mut failures = 0_u32;
        loop {
            self.connection += 1;
            let (end, signed_in) = self.connect_once();
            match end {
                End::Stopped => {
                    return self.emit(RoomEvent::Ended {
                        reason: "stopped".into(),
                    });
                }
                End::Final(reason) => return self.emit(RoomEvent::Ended { reason }),
                End::Retry(reason) => {
                    failures = if signed_in { 1 } else { failures + 1 };
                    let retry_in = with_jitter(backoff(failures));
                    tracing::info!(%reason, ?retry_in, "room connection lost");
                    self.emit(RoomEvent::Disconnected { reason, retry_in });
                    if self.wait_offline(retry_in) {
                        return self.emit(RoomEvent::Ended {
                            reason: "stopped".into(),
                        });
                    }
                }
            }
        }
    }

    /// While disconnected: keep the wanted state up to date, refuse what
    /// needs a connection. True if asked to stop.
    fn wait_offline(&mut self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.input_rx.recv_timeout(left) {
                Ok(Input::Stop) | Err(RecvTimeoutError::Disconnected) => return true,
                Ok(Input::Command(command)) => {
                    if let Some(message) = self.remember(command) {
                        let to = message
                            .get("to")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                            .to_owned();
                        if message["type"] == "signal" {
                            self.emit(RoomEvent::SignalFailed {
                                to,
                                reason: "not connected".into(),
                            });
                        }
                    }
                }
                Ok(Input::Frame(..)) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Updates the wanted state for `command`; the message to send for it.
    fn remember(&mut self, command: Command) -> Option<serde_json::Value> {
        Some(match command {
            Command::Enter => {
                self.entered = true;
                json!({ "type": "room.enter" })
            }
            Command::Exit => {
                self.entered = false;
                self.screen = None;
                json!({ "type": "room.exit" })
            }
            Command::StartScreen(info) => {
                self.entered = true;
                self.screen = Some(info.clone());
                json!({ "type": "screen.start", "screen": info })
            }
            Command::StopScreen => {
                self.screen = None;
                json!({ "type": "screen.stop" })
            }
            Command::Signal { to, envelope } => {
                json!({ "type": "signal", "to": to.to_string(), "envelope": envelope })
            }
            Command::CreateInvite { ttl_secs, max_uses } => {
                json!({ "type": "invite.create", "ttl_secs": ttl_secs, "max_uses": max_uses })
            }
            Command::Rename(name) => json!({ "type": "room.rename", "name": name }),
            Command::RemoveMember(key) => json!({ "type": "member.remove", "key": key }),
            Command::Leave => json!({ "type": "room.leave" }),
            Command::Preview(data) => json!({ "type": "screen.preview", "data": data }),
        })
    }

    /// One connection, from connecting to its end; also whether it got as
    /// far as signing in.
    fn connect_once(&mut self) -> (End, bool) {
        let url = format!(
            "{}/rooms/{}/ws",
            self.config.server.trim_end_matches('/'),
            self.config.room
        );
        let Some(url) = WsUrl::parse(&url) else {
            return (
                End::Final(format!("bad room server address {:?}", self.config.server)),
                false,
            );
        };
        let socket = match WebSocket::connect(&url, CONNECT_TIMEOUT) {
            Ok(socket) => Arc::new(socket),
            Err(error) => return (End::Retry(error.to_string()), false),
        };
        let reader = {
            let (socket, inputs, id) = (Arc::clone(&socket), self.inputs.clone(), self.connection);
            std::thread::Builder::new()
                .name("room-reader".into())
                .spawn(move || {
                    loop {
                        let received = socket.receive();
                        let last = !matches!(received, Ok(Received::Text(_)));
                        if inputs.send(Input::Frame(id, received)).is_err() || last {
                            break;
                        }
                    }
                })
        };
        if let Err(error) = reader {
            return (End::Retry(error.to_string()), false);
        }
        let (end, signed_in) = match self.sign_in(&socket) {
            Ok(()) => (self.session(&socket), true),
            Err(end) => (end, false),
        };
        self.finish(&socket, &end);
        (end, signed_in)
    }

    /// Ends a connection so its reader thread returns: a polite close when
    /// we chose to end (waiting up to `CLOSE_WAIT` for the answer), then an
    /// abort, which also covers a connection that is already dead.
    fn finish(&mut self, socket: &WebSocket, end: &End) {
        if !matches!(end, End::Retry(_)) {
            socket.close(1000, "bye");
            let deadline = Instant::now() + CLOSE_WAIT;
            while let Ok(input) = self
                .input_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                if let Input::Frame(id, frame) = input
                    && id == self.connection
                    && !matches!(frame, Ok(Received::Text(_)))
                {
                    break;
                }
            }
        }
        socket.abort();
    }

    /// Answers the challenge and waits for `welcome`.
    fn sign_in(&mut self, socket: &WebSocket) -> Result<(), End> {
        let deadline = Instant::now() + AUTH_TIMEOUT;
        loop {
            let text = match self.next_frame(deadline)? {
                Some(text) => text,
                None => return Err(End::Retry("the room did not answer in time".into())),
            };
            match protocol::parse(&text) {
                Some(ServerMessage::Challenge { challenge }) => {
                    let challenge = protocol::unhex(&challenge)
                        .ok_or_else(|| End::Retry("bad challenge".into()))?;
                    let signature = self
                        .identity
                        .sign(&protocol::auth_message(&self.config.room, &challenge));
                    let mut auth = json!({
                        "type": "auth",
                        "key": self.identity.public_key().to_string(),
                        "signature": protocol::hex(&signature),
                        "name": self.config.name,
                    });
                    match &self.config.join {
                        Join::Member => {}
                        Join::Create { name } => {
                            auth["create"] = json!(true);
                            auth["room_name"] = json!(name);
                        }
                        Join::Invite(token) => auth["invite"] = json!(token),
                    }
                    send(socket, &auth)?;
                }
                Some(ServerMessage::Welcome(welcome)) => {
                    // From now on we are a participant: reconnect as one.
                    self.config.join = Join::Member;
                    self.emit(RoomEvent::Connected(welcome));
                    if let Some(info) = self.screen.clone() {
                        send(socket, &json!({ "type": "screen.start", "screen": info }))?;
                    } else if self.entered {
                        send(socket, &json!({ "type": "room.enter" }))?;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    /// The next text frame of this connection before `deadline` (`None` on
    /// timeout), handling stops and closes on the way.
    fn next_frame(&mut self, deadline: Instant) -> Result<Option<String>, End> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.input_rx.recv_timeout(left) {
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) | Ok(Input::Stop) => return Err(End::Stopped),
                Ok(Input::Command(command)) => {
                    // Not signed in yet: remember it for after.
                    let _ = self.remember(command);
                }
                Ok(Input::Frame(id, _)) if id != self.connection => {}
                Ok(Input::Frame(_, frame)) => return frame_text(frame).map(Some),
            }
        }
    }

    fn session(&mut self, socket: &WebSocket) -> End {
        let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
        // Anything received (the keepalive's answer too) proves the
        // connection is alive; WinHTTP does not time out WebSocket receives.
        let mut last_heard = Instant::now();
        loop {
            let silent_until = last_heard + RECEIVE_TIMEOUT;
            let wake = next_keepalive.min(silent_until);
            match self
                .input_rx
                .recv_timeout(wake.saturating_duration_since(Instant::now()))
            {
                Err(RecvTimeoutError::Timeout) if Instant::now() >= silent_until => {
                    return End::Retry(format!(
                        "nothing from the room for {} s",
                        RECEIVE_TIMEOUT.as_secs()
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Err(error) = socket.send_text("ping") {
                        return End::Retry(error.to_string());
                    }
                    next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
                }
                Err(RecvTimeoutError::Disconnected) | Ok(Input::Stop) => return End::Stopped,
                Ok(Input::Command(command)) => {
                    if let Some(message) = self.remember(command)
                        && let Err(end) = send(socket, &message)
                    {
                        return end;
                    }
                }
                Ok(Input::Frame(id, _)) if id != self.connection => {}
                Ok(Input::Frame(_, frame)) => {
                    last_heard = Instant::now();
                    match frame_text(frame) {
                        Ok(text) => self.handle(&text),
                        Err(end) => return end,
                    }
                }
            }
        }
    }

    fn handle(&mut self, text: &str) {
        let event = match protocol::parse(text) {
            None
            | Some(
                ServerMessage::Unknown
                | ServerMessage::Challenge { .. }
                | ServerMessage::Welcome(_),
            ) => return,
            Some(ServerMessage::MemberJoined { member }) => RoomEvent::MemberJoined(member),
            Some(ServerMessage::MemberLeft { key }) => RoomEvent::MemberLeft(key),
            Some(ServerMessage::MemberRemoved { key, by }) => RoomEvent::MemberRemoved { key, by },
            Some(ServerMessage::OwnerChanged { key }) => RoomEvent::OwnerChanged(key),
            Some(ServerMessage::RoomRenamed { name, .. }) => RoomEvent::RoomRenamed(name),
            Some(ServerMessage::PresenceChanged(presence)) => RoomEvent::Presence(presence),
            Some(ServerMessage::Signal { from, envelope }) => match from.parse() {
                Ok(from) => RoomEvent::Signal { from, envelope },
                Err(_) => return,
            },
            Some(ServerMessage::SignalFailed { to, reason }) => {
                RoomEvent::SignalFailed { to, reason }
            }
            Some(ServerMessage::InviteCreated {
                token,
                expires_at,
                uses_left,
                ..
            }) => RoomEvent::InviteCreated {
                invite: RoomInvite {
                    room: self.config.room.clone(),
                    token,
                },
                expires_at,
                uses_left,
            },
            Some(ServerMessage::InviteRevoked { .. }) => return,
            Some(ServerMessage::ScreenPreview { from, data }) => match from.parse() {
                Ok(from) => RoomEvent::Preview { from, data },
                Err(_) => return,
            },
            Some(ServerMessage::Error { reason }) => RoomEvent::ServerError(reason),
        };
        self.emit(event);
    }

    /// Hands an event to the app without ever waiting: if the app stopped
    /// reading, events are dropped (and logged).
    fn emit(&self, event: RoomEvent) {
        if let Err(TrySendError::Full(event)) = self.events.try_send(event) {
            tracing::warn!(?event, "room event dropped: the app is not reading");
        }
    }
}

fn send(socket: &WebSocket, message: &serde_json::Value) -> Result<(), End> {
    socket
        .send_text(&message.to_string())
        .map_err(|e| End::Retry(e.to_string()))
}

/// A frame's text, or how the connection ended.
fn frame_text(frame: std::io::Result<Received>) -> Result<String, End> {
    match frame {
        Ok(Received::Text(text)) => Ok(text),
        Ok(Received::Closed { code, reason }) => Err(close_end(code, &reason)),
        Err(error) => Err(End::Retry(error.to_string())),
    }
}

/// Whether a close from the room is final: refused, removed or left (the
/// room's close codes), versus anything worth retrying.
fn close_end(code: u16, reason: &str) -> End {
    let text = format!("closed by the room ({code} {reason})");
    match code {
        1000 | 4001 | 4003 | 4004 => End::Final(text),
        _ => End::Retry(text),
    }
}

/// Wait before reconnect attempt `n` (1-based): 1, 2, 4, 8, 16, 30, 30 s.
pub fn backoff(n: u32) -> Duration {
    Duration::from_secs((1_u64 << n.saturating_sub(1).min(5)).min(30))
}

/// `delay` plus or minus up to 20%, so many apps do not reconnect in step
/// after a server restart.
fn with_jitter(delay: Duration) -> Duration {
    let random = identity::random::<2>().map_or(0x8000, u16::from_le_bytes);
    let factor = 0.8 + 0.4 * f64::from(random) / f64::from(u16::MAX);
    delay.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_30_seconds() {
        let secs: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff(0).as_secs(), 1);
        assert_eq!(backoff(u32::MAX).as_secs(), 30);
    }

    #[test]
    fn jitter_stays_within_20_percent() {
        for _ in 0..100 {
            let d = with_jitter(Duration::from_secs(10)).as_secs_f64();
            assert!((8.0..=12.0).contains(&d), "{d}");
        }
    }

    #[test]
    fn refusals_are_final_and_drops_are_retried() {
        for code in [1000, 4001, 4003, 4004] {
            assert!(matches!(close_end(code, ""), End::Final(_)), "{code}");
        }
        for code in [1001, 1006, 1011, 4008, 4009] {
            assert!(matches!(close_end(code, ""), End::Retry(_)), "{code}");
        }
    }

    #[test]
    fn an_unreachable_server_is_retried_and_stop_ends_it() {
        let identity = Arc::new(Identity::generate().unwrap());
        let (client, events) = RoomClient::start(
            RoomConfig {
                server: "ws://127.0.0.1:9".into(),
                room: crate::protocol::new_room_id().unwrap(),
                name: "test".into(),
                join: Join::Create {
                    name: "test".into(),
                },
            },
            identity,
        );
        let event = events.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(event, RoomEvent::Disconnected { .. }), "{event:?}");
        let started = Instant::now();
        client.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
        let last = events.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(last, RoomEvent::Ended { .. }), "{last:?}");
    }

    #[test]
    fn a_bad_server_address_ends_at_once() {
        let (_client, events) = RoomClient::start(
            RoomConfig {
                server: "http://nope".into(),
                room: crate::protocol::new_room_id().unwrap(),
                name: "test".into(),
                join: Join::Member,
            },
            Arc::new(Identity::generate().unwrap()),
        );
        let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(event, RoomEvent::Ended { .. }), "{event:?}");
    }
}
