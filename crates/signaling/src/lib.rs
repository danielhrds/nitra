//! Signaling between a host and its viewers: the messages they exchange to
//! set up and end a media session, and how they travel.
//!
//! The contract is the [`Message`] set and its framing; the transport is
//! replaceable (local and manual invites, or the Cloudflare rooms). This
//! crate has the local one, [`local`]: TCP on 127.0.0.1, one connection per
//! viewer, for processes on the same PC. Media never goes through here.

pub mod invite;
pub mod local;
pub mod signed;

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Largest message accepted (an SDP offer is a few KiB).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Which of a person's two streams a message is about. A room relays a
/// signal to every connection of its recipient, and a person shares their
/// screen and their camera from separate connections: each one answers
/// (or ends) only its own stream's sessions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    /// Also what apps without cameras mean (they do not send the field).
    #[default]
    Screen,
    Camera,
}

impl Stream {
    fn is_screen(&self) -> bool {
        *self == Stream::Screen
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Viewer to host: wants to watch `stream`. `profile` names the sender
    /// (logs).
    Offer {
        sdp: String,
        profile: String,
        /// Left out for the screen, as older apps send it.
        #[serde(default, skip_serializing_if = "Stream::is_screen")]
        stream: Stream,
    },
    /// Answer to an offer (it says which: `reply_to`). `profile` names the
    /// sender (logs, contact name).
    Answer { sdp: String, profile: String },
    /// Either side: the session of `stream` is over (closing, or refused
    /// with a reason).
    Bye {
        reason: String,
        #[serde(default, skip_serializing_if = "Stream::is_screen")]
        stream: Stream,
    },
}

/// Writes one message (a [`Message`], or a [`signed::Envelope`] carrying
/// one): 4-byte big-endian length, then JSON.
pub fn write_message(writer: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let json = serde_json::to_vec(message).map_err(io::Error::other)?;
    if json.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "signaling message too large",
        ));
    }
    writer.write_all(&(json.len() as u32).to_be_bytes())?;
    writer.write_all(&json)?;
    writer.flush()
}

/// Reads one message; rejects oversized or malformed ones.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "signaling message too large",
        ));
    }
    let mut json = vec![0_u8; length];
    reader.read_exact(&mut json)?;
    serde_json::from_slice(&json).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_screen_is_sent_as_older_apps_send_it_and_is_what_they_mean() {
        let screen = Message::Bye {
            reason: "x".into(),
            stream: Stream::Screen,
        };
        assert_eq!(
            serde_json::to_string(&screen).unwrap(),
            r#"{"type":"bye","reason":"x"}"#
        );
        let old_offer: Message =
            serde_json::from_str(r#"{"type":"offer","sdp":"s","profile":"p"}"#).unwrap();
        assert!(matches!(
            old_offer,
            Message::Offer {
                stream: Stream::Screen,
                ..
            }
        ));
        let camera = Message::Offer {
            sdp: "s".into(),
            profile: "p".into(),
            stream: Stream::Camera,
        };
        let text = serde_json::to_string(&camera).unwrap();
        assert!(text.contains(r#""stream":"camera""#), "{text}");
        assert_eq!(serde_json::from_str::<Message>(&text).unwrap(), camera);
    }

    #[test]
    fn messages_round_trip() {
        let messages = [
            Message::Offer {
                sdp: "v=0\r\n".into(),
                profile: "test-viewer".into(),
                stream: Stream::Screen,
            },
            Message::Answer {
                sdp: "v=0\r\na=x\r\n".into(),
                profile: "host".into(),
            },
            Message::Bye {
                reason: "closing".into(),
                stream: Stream::Camera,
            },
        ];
        let mut buffer = Vec::new();
        for message in &messages {
            write_message(&mut buffer, message).unwrap();
        }
        let mut reader = buffer.as_slice();
        for message in &messages {
            assert_eq!(&read_message::<Message>(&mut reader).unwrap(), message);
        }
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
    fn mutated_frames_are_rejected_without_panicking() {
        // A peer's framed message (length + JSON), damaged in every way,
        // including the length: never a panic, never a big allocation.
        let mut frame = Vec::new();
        write_message(
            &mut frame,
            &Message::Offer {
                sdp: "v=0\r\n".into(),
                profile: "p".into(),
                stream: Stream::Screen,
            },
        )
        .unwrap();
        let mut mutator = Mutator(0x5EED_1234_ABCD_0003);
        for _ in 0..20_000 {
            let bytes = mutator.mutate(&frame);
            let _ = read_message::<Message>(&mut bytes.as_slice());
        }
    }

    #[test]
    fn oversized_and_malformed_messages_are_rejected() {
        let huge = Message::Answer {
            sdp: "x".repeat(MAX_MESSAGE_BYTES),
            profile: "host".into(),
        };
        assert!(write_message(&mut Vec::new(), &huge).is_err());

        let mut too_long = (MAX_MESSAGE_BYTES as u32 + 1).to_be_bytes().to_vec();
        too_long.extend([b'{'; 8]);
        assert!(read_message::<Message>(&mut too_long.as_slice()).is_err());

        let mut garbage = 5_u32.to_be_bytes().to_vec();
        garbage.extend(b"hello");
        assert!(read_message::<Message>(&mut garbage.as_slice()).is_err());

        let json = br#"{"type":"teleport"}"#;
        let mut unknown = (json.len() as u32).to_be_bytes().to_vec();
        unknown.extend(json);
        assert!(read_message::<Message>(&mut unknown.as_slice()).is_err());
    }
}
