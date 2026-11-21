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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Viewer to host: wants to watch. `profile` names the sender (logs).
    Offer { sdp: String, profile: String },
    /// Answer to an offer. `profile` names the sender (logs, contact name).
    Answer { sdp: String, profile: String },
    /// Either side: the session is over (closing, or refused with a reason).
    Bye { reason: String },
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
    fn messages_round_trip() {
        let messages = [
            Message::Offer {
                sdp: "v=0\r\n".into(),
                profile: "test-viewer".into(),
            },
            Message::Answer {
                sdp: "v=0\r\na=x\r\n".into(),
                profile: "host".into(),
            },
            Message::Bye {
                reason: "closing".into(),
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
