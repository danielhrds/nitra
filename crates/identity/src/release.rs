//! The release key: the Ed25519 key that signs each release's checksums, so
//! the app installs an update only when the release comes from whoever holds
//! it. Its public half is built into the app; the private half lives on the
//! maintainer's PC, in two forms:
//!
//! - the working copy, protected with DPAPI (only that Windows user on that
//!   PC can read it), which signs releases;
//! - a backup file sealed with a passphrase (PBKDF2-SHA256, then
//!   AES-256-GCM), which works on any PC and is what survives a reinstall.
//!
//! Neither form is ever the raw key on disk.

use std::io;
use std::path::Path;

use windows::Win32::Security::Cryptography::{
    BCRYPT_HMAC_SHA256_ALG_HANDLE, BCryptDeriveKeyPBKDF2,
};
use zeroize::Zeroizing;

use crate::seal::{NONCE_LENGTH, aes_gcm_open, aes_gcm_seal};
use crate::{Identity, PublicKey, SIGNATURE_LENGTH, dpapi, random};

/// Keeps the working copy's DPAPI blob apart from a device key's.
const DPAPI_ENTROPY: &[u8] = b"nitra release key v1";
/// What every signed message starts with: a release signature can never be
/// taken for a signature of anything else (and the other way round).
const SIGNING_CONTEXT: &[u8] = b"nitra release v1\0";
/// The backup file's first line.
const BACKUP_HEADER: &str = "nitra release key backup v1";
const SALT_LENGTH: usize = 16;
/// PBKDF2-SHA256 rounds for the backup's passphrase (OWASP's 2023 advice).
const PBKDF2_ROUNDS: u64 = 600_000;
/// A passphrase shorter than this is refused when making a backup.
pub const MIN_PASSPHRASE_LENGTH: usize = 12;

/// The key that signs releases.
pub struct ReleaseKey(Identity);

impl ReleaseKey {
    /// A new random key.
    pub fn generate() -> io::Result<Self> {
        Identity::generate().map(Self)
    }

    pub fn public_key(&self) -> &PublicKey {
        self.0.public_key()
    }

    /// The signature of a release file (its `SHA256SUMS.txt`).
    pub fn sign(&self, file: &[u8]) -> [u8; SIGNATURE_LENGTH] {
        self.0.sign(&signed_message(file))
    }

    /// Writes the working copy (DPAPI) to `path`; refuses to replace one.
    pub fn store(&self, path: &Path) -> io::Result<()> {
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", path.display()),
            ));
        }
        let secret = Zeroizing::new(self.0.signing.to_bytes());
        let blob = dpapi::protect(secret.as_slice(), DPAPI_ENTROPY)?;
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, &blob)?;
        std::fs::rename(&temporary, path)
    }

    /// Reads the working copy written by [`store`](Self::store).
    pub fn load(path: &Path) -> io::Result<Self> {
        let blob = std::fs::read(path)?;
        let secret = dpapi::unprotect(&blob, DPAPI_ENTROPY).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cannot read the release key {} ({e}); restore it from the backup",
                    path.display()
                ),
            )
        })?;
        let secret: Zeroizing<[u8; 32]> = Zeroizing::new(
            secret
                .as_slice()
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "wrong key length"))?,
        );
        Ok(Self(Identity::from_secret(&secret)))
    }

    /// The backup file's text: a header line, the public key (to recognise
    /// it), then salt, nonce and the sealed key in hex.
    pub fn backup(&self, passphrase: &str) -> io::Result<String> {
        if passphrase.chars().count() < MIN_PASSPHRASE_LENGTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the passphrase needs at least {MIN_PASSPHRASE_LENGTH} characters"),
            ));
        }
        let salt = random::<SALT_LENGTH>()?;
        let nonce = random::<NONCE_LENGTH>()?;
        let key = passphrase_key(passphrase, &salt)?;
        let secret = Zeroizing::new(self.0.signing.to_bytes());
        let sealed = aes_gcm_seal(&key, &nonce, BACKUP_HEADER.as_bytes(), secret.as_slice())?;
        Ok(format!(
            "{BACKUP_HEADER}\npublic key: {}\n{}{}{}\n",
            self.public_key(),
            hex(&salt),
            hex(&nonce),
            hex(&sealed)
        ))
    }

    /// Opens a backup made by [`backup`](Self::backup). A wrong passphrase
    /// and a damaged file give the same error (GCM cannot tell them apart).
    pub fn from_backup(text: &str, passphrase: &str) -> io::Result<Self> {
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        if lines.next() != Some(BACKUP_HEADER) {
            return Err(invalid("not a nitra release key backup"));
        }
        let expected: PublicKey = lines
            .next()
            .and_then(|l| l.strip_prefix("public key:"))
            .and_then(|code| code.trim().parse().ok())
            .ok_or_else(|| invalid("the backup has no public key line"))?;
        let bytes = lines
            .next()
            .and_then(unhex)
            .ok_or_else(|| invalid("the backup's key line is damaged"))?;
        let (salt, rest) = bytes
            .split_first_chunk::<SALT_LENGTH>()
            .ok_or_else(|| invalid("the backup is too short"))?;
        let (nonce, sealed) = rest
            .split_first_chunk::<NONCE_LENGTH>()
            .ok_or_else(|| invalid("the backup is too short"))?;
        let key = passphrase_key(passphrase, salt)?;
        let secret = Zeroizing::new(
            aes_gcm_open(&key, nonce, BACKUP_HEADER.as_bytes(), sealed)
                .ok_or_else(|| invalid("wrong passphrase, or a damaged backup"))?,
        );
        let secret: Zeroizing<[u8; 32]> = Zeroizing::new(
            secret
                .as_slice()
                .try_into()
                .map_err(|_| invalid("wrong key length"))?,
        );
        let key = Self(Identity::from_secret(&secret));
        if *key.public_key() != expected {
            return Err(invalid(
                "the backup's key does not match its public key line",
            ));
        }
        Ok(key)
    }
}

/// The SHA-256 of `bytes` (Windows' CNG), as a release lists it.
pub fn sha256(bytes: &[u8]) -> io::Result<[u8; 32]> {
    crate::seal::sha256(bytes)
}

/// True if `signature` is `key`'s release signature of `file`.
pub fn verify(key: &PublicKey, file: &[u8], signature: &[u8; SIGNATURE_LENGTH]) -> bool {
    key.verify(&signed_message(file), signature)
}

fn signed_message(file: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(SIGNING_CONTEXT.len() + file.len());
    message.extend_from_slice(SIGNING_CONTEXT);
    message.extend_from_slice(file);
    message
}

/// The AES key for a passphrase and salt (PBKDF2-HMAC-SHA256).
fn passphrase_key(passphrase: &str, salt: &[u8]) -> io::Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0u8; 32]);
    // SAFETY: CNG's HMAC-SHA256 pseudo-handle; the passphrase, salt and
    // output slices are live for the call.
    unsafe {
        BCryptDeriveKeyPBKDF2(
            BCRYPT_HMAC_SHA256_ALG_HANDLE,
            Some(passphrase.as_bytes()),
            Some(salt),
            PBKDF2_ROUNDS,
            key.as_mut_slice(),
            0,
        )
    }
    .ok()
    .map_err(io::Error::other)?;
    Ok(key)
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses [`hex`] (either case); None if malformed.
pub fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSPHRASE: &str = "correct horse battery staple";

    #[test]
    fn signatures_check_the_file_and_the_key() {
        let key = ReleaseKey::generate().unwrap();
        let other = ReleaseKey::generate().unwrap();
        let file = b"abc123  nitra.exe\n";
        let signature = key.sign(file);
        assert!(verify(key.public_key(), file, &signature));
        assert!(!verify(
            key.public_key(),
            b"abc124  nitra.exe\n",
            &signature
        ));
        assert!(!verify(other.public_key(), file, &signature));
        // A device signature of the same bytes is not a release signature.
        assert!(!verify(key.public_key(), file, &key.0.sign(file)));
    }

    #[test]
    fn the_backup_needs_its_passphrase() {
        let key = ReleaseKey::generate().unwrap();
        let text = key.backup(PASSPHRASE).unwrap();
        assert!(text.starts_with(BACKUP_HEADER));
        let secret = hex(&key.0.signing.to_bytes());
        assert!(
            !text.contains(&secret),
            "the backup never holds the raw key"
        );
        let back = ReleaseKey::from_backup(&text, PASSPHRASE).unwrap();
        assert_eq!(back.public_key(), key.public_key());
        let wrong = ReleaseKey::from_backup(&text, "correct horse battery stapler");
        assert_eq!(wrong.err().unwrap().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_damaged_backup_or_a_short_passphrase_is_refused() {
        let key = ReleaseKey::generate().unwrap();
        assert!(key.backup("too short").is_err());
        let text = key.backup(PASSPHRASE).unwrap();
        let mut damaged: Vec<char> = text.chars().collect();
        let at = damaged.len() - 10;
        damaged[at] = if damaged[at] == '0' { '1' } else { '0' };
        let damaged: String = damaged.into_iter().collect();
        assert!(ReleaseKey::from_backup(&damaged, PASSPHRASE).is_err());
        assert!(ReleaseKey::from_backup("hello", PASSPHRASE).is_err());
    }

    #[test]
    fn the_working_copy_comes_back_and_is_never_replaced() {
        let dir = std::env::temp_dir().join(format!("nitra-release-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("release.key");
        let _ = std::fs::remove_file(&path);
        let key = ReleaseKey::generate().unwrap();
        key.store(&path).unwrap();
        assert_eq!(
            ReleaseKey::load(&path).unwrap().public_key(),
            key.public_key()
        );
        let blob = std::fs::read(&path).unwrap();
        assert!(!blob.windows(32).any(|w| w == key.0.signing.to_bytes()));
        let again = ReleaseKey::generate().unwrap().store(&path);
        assert_eq!(again.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(
            unhex(&hex(&[0, 1, 0xab, 0xff])).unwrap(),
            [0, 1, 0xab, 0xff]
        );
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }
}
