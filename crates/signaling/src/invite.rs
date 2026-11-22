//! Manual invites: a signed [`Envelope`] as text people can copy and paste
//! (WhatsApp, Discord...), for connecting without the signaling server.
//!
//! Format: `ziz1:` followed by the envelope's JSON, compressed with the
//! Windows Compression API (XPRESS Huffman; no extra library) and written in
//! base64url. Spaces and line breaks a chat app inserts are ignored when
//! reading. The text proves nothing by itself: the envelope inside is
//! verified like any other signed message.

use std::fmt;
use std::io;

use windows::Win32::Storage::Compression::{
    COMPRESS_ALGORITHM, COMPRESS_ALGORITHM_XPRESS_HUFF, COMPRESSOR_HANDLE, CloseCompressor,
    CloseDecompressor, Compress, CreateCompressor, CreateDecompressor, DECOMPRESSOR_HANDLE,
    Decompress,
};

use crate::MAX_MESSAGE_BYTES;
use crate::signed::Envelope;

/// Starts every invite and answer text; the digit is the format version.
pub const PREFIX: &str = "ziz1:";

const ALGORITHM: COMPRESS_ALGORITHM = COMPRESS_ALGORITHM_XPRESS_HUFF;

#[derive(Debug, PartialEq, Eq)]
pub enum InviteError {
    /// Does not start with `ziz1:` (not an invite, or a newer version).
    NotAnInvite,
    /// Cut short or altered on the way (bad characters, does not unpack).
    Damaged,
}

impl fmt::Display for InviteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnInvite => write!(f, "not a ziz invite (it starts with {PREFIX})"),
            Self::Damaged => f.write_str("the invite text is incomplete or damaged; copy it again"),
        }
    }
}

impl std::error::Error for InviteError {}

pub fn encode(envelope: &Envelope) -> io::Result<String> {
    let json = serde_json::to_vec(envelope).map_err(io::Error::other)?;
    if json.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invite too large",
        ));
    }
    Ok(format!("{PREFIX}{}", base64url(&compress(&json)?)))
}

pub fn decode(text: &str) -> Result<Envelope, InviteError> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let payload = compact
        .strip_prefix(PREFIX)
        .ok_or(InviteError::NotAnInvite)?;
    let packed = unbase64url(payload).ok_or(InviteError::Damaged)?;
    let json = decompress(&packed).map_err(|_| InviteError::Damaged)?;
    serde_json::from_slice(&json).map_err(|_| InviteError::Damaged)
}

fn compress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut handle = COMPRESSOR_HANDLE::default();
    // SAFETY: `handle` receives a compressor, closed below; the data and
    // output buffers are valid for the sizes passed.
    unsafe {
        CreateCompressor(ALGORITHM, None, &mut handle).map_err(io::Error::other)?;
        // Incompressible data grows a little; the size call is not needed
        // with this margin.
        let mut output = vec![0_u8; data.len() + 1024];
        let mut size = 0;
        let result = Compress(
            handle,
            Some(data.as_ptr().cast()),
            data.len(),
            Some(output.as_mut_ptr().cast()),
            output.len(),
            &mut size,
        );
        let _ = CloseCompressor(handle);
        result.map_err(io::Error::other)?;
        output.truncate(size);
        Ok(output)
    }
}

/// Unpacks at most `MAX_MESSAGE_BYTES`: a crafted text cannot make it
/// allocate more (the call fails instead).
fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut handle = DECOMPRESSOR_HANDLE::default();
    // SAFETY: as in `compress`; the output buffer is fixed-size and the API
    // never writes past the size passed.
    unsafe {
        CreateDecompressor(ALGORITHM, None, &mut handle).map_err(io::Error::other)?;
        let mut output = vec![0_u8; MAX_MESSAGE_BYTES];
        let mut size = 0;
        let result = Decompress(
            handle,
            Some(data.as_ptr().cast()),
            data.len(),
            Some(output.as_mut_ptr().cast()),
            output.len(),
            Some(&mut size),
        );
        let _ = CloseDecompressor(handle);
        result.map_err(io::Error::other)?;
        output.truncate(size);
        Ok(output)
    }
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// RFC 4648 base64url without padding.
fn base64url(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            text.push(BASE64URL[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    text
}

fn unbase64url(text: &str) -> Option<Vec<u8>> {
    if text.len() % 4 == 1 {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.as_bytes().chunks(4) {
        let mut n = 0_u32;
        for (i, c) in chunk.iter().enumerate() {
            let value = BASE64URL.iter().position(|b| b == c)? as u32;
            n |= value << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            bytes.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use crate::signed::{self, Trust, Verifier};
    use identity::Identity;

    /// An offer the size of a real one (H.264 + Opus, five candidates).
    fn realistic_sdp() -> String {
        let mut sdp = String::from("v=0\r\no=str0m-0.25.0 1 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n");
        for m in [
            "video 9 UDP/TLS/RTP/SAVPF 114 115",
            "audio 9 UDP/TLS/RTP/SAVPF 111",
        ] {
            sdp += &format!("m={m}\r\nc=IN IP4 0.0.0.0\r\n");
            for i in 0..5 {
                sdp += &format!(
                    "a=candidate:h{i} 1 udp {} 192.168.1.{i} 6435{i} typ host ufrag u6Nm1GQa8vO8AJAd\r\n",
                    2_130_706_431 - i
                );
            }
            sdp += "a=ice-ufrag:u6Nm1GQa8vO8AJAd\r\na=ice-pwd:tIV8qs0eFyXXypBYOjjQvg\r\n";
            sdp += "a=fingerprint:sha-256 11:6A:87:31:3E:B6:38:42:44:4D:FE:6A:EC:99:52:1E:E6:1F:91:0E:55:E0:B0:11:4B:E4:DB:30:06:DE:EE:4C\r\n";
            sdp += "a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n";
            sdp += "a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n";
            sdp += "a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r\n";
            sdp += "a=rtcp-fb:114 transport-cc\r\na=rtcp-fb:114 nack\r\na=rtcp-fb:114 nack pli\r\n";
            sdp += "a=ssrc:4184945893 cname:LVLoccxOzyIcGPzACY07\r\n";
        }
        sdp
    }

    fn invite(host: &Identity) -> Envelope {
        let offer = Message::Offer {
            sdp: realistic_sdp(),
            profile: "daniel".into(),
        };
        signed::seal_manual(host, None, offer, None, signed::now())
            .unwrap()
            .0
    }

    #[test]
    fn an_invite_survives_the_trip_and_still_verifies() {
        let host = Identity::generate().unwrap();
        let envelope = invite(&host);
        let text = encode(&envelope).unwrap();
        assert!(text.starts_with(PREFIX));
        let json_len = serde_json::to_vec(&envelope).unwrap().len();
        println!(
            "invite: {json_len} bytes of JSON -> {} characters",
            text.len()
        );
        assert!(text.len() < json_len, "compression helps");

        // A chat app wrapped the text and added spaces.
        let wrapped: String = text
            .chars()
            .collect::<Vec<_>>()
            .chunks(70)
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\r\n ");
        let back = decode(&format!("  {wrapped}\n")).unwrap();
        assert_eq!(back, envelope);
        let viewer = Identity::generate().unwrap();
        let mut verifier = Verifier::new(*viewer.public_key(), Trust::Anyone);
        assert!(verifier.open(&back, signed::now()).is_ok());
    }

    #[test]
    fn damaged_and_foreign_texts_are_refused() {
        let host = Identity::generate().unwrap();
        let text = encode(&invite(&host)).unwrap();
        assert_eq!(decode("hello"), Err(InviteError::NotAnInvite));
        assert_eq!(
            decode(&text.replace("ziz1:", "ziz2:")),
            Err(InviteError::NotAnInvite)
        );
        // Cut short (a message app truncated it).
        assert_eq!(
            decode(&text[..text.len() * 2 / 3]),
            Err(InviteError::Damaged)
        );
        assert_eq!(decode(&format!("{text}!")), Err(InviteError::Damaged));
        assert_eq!(decode(PREFIX), Err(InviteError::Damaged));
        // A changed character either fails to unpack or breaks the signature.
        let mut altered = text.into_bytes();
        let middle = altered.len() / 2;
        altered[middle] = if altered[middle] == b'A' { b'B' } else { b'A' };
        let altered = String::from_utf8(altered).unwrap();
        if let Ok(envelope) = decode(&altered) {
            let viewer = Identity::generate().unwrap();
            let mut verifier = Verifier::new(*viewer.public_key(), Trust::Anyone);
            assert!(verifier.open(&envelope, signed::now()).is_err());
        }
    }

    #[test]
    fn a_crafted_text_cannot_unpack_beyond_the_limit() {
        // A valid compressed stream of more than MAX_MESSAGE_BYTES.
        let huge = compress(&vec![b'a'; MAX_MESSAGE_BYTES * 4]).unwrap();
        assert!(huge.len() < MAX_MESSAGE_BYTES);
        assert!(decompress(&huge).is_err());
        assert_eq!(
            decode(&format!("{PREFIX}{}", base64url(&huge))),
            Err(InviteError::Damaged)
        );
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
    fn mutated_invites_are_refused_without_panicking() {
        let host = Identity::generate().unwrap();
        let text = encode(&invite(&host)).unwrap();
        let viewer = Identity::generate().unwrap();
        let mut verifier = Verifier::new(*viewer.public_key(), Trust::Anyone);
        let mut mutator = Mutator(0x5EED_1234_ABCD_0001);
        let mut opened = 0;
        for _ in 0..3_000 {
            let bytes = mutator.mutate(text.as_bytes());
            if let Ok(envelope) = decode(&String::from_utf8_lossy(&bytes)) {
                // What still unpacks must still carry a valid signature.
                if verifier.open(&envelope, signed::now()).is_ok() {
                    opened += 1;
                }
            }
        }
        // Only mutations that changed nothing that matters (spaces) open,
        // and each invite opens once (replay protection).
        assert!(opened <= 1, "{opened} mutated invites opened");
    }

    #[test]
    fn base64url_matches_rfc_4648() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url(plain.as_bytes()), encoded);
            assert_eq!(unbase64url(encoded).unwrap(), plain.as_bytes());
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert!(unbase64url("Z").is_none());
        assert!(unbase64url("Zm9v+").is_none());
    }
}
