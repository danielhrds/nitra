//! Signaling through a room (the room server in `worker/`): the host shows
//! its screen in the room and answers offers relayed by it; a viewer sends
//! its offer to the host through the room. Offers, answers and goodbyes are
//! the same signed envelopes as local and manual signaling: the room relays
//! them and cannot change or forge them. Media goes directly between the PCs.

use std::collections::HashSet;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use identity::{Identity, PublicKey};
use rooms::{Command, Join, RoomClient, RoomConfig, RoomEvent, RoomInvite, ScreenInfo};
use signaling::Message;
use signaling::signed::{self, Envelope, Trust, Verifier};
use transport::ViewerSession;
use transport::gather::NetworkConfig;

use crate::profile::Profile;

/// How long a viewer waits to sign in and for the host's answer.
const WATCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Room invites made by `--room-invite`: one hour, five uses.
const INVITE_TTL_SECS: u32 = 3600;
const INVITE_USES: u32 = 5;

#[derive(Clone, Debug)]
pub struct RoomArgs {
    /// `ws://` or `wss://` origin of the room server.
    pub server: String,
    pub room: String,
    pub join: Join,
    /// Host: also create an invite (printed and copied to the clipboard).
    pub invite: bool,
}

impl RoomArgs {
    fn config(&self, name: &str) -> RoomConfig {
        RoomConfig {
            server: self.server.clone(),
            room: self.room.clone(),
            name: name.to_owned(),
            join: self.join.clone(),
        }
    }
}

/// The host's side, on a thread of its own until `stop`: shows `screen` in
/// the room and answers the offers of room participants. `accept` turns an
/// offer (SDP, viewer name, viewer key) into our answer SDP; `left` is told
/// when a viewer says goodbye.
#[allow(clippy::too_many_arguments)]
pub fn serve(
    identity: Arc<Identity>,
    name: String,
    screen: ScreenInfo,
    args: RoomArgs,
    stop: Arc<AtomicBool>,
    previews: Previews,
    mut accept: impl FnMut(String, String, PublicKey) -> Result<String, String> + Send + 'static,
    mut left: impl FnMut(PublicKey) + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("room".into())
        .spawn(move || {
            use windows::Win32::System::Com::{
                COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
            };
            // SAFETY: joins the MTA for this thread (previews use WIC);
            // balanced at the end of the thread.
            let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            let (client, events) = RoomClient::start(args.config(&name), Arc::clone(&identity));
            let mut verifier = Verifier::new(*identity.public_key(), Trust::Anyone);
            let mut members: HashSet<PublicKey> = HashSet::new();
            let mut online: HashSet<String> = HashSet::new();
            let mut announced = false;
            while !stop.load(Ordering::Relaxed) {
                // Pictures the host took: JPEG, sealed for everyone else, sent.
                while let Ok(picture) = previews.pictures.try_recv() {
                    send_preview(&client, &identity, &args.room, &members, &picture);
                }
                let event = match events.recv_timeout(Duration::from_millis(200)) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                match event {
                    RoomEvent::Connected(welcome) => {
                        members = welcome
                            .members
                            .iter()
                            .filter_map(|m| m.key.parse().ok())
                            .collect();
                        online = welcome
                            .presence
                            .iter()
                            .filter(|p| p.online)
                            .map(|p| p.key.clone())
                            .collect();
                        if !announced {
                            announced = true;
                            println!(
                                "room {} ({:?}): in as {:?}, {} participant(s); sharing {:?}",
                                args.room,
                                welcome.room.name,
                                welcome.you.role,
                                members.len(),
                                screen.name
                            );
                            let _ = client.send(Command::StartScreen(screen.clone()));
                            if args.invite {
                                let _ = client.send(Command::CreateInvite {
                                    ttl_secs: INVITE_TTL_SECS,
                                    max_uses: INVITE_USES,
                                });
                            }
                        }
                    }
                    RoomEvent::MemberJoined(member) => {
                        members.extend(member.key.parse::<PublicKey>().ok());
                    }
                    RoomEvent::MemberLeft(key) | RoomEvent::MemberRemoved { key, .. } => {
                        if let Ok(key) = key.parse::<PublicKey>() {
                            members.remove(&key);
                        }
                    }
                    RoomEvent::InviteCreated { invite, .. } => share_invite(&invite),
                    RoomEvent::Signal { from, envelope } => {
                        if !members.contains(&from) {
                            tracing::warn!(%from, "signal from someone not in the room; ignored");
                            continue;
                        }
                        let opened = match open(&mut verifier, &from, &envelope) {
                            Ok(opened) => opened,
                            Err(error) => {
                                tracing::warn!(%from, %error, "signal refused");
                                continue;
                            }
                        };
                        match opened.message {
                            Message::Offer { sdp, profile } => {
                                let reply = match accept(sdp, profile, from) {
                                    Ok(answer) => Message::Answer {
                                        sdp: answer,
                                        profile: name.clone(),
                                    },
                                    Err(reason) => Message::Bye { reason },
                                };
                                send_sealed(&client, &identity, &from, reply, Some(&opened.nonce));
                            }
                            Message::Bye { .. } => left(from),
                            Message::Answer { .. } => {}
                        }
                    }
                    RoomEvent::SignalFailed { to, reason } => {
                        tracing::info!(%to, %reason, "signal not delivered")
                    }
                    RoomEvent::Disconnected { reason, retry_in } => {
                        println!(
                            "room: connection lost ({reason}); retrying in {:.0} s",
                            retry_in.as_secs_f64()
                        );
                    }
                    RoomEvent::Ended { reason } => {
                        println!("room: {reason}");
                        break;
                    }
                    RoomEvent::Presence(presence) => {
                        // Someone just came online: they have no preview of
                        // ours yet (the room keeps none), so a fresh one is due.
                        let me = identity.public_key().to_string().replace('-', "");
                        if !presence.online {
                            online.remove(&presence.key);
                        } else if presence.key != me && online.insert(presence.key.clone()) {
                            previews.wanted.store(true, Ordering::Relaxed);
                        }
                    }
                    RoomEvent::Preview { .. }
                    | RoomEvent::MemberRenamed { .. }
                    | RoomEvent::OwnerChanged(_)
                    | RoomEvent::RoomRenamed(_)
                    | RoomEvent::ServerError(_) => {}
                }
            }
            client.stop();
            if com {
                // SAFETY: balances the call above, after every WIC object is gone.
                unsafe { CoUninitialize() };
            }
        })
}

/// What the host's room thread needs for previews: the small pictures the
/// host takes, and a flag it raises when a fresh one is due (someone came
/// online).
pub struct Previews {
    pub pictures: Receiver<gpu::convert::Nv12Image>,
    pub wanted: Arc<AtomicBool>,
}

/// Turns `picture` into a JPEG, seals it for every other participant and
/// sends it to the room (on this thread, off the media path).
fn send_preview(
    client: &RoomClient,
    identity: &Identity,
    room: &str,
    members: &HashSet<PublicKey>,
    picture: &gpu::convert::Nv12Image,
) {
    let recipients: Vec<PublicKey> = members
        .iter()
        .filter(|k| *k != identity.public_key())
        .copied()
        .collect();
    if recipients.is_empty() {
        return;
    }
    let started = Instant::now();
    let bgr = crate::preview::bgr_from_nv12(picture);
    let sealed = crate::preview::jpeg_from_bgr(picture.width, picture.height, &bgr)
        .and_then(|jpeg| crate::preview::seal_for_room(identity, room, &recipients, &jpeg));
    match sealed {
        Ok(data) => {
            tracing::info!(
                bytes = data.len(),
                recipients = recipients.len(),
                made_us = started.elapsed().as_micros() as u64,
                "preview sent"
            );
            let _ = client.send(Command::Preview(data));
        }
        Err(error) => tracing::warn!(%error, "preview not made"),
    }
}

/// Checks a relayed envelope: correctly signed, for us, fresh, and signed by
/// the participant the room says sent it.
fn open(
    verifier: &mut Verifier,
    from: &PublicKey,
    envelope: &str,
) -> Result<signed::Opened, String> {
    let envelope: Envelope =
        serde_json::from_str(envelope).map_err(|e| format!("not an envelope: {e}"))?;
    let opened = verifier
        .open(&envelope, signed::now())
        .map_err(|e| e.to_string())?;
    if opened.from != *from {
        return Err(format!(
            "signed by {} but relayed as from {from}",
            opened.from
        ));
    }
    Ok(opened)
}

fn send_sealed(
    client: &RoomClient,
    identity: &Identity,
    to: &PublicKey,
    message: Message,
    reply_to: Option<&str>,
) -> Option<String> {
    let (envelope, nonce) = match signed::seal(identity, to, message, reply_to, signed::now()) {
        Ok(sealed) => sealed,
        Err(error) => {
            tracing::warn!(%error, "sealing failed");
            return None;
        }
    };
    let envelope = serde_json::to_string(&envelope).expect("envelopes serialize");
    client
        .send(Command::Signal { to: *to, envelope })
        .ok()
        .map(|()| nonce)
}

/// Prints the invite, and copies it when someone is at the console (not in
/// scripts, which must not replace the user's clipboard).
fn share_invite(invite: &RoomInvite) {
    let text = invite.text();
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("room invite: {text}");
        return;
    }
    match crate::clipboard::set_text(&text) {
        Ok(()) => println!("room invite (copied to the clipboard): {text}"),
        Err(error) => println!("room invite: {text} (not copied: {error})"),
    }
}

/// A viewer's open room connection, kept to say goodbye and to notice the
/// host leaving.
pub struct RoomLink {
    client: RoomClient,
    events: Option<Receiver<RoomEvent>>,
    identity: Arc<Identity>,
    host_key: PublicKey,
}

/// Viewer: signs in to the room, sends our offer to `host_key` and waits for
/// its signed answer.
pub fn watch(
    profile: &Profile,
    network: &NetworkConfig,
    args: &RoomArgs,
    host_key: PublicKey,
) -> Result<(ViewerSession, RoomLink), Box<dyn Error>> {
    let (client, events) =
        RoomClient::start(args.config(&profile.name), Arc::clone(&profile.identity));
    let deadline = Instant::now() + WATCH_TIMEOUT;
    let welcome = loop {
        match next(&events, deadline)? {
            RoomEvent::Connected(welcome) => break welcome,
            RoomEvent::Ended { reason } => return Err(format!("room: {reason}").into()),
            RoomEvent::Disconnected { reason, .. } => println!("room: {reason}; retrying"),
            _ => {}
        }
    };
    let host_code = host_key.to_string().replace('-', "");
    match welcome.presence.iter().find(|p| p.key == host_code) {
        Some(p) if p.screen.is_some() => {}
        Some(_) => println!("room: the host is online but not sharing a screen yet"),
        None => return Err("room: the host is not online in this room".into()),
    }
    let (mut session, offer) = ViewerSession::offer_network(network)?;
    let offer = Message::Offer {
        sdp: offer,
        profile: profile.name.clone(),
    };
    let nonce = send_sealed(&client, &profile.identity, &host_key, offer, None)
        .ok_or("room: could not send the offer")?;
    let mut verifier = Verifier::new(*profile.identity.public_key(), Trust::Keys(vec![host_key]));
    loop {
        match next(&events, deadline)? {
            RoomEvent::Signal { from, envelope } if from == host_key => {
                let opened = open(&mut verifier, &from, &envelope)?;
                match opened.message {
                    Message::Answer { sdp, .. }
                        if opened.reply_to.as_deref() == Some(nonce.as_str()) =>
                    {
                        session.accept_answer(&sdp)?;
                        break;
                    }
                    Message::Bye { reason } => return Err(format!("host refused: {reason}").into()),
                    _ => {}
                }
            }
            RoomEvent::SignalFailed { reason, .. } => {
                return Err(format!("room: offer not delivered ({reason})").into());
            }
            RoomEvent::Ended { reason } => return Err(format!("room: {reason}").into()),
            _ => {}
        }
    }
    Ok((
        session,
        RoomLink {
            client,
            events: Some(events),
            identity: Arc::clone(&profile.identity),
            host_key,
        },
    ))
}

fn next(events: &Receiver<RoomEvent>, deadline: Instant) -> Result<RoomEvent, Box<dyn Error>> {
    events
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "room: no answer in time".into())
}

impl RoomLink {
    /// A thread that sets `gone` (and wakes the viewer) when the host says
    /// goodbye, stops sharing or goes offline.
    pub fn watch_host(
        &mut self,
        gone: Arc<AtomicBool>,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<JoinHandle<()>> {
        let events = self.events.take().expect("watched once");
        let identity = Arc::clone(&self.identity);
        let host_key = self.host_key;
        let host_code = host_key.to_string().replace('-', "");
        std::thread::Builder::new().name("room-watch".into()).spawn(move || {
            let mut verifier = Verifier::new(*identity.public_key(), Trust::Keys(vec![host_key]));
            for event in events {
                let ended = match event {
                    RoomEvent::Signal { from, envelope } if from == host_key => {
                        matches!(open(&mut verifier, &from, &envelope), Ok(o) if matches!(o.message, Message::Bye { .. }))
                    }
                    RoomEvent::Presence(p) if p.key == host_code => !p.online || p.screen.is_none(),
                    RoomEvent::Ended { .. } => break,
                    _ => false,
                };
                if ended {
                    tracing::info!("the host stopped sharing");
                    gone.store(true, Ordering::Relaxed);
                    wake();
                    break;
                }
            }
        })
    }

    /// Says goodbye to the host (unless it left) and leaves the room
    /// connection.
    pub fn finish(self, reason: &str, say_bye: bool) {
        if say_bye {
            let bye = Message::Bye {
                reason: reason.into(),
            };
            let _ = send_sealed(&self.client, &self.identity, &self.host_key, bye, None);
        }
        self.client.stop();
    }
}
