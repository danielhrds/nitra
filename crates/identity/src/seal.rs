//! End-to-end sealing of a small payload (a screen preview) for several
//! people at once, so a relay in the middle (the room server) passes it on
//! without being able to read it.
//!
//! - Each pair of devices shares a secret without any exchange: X25519 of
//!   one's key with the other's (the Ed25519 device keys, seen as X25519
//!   keys), the same from both sides.
//! - The payload is encrypted once (AES-256-GCM) with a new random key; that
//!   key goes wrapped for each recipient with a key derived from the pair's
//!   secret. Only the sender and that recipient can derive it, so a recipient
//!   who opens the payload also knows who made it.
//! - `context` (the room) is bound into both layers: a payload cannot be
//!   moved to another room or passed off as another sender's.
//!
//! AES-GCM and SHA-256 come from Windows (CNG), the curve maths from the
//! crate the device keys already use. Long-term keys, no forward secrecy:
//! fine for low-resolution previews; the live media has its own (DTLS).

use std::io;

use ed25519_dalek::VerifyingKey;
use windows::Win32::Security::Cryptography::{
    BCRYPT_AES_GCM_ALG_HANDLE, BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO,
    BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION, BCRYPT_FLAGS, BCRYPT_KEY_HANDLE,
    BCRYPT_SHA256_ALG_HANDLE, BCryptDecrypt, BCryptDestroyKey, BCryptEncrypt,
    BCryptGenerateSymmetricKey, BCryptHash,
};
use zeroize::Zeroizing;

use crate::{Identity, PublicKey, random};

pub const NONCE_LENGTH: usize = 12;
pub const TAG_LENGTH: usize = 16;
const KEY_LENGTH: usize = 32;
/// A wrapped content key: the key and its tag.
const WRAPPED_LENGTH: usize = KEY_LENGTH + TAG_LENGTH;
const FORMAT_VERSION: u8 = 1;
/// At most this many recipients (a room is small; bounds the parsing).
pub const MAX_RECIPIENTS: usize = 64;

/// A payload sealed for several recipients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    nonce: [u8; NONCE_LENGTH],
    /// The payload encrypted with the content key, then its tag.
    body: Vec<u8>,
    /// The content key wrapped for each recipient.
    keys: Vec<WrappedKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WrappedKey {
    to: PublicKey,
    nonce: [u8; NONCE_LENGTH],
    key: [u8; WRAPPED_LENGTH],
}

impl Identity {
    /// The key this device and `peer` both derive for `context`, or None for
    /// a peer key that is not a usable curve point.
    fn pair_key(
        &self,
        peer: &PublicKey,
        context: &[u8],
        sender: &PublicKey,
    ) -> Option<Zeroizing<[u8; 32]>> {
        let theirs = VerifyingKey::from_bytes(&peer.to_bytes())
            .ok()?
            .to_montgomery();
        let mine = Zeroizing::new(self.signing.to_scalar_bytes());
        let shared = Zeroizing::new(theirs.mul_clamped(*mine).to_bytes());
        // An all-zero secret means a low-order point: nothing secret in it.
        if shared.iter().all(|&b| b == 0) {
            return None;
        }
        // Who sends and who receives is fixed in the derivation, so the two
        // directions of a pair use different keys.
        let recipient = if sender == self.public_key() {
            peer
        } else {
            self.public_key()
        };
        let mut input = Zeroizing::new(Vec::with_capacity(16 + context.len() + 96));
        input.extend_from_slice(b"ziz seal v1\0");
        input.extend_from_slice(&(context.len() as u32).to_be_bytes());
        input.extend_from_slice(context);
        input.extend_from_slice(&*shared);
        input.extend_from_slice(&sender.to_bytes());
        input.extend_from_slice(&recipient.to_bytes());
        sha256(&input).ok().map(Zeroizing::new)
    }
}

/// Seals `payload` from `identity` for `recipients` (the sender can be one
/// of them; nobody else can open it), bound to `context`.
pub fn seal(
    identity: &Identity,
    recipients: &[PublicKey],
    context: &[u8],
    payload: &[u8],
) -> io::Result<Sealed> {
    if recipients.is_empty() || recipients.len() > MAX_RECIPIENTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("1 to {MAX_RECIPIENTS} recipients, got {}", recipients.len()),
        ));
    }
    let content_key = Zeroizing::new(random::<KEY_LENGTH>()?);
    let nonce = random::<NONCE_LENGTH>()?;
    let me = *identity.public_key();
    let body = aes_gcm_seal(&content_key, &nonce, &body_aad(context, &me), payload)?;
    let mut keys = Vec::with_capacity(recipients.len());
    for to in recipients {
        let pair = identity
            .pair_key(to, context, &me)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unusable recipient key"))?;
        let key_nonce = random::<NONCE_LENGTH>()?;
        let wrapped = aes_gcm_seal(&pair, &key_nonce, &nonce, &*content_key)?;
        keys.push(WrappedKey {
            to: *to,
            nonce: key_nonce,
            key: wrapped.try_into().expect("32-byte key plus tag"),
        });
    }
    Ok(Sealed { nonce, body, keys })
}

/// Opens what `sender` sealed for this device in `context`; None if it is
/// not for us, not from `sender`, for another context, or altered.
pub fn open(
    identity: &Identity,
    sender: &PublicKey,
    context: &[u8],
    sealed: &Sealed,
) -> Option<Vec<u8>> {
    let me = identity.public_key();
    let wrapped = sealed.keys.iter().find(|k| k.to == *me)?;
    let pair = identity.pair_key(sender, context, sender)?;
    let content_key = Zeroizing::new(aes_gcm_open(
        &pair,
        &wrapped.nonce,
        &sealed.nonce,
        &wrapped.key,
    )?);
    let content_key: &[u8; KEY_LENGTH] = content_key.as_slice().try_into().ok()?;
    aes_gcm_open(
        content_key,
        &sealed.nonce,
        &body_aad(context, sender),
        &sealed.body,
    )
}

/// What the body is bound to: the context and who made it.
fn body_aad(context: &[u8], sender: &PublicKey) -> Vec<u8> {
    let mut aad = Vec::with_capacity(context.len() + 36);
    aad.extend_from_slice(&(context.len() as u32).to_be_bytes());
    aad.extend_from_slice(context);
    aad.extend_from_slice(&sender.to_bytes());
    aad
}

impl Sealed {
    /// Version, nonce, recipient count, each recipient (key, nonce, wrapped
    /// key), then the body.
    pub fn to_bytes(&self) -> Vec<u8> {
        let per_key = 32 + NONCE_LENGTH + WRAPPED_LENGTH;
        let mut out =
            Vec::with_capacity(2 + NONCE_LENGTH + self.keys.len() * per_key + self.body.len());
        out.push(FORMAT_VERSION);
        out.extend_from_slice(&self.nonce);
        out.push(self.keys.len() as u8);
        for k in &self.keys {
            out.extend_from_slice(&k.to.to_bytes());
            out.extend_from_slice(&k.nonce);
            out.extend_from_slice(&k.key);
        }
        out.extend_from_slice(&self.body);
        out
    }

    /// Parses [`to_bytes`](Self::to_bytes); None if malformed.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (&version, rest) = bytes.split_first()?;
        if version != FORMAT_VERSION {
            return None;
        }
        let (nonce, rest) = rest.split_first_chunk::<NONCE_LENGTH>()?;
        let (&count, mut rest) = rest.split_first()?;
        if count == 0 || usize::from(count) > MAX_RECIPIENTS {
            return None;
        }
        let mut keys = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let (to, r) = rest.split_first_chunk::<32>()?;
            let (key_nonce, r) = r.split_first_chunk::<NONCE_LENGTH>()?;
            let (key, r) = r.split_first_chunk::<WRAPPED_LENGTH>()?;
            keys.push(WrappedKey {
                to: PublicKey::from_bytes(to)?,
                nonce: *key_nonce,
                key: *key,
            });
            rest = r;
        }
        if rest.len() < TAG_LENGTH {
            return None;
        }
        Some(Self {
            nonce: *nonce,
            body: rest.to_vec(),
            keys,
        })
    }

    /// Who it was sealed for.
    pub fn recipients(&self) -> impl Iterator<Item = &PublicKey> {
        self.keys.iter().map(|k| &k.to)
    }
}

fn sha256(input: &[u8]) -> io::Result<[u8; 32]> {
    let mut out = [0u8; 32];
    // SAFETY: CNG's SHA-256 pseudo-handle; in and out slices are live.
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, input, &mut out) }.ok()?;
    Ok(out)
}

/// An AES key handle, destroyed on drop.
struct AesKey(BCRYPT_KEY_HANDLE);

impl AesKey {
    fn new(key: &[u8; KEY_LENGTH]) -> io::Result<Self> {
        let mut handle = BCRYPT_KEY_HANDLE::default();
        // SAFETY: CNG's AES-GCM pseudo-handle; CNG keeps its own copy of the
        // key bytes and owns the key object (no buffer passed).
        unsafe { BCryptGenerateSymmetricKey(BCRYPT_AES_GCM_ALG_HANDLE, &mut handle, None, key, 0) }
            .ok()?;
        Ok(Self(handle))
    }
}

impl Drop for AesKey {
    fn drop(&mut self) {
        // SAFETY: a key handle we created and destroy once.
        let _ = unsafe { BCryptDestroyKey(self.0) };
    }
}

/// The GCM parameters CNG reads through a pointer: nonce, extra
/// authenticated data and the tag (written on encrypt, checked on decrypt).
fn gcm_info(
    nonce: &[u8; NONCE_LENGTH],
    aad: &[u8],
    tag: &mut [u8; TAG_LENGTH],
) -> BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
    BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
        cbSize: size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
        dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
        // CNG does not write through these two (it only reads them).
        pbNonce: nonce.as_ptr().cast_mut(),
        cbNonce: NONCE_LENGTH as u32,
        pbAuthData: aad.as_ptr().cast_mut(),
        cbAuthData: aad.len() as u32,
        pbTag: tag.as_mut_ptr(),
        cbTag: TAG_LENGTH as u32,
        ..Default::default()
    }
}

/// AES-256-GCM: the ciphertext followed by the tag.
fn aes_gcm_seal(
    key: &[u8; KEY_LENGTH],
    nonce: &[u8; NONCE_LENGTH],
    aad: &[u8],
    plain: &[u8],
) -> io::Result<Vec<u8>> {
    let key = AesKey::new(key)?;
    let mut tag = [0u8; TAG_LENGTH];
    let info = gcm_info(nonce, aad, &mut tag);
    let mut out = vec![0u8; plain.len()];
    let mut written = 0u32;
    // SAFETY: valid key; `info` points at live nonce, aad and tag buffers
    // for the call; GCM is a stream mode, so the output is as long as the input.
    unsafe {
        BCryptEncrypt(
            key.0,
            Some(plain),
            Some((&raw const info).cast()),
            None,
            Some(&mut out),
            &mut written,
            BCRYPT_FLAGS(0),
        )
    }
    .ok()?;
    out.truncate(written as usize);
    out.extend_from_slice(&tag);
    Ok(out)
}

/// Opens [`aes_gcm_seal`]'s output; None if anything does not check out.
fn aes_gcm_open(
    key: &[u8; KEY_LENGTH],
    nonce: &[u8; NONCE_LENGTH],
    aad: &[u8],
    sealed: &[u8],
) -> Option<Vec<u8>> {
    let (cipher, tag) = sealed.split_last_chunk::<TAG_LENGTH>()?;
    let key = AesKey::new(key).ok()?;
    let mut tag = *tag;
    let info = gcm_info(nonce, aad, &mut tag);
    let mut out = vec![0u8; cipher.len()];
    let mut written = 0u32;
    // SAFETY: as in `aes_gcm_seal`; a wrong tag makes the call fail.
    unsafe {
        BCryptDecrypt(
            key.0,
            Some(cipher),
            Some((&raw const info).cast()),
            None,
            Some(&mut out),
            &mut written,
            BCRYPT_FLAGS(0),
        )
    }
    .ok()
    .ok()?;
    out.truncate(written as usize);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM: &[u8] = b"room-abc";

    #[test]
    fn every_recipient_opens_it_and_nobody_else() {
        let (ana, bia, caio, eve) = (
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
        );
        let payload = b"a small jpeg".repeat(100);
        let sealed = seal(
            &ana,
            &[*bia.public_key(), *caio.public_key()],
            ROOM,
            &payload,
        )
        .unwrap();
        // Through bytes, as it travels.
        let sealed = Sealed::from_bytes(&sealed.to_bytes()).unwrap();
        assert_eq!(
            open(&bia, ana.public_key(), ROOM, &sealed).as_deref(),
            Some(&payload[..])
        );
        assert_eq!(
            open(&caio, ana.public_key(), ROOM, &sealed).as_deref(),
            Some(&payload[..])
        );
        // Not a recipient.
        assert_eq!(open(&eve, ana.public_key(), ROOM, &sealed), None);
        // Another room, or passed off as someone else's.
        assert_eq!(open(&bia, ana.public_key(), b"room-xyz", &sealed), None);
        assert_eq!(open(&bia, caio.public_key(), ROOM, &sealed), None);
    }

    #[test]
    fn any_change_is_refused() {
        let (ana, bia) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let bytes = seal(&ana, &[*bia.public_key()], ROOM, b"preview")
            .unwrap()
            .to_bytes();
        for i in 0..bytes.len() {
            let mut altered = bytes.clone();
            altered[i] ^= 0x01;
            let opened =
                Sealed::from_bytes(&altered).and_then(|s| open(&bia, ana.public_key(), ROOM, &s));
            assert_eq!(opened, None, "byte {i} changed and it still opened");
        }
        // Cut short.
        for len in 0..bytes.len() {
            let opened = Sealed::from_bytes(&bytes[..len])
                .and_then(|s| open(&bia, ana.public_key(), ROOM, &s));
            assert_eq!(opened, None, "cut at {len}");
        }
    }

    #[test]
    fn the_pair_key_is_the_same_from_both_sides() {
        let (ana, bia) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let from_ana = ana
            .pair_key(bia.public_key(), ROOM, ana.public_key())
            .unwrap();
        let from_bia = bia
            .pair_key(ana.public_key(), ROOM, ana.public_key())
            .unwrap();
        assert_eq!(*from_ana, *from_bia);
        // The other direction uses another key.
        let back = bia
            .pair_key(ana.public_key(), ROOM, bia.public_key())
            .unwrap();
        assert_ne!(*from_ana, *back);
    }

    #[test]
    fn aes_gcm_matches_a_known_vector() {
        // NIST GCM test case 13 (AES-256, zero key and nonce, empty input):
        // only the tag.
        let tag = aes_gcm_seal(&[0; 32], &[0; 12], &[], &[]).unwrap();
        assert_eq!(
            tag,
            [
                0x53, 0x0f, 0x8a, 0xfb, 0xc7, 0x45, 0x36, 0xb9, 0xa9, 0x63, 0xb4, 0xf1, 0xc4, 0xcb,
                0x73, 0x8b
            ]
        );
        // Test case 14: one zero block.
        let out = aes_gcm_seal(&[0; 32], &[0; 12], &[], &[0; 16]).unwrap();
        assert_eq!(
            &out[..16],
            [
                0xce, 0xa7, 0x40, 0x3d, 0x4d, 0x60, 0x6b, 0x6e, 0x07, 0x4e, 0xc5, 0xd3, 0xba, 0xf3,
                0x9d, 0x18
            ]
        );
        assert_eq!(
            &out[16..],
            [
                0xd0, 0xd1, 0xc8, 0xa7, 0x99, 0x99, 0x6b, 0xf0, 0x26, 0x5b, 0x98, 0xb5, 0xd4, 0x8a,
                0xb9, 0x19
            ]
        );
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        assert_eq!(sha256(b"abc").unwrap()[..4], [0xba, 0x78, 0x16, 0xbf]);
    }

    #[test]
    fn x25519_matches_rfc_7748() {
        // RFC 7748 section 6.1: Alice's private key and Bob's public key give
        // the shared secret below (the curve maths used for the pair keys).
        let alice: [u8; 32] =
            hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob_public = curve25519_dalek::MontgomeryPoint(hex(
            "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
        ));
        assert_eq!(
            bob_public.mul_clamped(alice).to_bytes(),
            hex::<32>("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742")
        );
    }

    fn hex<const N: usize>(text: &str) -> [u8; N] {
        let mut out = [0u8; N];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }
}
