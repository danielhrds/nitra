//! Friend requests through a room both people are in: the sender starts a
//! friendship (a room of two) and sends its single-use invite to the other
//! person as a room signal, sealed end to end for them (the room server
//! relays it but can neither read nor use the invite). The other answers by
//! joining with the invite, or with a "declined" signal so the sender drops
//! the waiting friendship.
//!
//! The seal binds the room the request came through and proves the sender
//! (see `identity::seal`); a request older than [`MAX_AGE_MS`] is ignored, so
//! one replayed later shows nothing.

use std::io;

use identity::seal::{self, Sealed};
use identity::{Identity, PublicKey};
use serde::{Deserialize, Serialize};

use crate::preview::{base64_decode, base64_encode};

/// Requests older than this (or this far in the future) are ignored.
pub const MAX_AGE_MS: u64 = 10 * 60 * 1000;

/// What the sender offers: the friendship's invite and who they are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// `nitraroom:` invite to the friendship.
    pub invite: String,
    /// The sender's name, as they call themselves.
    pub name: String,
    /// When it was sent (ms since the Unix epoch).
    pub sent_at_ms: u64,
}

/// A friend signal that arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    Request(Request),
    /// The other one said no to our friendship `friend_room`.
    Declined {
        friend_room: String,
    },
}

/// The envelope as it travels in the room's `signal`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum Envelope {
    #[serde(rename = "friend.request")]
    Request { sealed: String },
    #[serde(rename = "friend.declined")]
    Declined { room: String },
}

fn context(room: &str) -> Vec<u8> {
    let mut context = b"nitra friend request v1\0".to_vec();
    context.extend_from_slice(room.as_bytes());
    context
}

/// The signal envelope asking `to` (through `room`) to be friends.
pub fn request_envelope(
    identity: &Identity,
    to: &PublicKey,
    room: &str,
    request: &Request,
) -> io::Result<String> {
    let payload = serde_json::to_vec(request).map_err(io::Error::other)?;
    let sealed = seal::seal(identity, &[*to], &context(room), &payload)?;
    serde_json::to_string(&Envelope::Request {
        sealed: base64_encode(&sealed.to_bytes()),
    })
    .map_err(io::Error::other)
}

/// The signal envelope saying no to friendship `friend_room`.
pub fn declined_envelope(friend_room: &str) -> String {
    serde_json::to_string(&Envelope::Declined {
        room: friend_room.to_owned(),
    })
    .expect("plain JSON")
}

/// Reads a friend signal `from` someone in `room`; None for anything else
/// (another kind of signal, not for us, altered, too old).
pub fn open(
    identity: &Identity,
    from: &PublicKey,
    room: &str,
    envelope: &str,
    now_ms: u64,
) -> Option<Incoming> {
    match serde_json::from_str::<Envelope>(envelope).ok()? {
        Envelope::Request { sealed } => {
            let sealed = Sealed::from_bytes(&base64_decode(&sealed)?)?;
            let payload = seal::open(identity, from, &context(room), &sealed)?;
            let request: Request = serde_json::from_slice(&payload).ok()?;
            (request.sent_at_ms.abs_diff(now_ms) <= MAX_AGE_MS
                && rooms::RoomInvite::parse(&request.invite).is_some())
            .then_some(Incoming::Request(request))
        }
        Envelope::Declined { room } => {
            rooms::protocol::is_id(&room).then_some(Incoming::Declined { friend_room: room })
        }
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM: &str = "abcdefghijklmnopqrstuvwxyz";
    const INVITE: &str = "nitraroom:abcdefghijklmnopqrstuvwxyz.234567abcdefghijklmnopqrst";

    fn request(at: u64) -> Request {
        Request {
            invite: INVITE.into(),
            name: "Ana".into(),
            sent_at_ms: at,
        }
    }

    #[test]
    fn a_request_opens_only_for_its_recipient_room_and_time() {
        let ana = Identity::generate().unwrap();
        let bia = Identity::generate().unwrap();
        let eve = Identity::generate().unwrap();
        let envelope = request_envelope(&ana, bia.public_key(), ROOM, &request(1_000_000)).unwrap();
        assert!(!envelope.contains("nitraroom"), "the invite travels sealed");
        let opened = open(&bia, ana.public_key(), ROOM, &envelope, 1_000_000 + 1_000);
        assert_eq!(opened, Some(Incoming::Request(request(1_000_000))));
        // Someone else, another claimed sender, another room, too late.
        assert_eq!(
            open(&eve, ana.public_key(), ROOM, &envelope, 1_000_000),
            None
        );
        assert_eq!(
            open(&bia, eve.public_key(), ROOM, &envelope, 1_000_000),
            None
        );
        let other_room = "bcdefghijklmnopqrstuvwxyza";
        assert_eq!(
            open(&bia, ana.public_key(), other_room, &envelope, 1_000_000),
            None
        );
        assert_eq!(
            open(
                &bia,
                ana.public_key(),
                ROOM,
                &envelope,
                1_000_000 + MAX_AGE_MS + 1
            ),
            None
        );
    }

    #[test]
    fn a_decline_names_the_friendship_and_other_signals_are_not_ours() {
        let bia = Identity::generate().unwrap();
        let ana = Identity::generate().unwrap();
        let declined = declined_envelope(ROOM);
        assert_eq!(
            open(&ana, bia.public_key(), "r", &declined, 0),
            Some(Incoming::Declined {
                friend_room: ROOM.into()
            })
        );
        assert_eq!(
            open(
                &ana,
                bia.public_key(),
                "r",
                r#"{"t":"friend.declined","room":"x"}"#,
                0
            ),
            None
        );
        // A signed offer envelope (watching) is not a friend signal.
        assert_eq!(
            open(
                &ana,
                bia.public_key(),
                "r",
                r#"{"payload":"..","signature":".."}"#,
                0
            ),
            None
        );
        assert_eq!(open(&ana, bia.public_key(), "r", "not json", 0), None);
    }
}
