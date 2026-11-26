//! Device identity: each profile has an Ed25519 key pair, created the first
//! time it runs. The private key never leaves the PC: it is stored with
//! DPAPI (only this Windows user on this PC can read it back) and used to
//! sign signaling messages. The public key is what others see and trust,
//! shown as a [`PublicKey`] code people compare or paste.

mod code;
mod dpapi;
pub mod release;
pub mod seal;

use std::fmt;
use std::io;
use std::path::Path;
use std::str::FromStr;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use zeroize::Zeroizing;

/// Length of an Ed25519 signature in bytes.
pub const SIGNATURE_LENGTH: usize = 64;

/// Names the protected data and keeps the DPAPI blob from being accepted
/// by any other program that happens to use DPAPI.
const DPAPI_ENTROPY: &[u8] = b"nitra identity key v1";

/// This device's key pair.
pub struct Identity {
    signing: SigningKey,
    public: PublicKey,
}

impl Identity {
    /// Loads the key stored in `path`, creating and storing a new one if
    /// the file does not exist. A file that exists but cannot be read back
    /// (corrupted, or copied from another user or PC) is an error, not a
    /// reason to make a new key silently: that would change who we are.
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(blob) => {
                let secret = dpapi::unprotect(&blob, DPAPI_ENTROPY).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "cannot read the identity key {} ({e}); delete it to make a new identity",
                            path.display()
                        ),
                    )
                })?;
                let secret: Zeroizing<[u8; 32]> =
                    Zeroizing::new(secret.as_slice().try_into().map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("identity key {} has the wrong length", path.display()),
                        )
                    })?);
                Ok(Self::from_secret(&secret))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                let secret = Zeroizing::new(identity.signing.to_bytes());
                let blob = dpapi::protect(secret.as_slice(), DPAPI_ENTROPY)?;
                // Write next to it, then rename: a crash never leaves half a key.
                let temporary = path.with_extension("tmp");
                std::fs::write(&temporary, &blob)?;
                std::fs::rename(&temporary, path)?;
                Ok(identity)
            }
            Err(e) => Err(e),
        }
    }

    /// A new random key pair (not stored).
    pub fn generate() -> io::Result<Self> {
        let secret = Zeroizing::new(random::<32>()?);
        Ok(Self::from_secret(&secret))
    }

    pub(crate) fn from_secret(secret: &[u8; 32]) -> Self {
        // SigningKey zeroizes its secret when dropped.
        let signing = SigningKey::from_bytes(secret);
        let public = PublicKey(signing.verifying_key().to_bytes());
        Self { signing, public }
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.public
    }

    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LENGTH] {
        self.signing.sign(message).to_bytes()
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the private key.
        f.debug_struct("Identity")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// Someone's public key. Its text form, the key code, is the whole key in
/// base32 (52 characters in groups of four), so a code that was pasted is
/// all it takes to check that person's signatures.
///
/// Holds the 32 encoded bytes (checked to be a valid key when made) rather
/// than the decoded curve point, so it stays small to copy around; decoding
/// again to verify costs microseconds, and signaling verifies rarely.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    pub fn from_bytes(bytes: &[u8; 32]) -> Option<Self> {
        VerifyingKey::from_bytes(bytes).ok().map(|_| Self(*bytes))
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// True if `signature` is this key's signature of `message`. Strict
    /// verification: rejects the malleable and weak-key forms Ed25519 allows.
    pub fn verify(&self, message: &[u8], signature: &[u8; SIGNATURE_LENGTH]) -> bool {
        VerifyingKey::from_bytes(&self.0).is_ok_and(|key| {
            key.verify_strict(message, &Signature::from_bytes(signature))
                .is_ok()
        })
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&code::encode(&self.to_bytes()))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

impl FromStr for PublicKey {
    type Err = String;

    /// Accepts the code with or without dashes and spaces, in any case.
    fn from_str(text: &str) -> Result<Self, String> {
        let bytes = code::decode(text).ok_or("not a key code")?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "a key code has 52 characters")?;
        Self::from_bytes(&bytes).ok_or_else(|| "not a valid key".to_owned())
    }
}

/// `N` random bytes from the system's cryptographic generator.
pub fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    // SAFETY: the buffer is valid for writes of its whole length; no
    // algorithm handle is needed with the system-preferred flag.
    unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .map_err(io::Error::other)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nitra-identity-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn signatures_verify_only_for_the_same_key_and_message() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let signature = alice.sign(b"hello");
        assert!(alice.public_key().verify(b"hello", &signature));
        assert!(!alice.public_key().verify(b"hellO", &signature));
        assert!(!bob.public_key().verify(b"hello", &signature));
        let mut forged = signature;
        forged[10] ^= 1;
        assert!(!alice.public_key().verify(b"hello", &forged));
    }

    #[test]
    fn key_codes_round_trip_and_tolerate_formatting() {
        let key = *Identity::generate().unwrap().public_key();
        let code = key.to_string();
        assert_eq!(code.len(), 52 + 12, "13 groups of 4 with dashes");
        assert_eq!(code.parse::<PublicKey>().unwrap(), key);
        let loose = code.replace('-', " ").to_uppercase();
        assert_eq!(loose.parse::<PublicKey>().unwrap(), key);
        assert!(code[..40].parse::<PublicKey>().is_err(), "too short");
        assert!("not a key at all".parse::<PublicKey>().is_err());
    }

    #[test]
    fn the_stored_key_comes_back_and_is_protected() {
        let path = temp_path("round-trip.key");
        let _ = std::fs::remove_file(&path);
        let created = Identity::load_or_create(&path).unwrap();
        let loaded = Identity::load_or_create(&path).unwrap();
        assert_eq!(created.public_key(), loaded.public_key());
        // The file is a DPAPI blob, not the raw 32-byte secret.
        let blob = std::fs::read(&path).unwrap();
        let secret = created.signing.to_bytes();
        assert!(blob.len() > 32);
        assert!(!blob.windows(32).any(|w| w == secret));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_corrupted_key_file_is_an_error_not_a_new_identity() {
        let path = temp_path("corrupted.key");
        let _ = std::fs::remove_file(&path);
        Identity::load_or_create(&path).unwrap();
        let mut blob = std::fs::read(&path).unwrap();
        let middle = blob.len() / 2;
        blob[middle] ^= 0xFF;
        std::fs::write(&path, &blob).unwrap();
        let error = Identity::load_or_create(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), blob, "file left as it was");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn random_bytes_differ() {
        assert_ne!(random::<16>().unwrap(), random::<16>().unwrap());
    }
}
