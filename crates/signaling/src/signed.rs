//! Signed signaling: every [`Message`] travels inside an [`Envelope`] that
//! says who sent it, to whom, when it expires and a one-time nonce, all
//! signed with the sender's identity key. The receiver checks the signature
//! against the sender's public key and refuses anything altered, addressed
//! to someone else, from a key it does not trust, expired or seen before.
//!
//! This is what makes the media connection authentic: the SDP inside an
//! offer or answer carries the DTLS certificate fingerprint, and DTLS
//! certificates are self-signed, so the signature on the SDP is what proves
//! the certificate belongs to the person we meant to talk to. A relay in
//! the middle (a compromised server, someone on the network) can drop
//! messages but cannot change or forge them.

use std::collections::HashMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use identity::{Identity, PublicKey, SIGNATURE_LENGTH};
use serde::{Deserialize, Serialize};

use crate::Message;

/// How long a message stays valid after it is signed (automatic signaling).
pub const LIFETIME_SECS: u64 = 60;
/// The same for manual invites and their answers: people copy and paste
/// them, which takes minutes. Also the longest lifetime accepted.
pub const MANUAL_LIFETIME_SECS: u64 = 600;
/// How far apart the two PCs' clocks may be.
pub const CLOCK_TOLERANCE_SECS: u64 = 120;
/// Nonces remembered to refuse replays (each until its message expires).
/// Bounded: when full of unexpired nonces, new messages are refused.
pub const REPLAY_CACHE_CAPACITY: usize = 4096;

/// Separates these signatures from any other use of the same key.
const DOMAIN: &[u8] = b"ziz signaling v1\0";

/// What goes on the wire: the signed body, kept as the exact JSON text that
/// was signed (no re-serialization questions), and the signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub body: String,
    /// Hex, 64 bytes.
    pub signature: String,
}

/// The signed part.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Body {
    /// Sender's key code.
    from: String,
    /// Recipient's key code; `None` only for a manual invite (an offer for
    /// whoever receives the text; it is still answered only once).
    to: Option<String>,
    /// Hex, 16 random bytes.
    nonce: String,
    /// The nonce of the message this one answers (an answer names its offer).
    reply_to: Option<String>,
    /// Unix seconds.
    issued_at: u64,
    expires_at: u64,
    message: Message,
}

/// A verified message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub from: PublicKey,
    pub nonce: String,
    pub reply_to: Option<String>,
    pub message: Message,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    Malformed,
    BadSignature,
    WrongRecipient,
    /// Correctly signed by a key we do not trust (its code, reported so the
    /// user can decide to trust it).
    Untrusted(String),
    Expired,
    NotYetValid,
    Replayed,
    /// Too many messages in flight to remember them all.
    Busy,
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed signed message"),
            Self::BadSignature => f.write_str("bad signature"),
            Self::WrongRecipient => f.write_str("message addressed to someone else"),
            Self::Untrusted(key) => write!(f, "untrusted key {key}"),
            Self::Expired => f.write_str("message expired (or the clocks differ too much)"),
            Self::NotYetValid => {
                f.write_str("message from the future (the clocks differ too much)")
            }
            Self::Replayed => f.write_str("message already received (replay)"),
            Self::Busy => f.write_str("too many messages in flight"),
        }
    }
}

impl std::error::Error for Rejected {}

/// Unix seconds now.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Signs `message` for `to`. Returns the envelope and its nonce (to match
/// the reply's `reply_to`).
pub fn seal(
    identity: &Identity,
    to: &PublicKey,
    message: Message,
    reply_to: Option<&str>,
    now: u64,
) -> std::io::Result<(Envelope, String)> {
    seal_for(
        identity,
        Some(to),
        message,
        reply_to,
        now + LIFETIME_SECS,
        now,
    )
}

/// As [`seal`], valid for [`MANUAL_LIFETIME_SECS`]. `to: None` makes an open
/// invite (accepted only for an offer).
pub fn seal_manual(
    identity: &Identity,
    to: Option<&PublicKey>,
    message: Message,
    reply_to: Option<&str>,
    now: u64,
) -> std::io::Result<(Envelope, String)> {
    seal_for(
        identity,
        to,
        message,
        reply_to,
        now + MANUAL_LIFETIME_SECS,
        now,
    )
}

fn seal_for(
    identity: &Identity,
    to: Option<&PublicKey>,
    message: Message,
    reply_to: Option<&str>,
    expires_at: u64,
    now: u64,
) -> std::io::Result<(Envelope, String)> {
    let nonce = hex(&identity::random::<16>()?);
    let body = Body {
        from: identity.public_key().to_string(),
        to: to.map(PublicKey::to_string),
        nonce: nonce.clone(),
        reply_to: reply_to.map(str::to_owned),
        issued_at: now,
        expires_at,
        message,
    };
    let body = serde_json::to_string(&body).map_err(std::io::Error::other)?;
    let signature = identity.sign(&signed_bytes(&body));
    Ok((
        Envelope {
            body,
            signature: hex(&signature),
        },
        nonce,
    ))
}

/// Who may send us messages.
#[derive(Clone, Debug)]
pub enum Trust {
    /// Only these keys.
    Keys(Vec<PublicKey>),
    /// Any correctly signed sender (development on one PC). Messages are
    /// still checked for signature, recipient, expiry and replay.
    Anyone,
}

impl Trust {
    fn allows(&self, key: &PublicKey) -> bool {
        match self {
            Self::Keys(keys) => keys.contains(key),
            Self::Anyone => true,
        }
    }
}

/// Checks incoming envelopes for one identity.
pub struct Verifier {
    me: PublicKey,
    trust: Trust,
    /// Nonce to the time after which a replay would be refused as expired
    /// anyway (so the entry can go).
    seen: HashMap<String, u64>,
    capacity: usize,
}

impl Verifier {
    pub fn new(me: PublicKey, trust: Trust) -> Self {
        Self {
            me,
            trust,
            seen: HashMap::new(),
            capacity: REPLAY_CACHE_CAPACITY,
        }
    }

    pub fn open(&mut self, envelope: &Envelope, now: u64) -> Result<Opened, Rejected> {
        let body: Body = serde_json::from_str(&envelope.body).map_err(|_| Rejected::Malformed)?;
        let from: PublicKey = body.from.parse().map_err(|_| Rejected::Malformed)?;
        let signature: [u8; SIGNATURE_LENGTH] = unhex(&envelope.signature)
            .and_then(|s| s.try_into().ok())
            .ok_or(Rejected::Malformed)?;
        if !from.verify(&signed_bytes(&envelope.body), &signature) {
            return Err(Rejected::BadSignature);
        }
        // From here on the body is exactly what `from` signed.
        let for_me = match &body.to {
            Some(to) => to.parse::<PublicKey>().ok() == Some(self.me),
            // An open invite: only an offer may be addressed to anyone.
            None => matches!(body.message, Message::Offer { .. }),
        };
        if !for_me {
            return Err(Rejected::WrongRecipient);
        }
        if !self.trust.allows(&from) {
            return Err(Rejected::Untrusted(from.to_string()));
        }
        // A longer lifetime is not ours to accept: the replay cache only
        // remembers nonces for that long.
        if body.expires_at < body.issued_at
            || body.expires_at - body.issued_at > MANUAL_LIFETIME_SECS
        {
            return Err(Rejected::Malformed);
        }
        let valid_until = body.expires_at.saturating_add(CLOCK_TOLERANCE_SECS);
        if now > valid_until {
            return Err(Rejected::Expired);
        }
        if body.issued_at > now + CLOCK_TOLERANCE_SECS {
            return Err(Rejected::NotYetValid);
        }
        self.seen.retain(|_, until| *until >= now);
        if self.seen.contains_key(&body.nonce) {
            return Err(Rejected::Replayed);
        }
        if self.seen.len() >= self.capacity {
            return Err(Rejected::Busy);
        }
        self.seen.insert(body.nonce.clone(), valid_until);
        Ok(Opened {
            from,
            nonce: body.nonce,
            reply_to: body.reply_to,
            message: body.message,
        })
    }
}

fn signed_bytes(body: &str) -> Vec<u8> {
    [DOMAIN, body.as_bytes()].concat()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000;

    fn offer() -> Message {
        Message::Offer {
            sdp: "v=0\r\na=fingerprint:sha-256 AA:BB:CC\r\n".into(),
            profile: "viewer".into(),
        }
    }

    struct Pair {
        viewer: Identity,
        host: Identity,
    }

    fn pair() -> Pair {
        Pair {
            viewer: Identity::generate().unwrap(),
            host: Identity::generate().unwrap(),
        }
    }

    fn host_verifier(p: &Pair) -> Verifier {
        Verifier::new(
            *p.host.public_key(),
            Trust::Keys(vec![*p.viewer.public_key()]),
        )
    }

    #[test]
    fn offer_and_answer_round_trip() {
        let p = pair();
        let (envelope, nonce) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
        let opened = host_verifier(&p).open(&envelope, T0 + 1).unwrap();
        assert_eq!(opened.from, *p.viewer.public_key());
        assert_eq!(opened.message, offer());
        assert_eq!(opened.nonce, nonce);

        let answer = Message::Answer {
            sdp: "v=0\r\n".into(),
            profile: "host".into(),
        };
        let (reply, _) = seal(
            &p.host,
            p.viewer.public_key(),
            answer.clone(),
            Some(&nonce),
            T0 + 1,
        )
        .unwrap();
        let mut viewer = Verifier::new(
            *p.viewer.public_key(),
            Trust::Keys(vec![*p.host.public_key()]),
        );
        let opened = viewer.open(&reply, T0 + 2).unwrap();
        assert_eq!(opened.message, answer);
        assert_eq!(opened.reply_to.as_deref(), Some(nonce.as_str()));
    }

    #[test]
    fn any_change_breaks_the_signature() {
        let p = pair();
        let (envelope, _) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
        // Someone in the middle swaps the DTLS fingerprint for their own.
        let swapped = Envelope {
            body: envelope.body.replace("AA:BB:CC", "EE:EE:EE"),
            ..envelope.clone()
        };
        assert_ne!(swapped.body, envelope.body);
        assert_eq!(
            host_verifier(&p).open(&swapped, T0),
            Err(Rejected::BadSignature)
        );
        // Or re-signs it with their own key while still claiming to be the viewer.
        let mallory = Identity::generate().unwrap();
        let forged = Envelope {
            signature: hex(&mallory.sign(&signed_bytes(&envelope.body))),
            ..envelope.clone()
        };
        assert_eq!(
            host_verifier(&p).open(&forged, T0),
            Err(Rejected::BadSignature)
        );
        let mut flipped = envelope.clone();
        flipped.signature.replace_range(0..2, "00");
        if flipped != envelope {
            assert_eq!(
                host_verifier(&p).open(&flipped, T0),
                Err(Rejected::BadSignature)
            );
        }
    }

    #[test]
    fn wrong_recipient_and_untrusted_sender_are_refused() {
        let p = pair();
        let other = Identity::generate().unwrap();
        let (to_other, _) = seal(&p.viewer, other.public_key(), offer(), None, T0).unwrap();
        assert_eq!(
            host_verifier(&p).open(&to_other, T0),
            Err(Rejected::WrongRecipient)
        );
        let (from_stranger, _) = seal(&other, p.host.public_key(), offer(), None, T0).unwrap();
        assert_eq!(
            host_verifier(&p).open(&from_stranger, T0),
            Err(Rejected::Untrusted(other.public_key().to_string()))
        );
        let mut anyone = Verifier::new(*p.host.public_key(), Trust::Anyone);
        assert!(anyone.open(&from_stranger, T0).is_ok());
    }

    #[test]
    fn expiry_tolerates_clock_differences_within_limits() {
        let p = pair();
        let (envelope, _) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
        let last_valid = T0 + LIFETIME_SECS + CLOCK_TOLERANCE_SECS;
        assert!(host_verifier(&p).open(&envelope, last_valid).is_ok());
        assert_eq!(
            host_verifier(&p).open(&envelope, last_valid + 1),
            Err(Rejected::Expired)
        );
        // The receiver's clock is behind the sender's.
        assert!(
            host_verifier(&p)
                .open(&envelope, T0 - CLOCK_TOLERANCE_SECS)
                .is_ok()
        );
        assert_eq!(
            host_verifier(&p).open(&envelope, T0 - CLOCK_TOLERANCE_SECS - 1),
            Err(Rejected::NotYetValid)
        );
    }

    #[test]
    fn a_message_is_accepted_once() {
        let p = pair();
        let mut host = host_verifier(&p);
        let (envelope, _) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
        assert!(host.open(&envelope, T0).is_ok());
        assert_eq!(host.open(&envelope, T0 + 5), Err(Rejected::Replayed));
        // A fresh message with the same content is fine.
        let (again, _) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
        assert!(host.open(&again, T0 + 5).is_ok());
    }

    #[test]
    fn the_replay_cache_is_bounded_and_forgets_expired_nonces() {
        let p = pair();
        let mut host = host_verifier(&p);
        host.capacity = 8; // same logic, fewer (slow in debug) signatures
        for _ in 0..host.capacity {
            let (e, _) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
            host.open(&e, T0).unwrap();
        }
        let (extra, _) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
        assert_eq!(host.open(&extra, T0), Err(Rejected::Busy));
        // Once the earlier ones expire, there is room again.
        let later = T0 + LIFETIME_SECS + CLOCK_TOLERANCE_SECS + 1;
        let (fresh, _) = seal(&p.viewer, p.host.public_key(), offer(), None, later).unwrap();
        assert!(host.open(&fresh, later).is_ok());
        assert_eq!(host.seen.len(), 1);
    }

    #[test]
    fn malformed_envelopes_and_overlong_lifetimes_are_refused() {
        let p = pair();
        let mut host = host_verifier(&p);
        let garbage = Envelope {
            body: "{".into(),
            signature: "00".into(),
        };
        assert_eq!(host.open(&garbage, T0), Err(Rejected::Malformed));
        let (envelope, _) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
        let short_signature = Envelope {
            signature: envelope.signature[..64].into(),
            ..envelope
        };
        assert_eq!(host.open(&short_signature, T0), Err(Rejected::Malformed));

        // Correctly signed, but asking to stay valid for an hour.
        let body = Body {
            from: p.viewer.public_key().to_string(),
            to: Some(p.host.public_key().to_string()),
            nonce: "00".repeat(16),
            reply_to: None,
            issued_at: T0,
            expires_at: T0 + 3600,
            message: offer(),
        };
        let body = serde_json::to_string(&body).unwrap();
        let long = Envelope {
            signature: hex(&p.viewer.sign(&signed_bytes(&body))),
            body,
        };
        assert_eq!(host.open(&long, T0), Err(Rejected::Malformed));
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(&[0, 1, 0xab, 0xff]), "0001abff");
        assert_eq!(unhex("0001abff").unwrap(), [0, 1, 0xab, 0xff]);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }

    #[test]
    fn an_open_invite_is_accepted_only_for_an_offer() {
        let p = pair();
        let mut viewer = Verifier::new(*p.viewer.public_key(), Trust::Anyone);
        let (invite, nonce) = seal_manual(&p.host, None, offer(), None, T0).unwrap();
        let opened = viewer.open(&invite, T0).unwrap();
        assert_eq!(opened.from, *p.host.public_key());
        assert_eq!(opened.nonce, nonce);
        let answer = Message::Answer {
            sdp: "v=0\r\n".into(),
            profile: "host".into(),
        };
        let (open_answer, _) = seal_manual(&p.host, None, answer, None, T0).unwrap();
        assert_eq!(viewer.open(&open_answer, T0), Err(Rejected::WrongRecipient));
        let bye = Message::Bye { reason: "x".into() };
        let (open_bye, _) = seal_manual(&p.host, None, bye, None, T0).unwrap();
        assert_eq!(viewer.open(&open_bye, T0), Err(Rejected::WrongRecipient));
    }

    #[test]
    fn manual_messages_last_ten_minutes() {
        let p = pair();
        let (envelope, _) =
            seal_manual(&p.viewer, Some(p.host.public_key()), offer(), None, T0).unwrap();
        let last_valid = T0 + MANUAL_LIFETIME_SECS + CLOCK_TOLERANCE_SECS;
        assert!(host_verifier(&p).open(&envelope, last_valid).is_ok());
        assert_eq!(
            host_verifier(&p).open(&envelope, last_valid + 1),
            Err(Rejected::Expired)
        );
        // Automatic messages still expire after a minute.
        let (quick, _) = seal(&p.viewer, p.host.public_key(), offer(), None, T0).unwrap();
        assert_eq!(
            host_verifier(&p).open(&quick, T0 + LIFETIME_SECS + CLOCK_TOLERANCE_SECS + 1),
            Err(Rejected::Expired)
        );
    }
}
