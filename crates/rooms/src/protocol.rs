//! The room server's messages (see `worker/README.md`): what it sends,
//! parsed into types, and helpers for what we send. Shared encodings: room
//! ids and invite tokens are lowercase base32 (RFC 4648, no padding), the
//! alphabet of key codes; the entry signature covers [`auth_message`].

use std::io;

use serde::{Deserialize, Serialize};

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
/// Room ids and invite tokens: 16 random bytes in base32.
pub const ID_CHARS: usize = 26;

pub fn base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Whether `text` is a room id or invite token (26 base32 characters).
pub fn is_id(text: &str) -> bool {
    text.len() == ID_CHARS && text.bytes().all(|b| ALPHABET.contains(&b))
}

/// A fresh random room id.
pub fn new_room_id() -> io::Result<String> {
    Ok(base32(&identity::random::<16>()?))
}

/// What a participant signs to enter a room: a domain prefix (the signature
/// means nothing anywhere else), the room id and the room's one-time
/// challenge for this connection. Must match the room server byte for byte.
pub fn auth_message(room: &str, challenge: &[u8]) -> Vec<u8> {
    let mut message = format!("nitra room auth v1\0{room}\0").into_bytes();
    message.extend_from_slice(challenge);
    message
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Member,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Member {
    /// Key code, canonical (no dashes).
    pub key: String,
    pub name: String,
    pub role: Role,
    pub joined_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenKind {
    Desktop,
    Window,
}

/// A screen as we announce it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub kind: ScreenKind,
    /// A short name: the app's name or "Monitor 1" (not a window title,
    /// which can show private things).
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Screen {
    #[serde(flatten)]
    pub info: ScreenInfo,
    pub started_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Presence {
    pub key: String,
    pub online: bool,
    pub entered: bool,
    pub screen: Option<Screen>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct RoomInfo {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Welcome {
    #[serde(default)]
    pub room: RoomInfo,
    pub you: Member,
    pub members: Vec<Member>,
    #[serde(default)]
    pub presence: Vec<Presence>,
}

/// Messages from the room server.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "challenge")]
    Challenge { challenge: String },
    #[serde(rename = "welcome")]
    Welcome(Welcome),
    #[serde(rename = "member.joined")]
    MemberJoined { member: Member },
    #[serde(rename = "member.left")]
    MemberLeft { key: String },
    #[serde(rename = "member.removed")]
    MemberRemoved { key: String, by: String },
    #[serde(rename = "owner.changed")]
    OwnerChanged { key: String },
    #[serde(rename = "presence.changed")]
    PresenceChanged(Presence),
    #[serde(rename = "signal")]
    Signal { from: String, envelope: String },
    #[serde(rename = "signal.failed")]
    SignalFailed { to: String, reason: String },
    #[serde(rename = "invite.created")]
    InviteCreated {
        id: String,
        token: String,
        expires_at: u64,
        uses_left: u32,
    },
    #[serde(rename = "room.renamed")]
    RoomRenamed { name: String, by: String },
    #[serde(rename = "invite.revoked")]
    InviteRevoked { id: String },
    /// A participant came back with another name.
    #[serde(rename = "member.renamed")]
    MemberRenamed { key: String, name: String },
    /// A sharer's still image (sealed end to end: base64, opaque here).
    #[serde(rename = "screen.preview")]
    ScreenPreview { from: String, data: String },
    #[serde(rename = "error")]
    Error { reason: String },
    /// Anything newer than this client knows about.
    #[serde(other)]
    Unknown,
}

/// A message from the server, or `None` if it is not one (not JSON, or a
/// known type with bad fields).
pub fn parse(text: &str) -> Option<ServerMessage> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc_4648() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32(plain.as_bytes()), encoded);
        }
        assert_eq!(base32(&[0xff; 16]).len(), ID_CHARS);
    }

    #[test]
    fn room_ids_are_fresh_and_valid() {
        let (a, b) = (new_room_id().unwrap(), new_room_id().unwrap());
        assert!(is_id(&a) && is_id(&b));
        assert_ne!(a, b);
        assert!(!is_id("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        assert!(!is_id("abc"));
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(&[0, 1, 0xfe, 0xff]), "0001feff");
        assert_eq!(unhex("0001FEff"), Some(vec![0, 1, 0xfe, 0xff]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }

    #[test]
    fn the_auth_message_has_the_domain_room_and_challenge() {
        let message = auth_message("room", &[1, 2]);
        assert_eq!(message, b"nitra room auth v1\0room\0\x01\x02");
    }

    #[test]
    fn server_messages_parse() {
        assert_eq!(
            parse(r#"{"type":"challenge","challenge":"00ff"}"#),
            Some(ServerMessage::Challenge {
                challenge: "00ff".into()
            })
        );
        let welcome = parse(
            r#"{"type":"welcome","you":{"key":"k","name":"Ana","role":"owner","joined_at":1},
                "members":[{"key":"k","name":"Ana","role":"owner","joined_at":1}],
                "presence":[{"key":"k","online":true,"entered":true,
                  "screen":{"kind":"window","name":"Valorant","width":1920,"height":1080,"fps":60,"started_at":5}}]}"#,
        );
        let Some(ServerMessage::Welcome(w)) = welcome else {
            panic!("{welcome:?}");
        };
        assert_eq!(w.you.role, Role::Owner);
        let screen = w.presence[0].screen.as_ref().unwrap();
        assert_eq!(
            (screen.info.kind, screen.info.fps, screen.started_at),
            (ScreenKind::Window, 60, 5)
        );
        assert_eq!(
            parse(
                r#"{"type":"presence.changed","key":"k","online":false,"entered":false,"screen":null}"#
            ),
            Some(ServerMessage::PresenceChanged(Presence {
                key: "k".into(),
                online: false,
                entered: false,
                screen: None
            }))
        );
        assert_eq!(
            parse(r#"{"type":"signal","from":"k","envelope":"{}"}"#),
            Some(ServerMessage::Signal {
                from: "k".into(),
                envelope: "{}".into()
            })
        );
        assert_eq!(
            parse(r#"{"type":"something.new","x":1}"#),
            Some(ServerMessage::Unknown)
        );
        assert_eq!(parse(r#"{"type":"signal","from":1}"#), None);
        assert_eq!(parse("pong"), None);
    }

    /// Deterministic mutations (xorshift; the same seed gives the same
    /// inputs, so a failure can be replayed): flipped bits, cuts, inserted
    /// bytes, repeated runs, stray JSON punctuation.
    struct Mutator(u64);

    impl Mutator {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }

        fn mutate(&mut self, seed: &[u8]) -> Vec<u8> {
            let mut v = seed.to_vec();
            for _ in 0..=self.below(4) {
                match self.below(5) {
                    0 if !v.is_empty() => {
                        let i = self.below(v.len());
                        v[i] ^= 1 << self.below(8);
                    }
                    1 => {
                        let i = self.below(v.len() + 1);
                        v.truncate(i);
                    }
                    2 => {
                        let (i, b) = (self.below(v.len() + 1), self.next() as u8);
                        v.insert(i, b);
                    }
                    3 if !v.is_empty() => {
                        let i = self.below(v.len());
                        let j = (i + 1 + self.below(32)).min(v.len());
                        let run = v[i..j].to_vec();
                        v.splice(i..i, run);
                    }
                    _ => {
                        let i = self.below(v.len() + 1);
                        v.insert(i, b"{}[]\":,\\0e-"[self.below(11)]);
                    }
                }
            }
            v
        }
    }

    #[test]
    fn mutated_server_messages_never_panic() {
        // What a server (or anyone in the middle of a broken TLS) could send.
        let seeds = [
            r#"{"type":"challenge","challenge":"00ff"}"#,
            r#"{"type":"signal","from":"k","envelope":"{}"}"#,
            r#"{"type":"presence.changed","key":"k","online":true,"entered":false,"screen":{"kind":"window","name":"n","width":1280,"height":720,"fps":60}}"#,
            r#"{"type":"screen.preview","from":"k","data":"AAAA"}"#,
            r#"{"type":"member.renamed","key":"k","name":"Ana"}"#,
            r#"{"type":"error","message":"x"}"#,
        ];
        let mut mutator = Mutator(0x5EED_1234_ABCD_0004);
        for _ in 0..30_000 {
            let seed = seeds[mutator.below(seeds.len())];
            let _ = parse(&String::from_utf8_lossy(&mutator.mutate(seed.as_bytes())));
        }
        // Deep nesting stops at serde_json's recursion limit.
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(parse(&deep), None);
    }
}
