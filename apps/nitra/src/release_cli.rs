//! The maintainer's release-key commands (hidden `nitra-cli` options; see
//! `identity::release` for what the key is and how it is kept):
//!
//! ```text
//! nitra-cli --release-keygen DIR                              (once)
//! nitra-cli --release-sign SHA256SUMS.txt --release-key DIR\release.key
//! nitra-cli --release-verify SHA256SUMS.txt                  (against the built-in key)
//! nitra-cli --release-key-restore BACKUP --release-key DIR\release.key
//! ```
//!
//! The passphrase is typed in the console with echo off; it is never an
//! argument (it would stay in the shell's history). `NITRA_RELEASE_PASSPHRASE`
//! stands in for typing in a development build only (scripted tests).

use std::error::Error;
use std::io::{self, BufRead, Write};
use std::path::Path;

use identity::release::{MIN_PASSPHRASE_LENGTH, ReleaseKey, hex, unhex, verify};
use identity::{PublicKey, SIGNATURE_LENGTH};
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};

/// The working copy's and the backup's file names in the keygen folder.
pub const KEY_FILE: &str = "release.key";
pub const BACKUP_FILE: &str = "release-key-backup.txt";
pub const PUBLIC_FILE: &str = "release-key.pub";

/// Makes the key: the working copy, the passphrase-sealed backup and the
/// public key, in `dir` (which must not hold a key already).
pub fn keygen(dir: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(dir)?;
    let key_path = dir.join(KEY_FILE);
    if key_path.exists() {
        return Err(format!("{} already exists: nothing changed", key_path.display()).into());
    }
    println!(
        "A passphrase protects the backup file (at least {MIN_PASSPHRASE_LENGTH} characters). \
         Keep it somewhere safe: the backup is useless without it."
    );
    let passphrase = read_passphrase("Passphrase: ")?;
    if !from_environment() && read_passphrase("Again: ")? != passphrase {
        return Err("the two passphrases differ: nothing changed".into());
    }
    let key = ReleaseKey::generate()?;
    let backup = key.backup(&passphrase)?;
    key.store(&key_path)?;
    std::fs::write(dir.join(BACKUP_FILE), backup)?;
    std::fs::write(dir.join(PUBLIC_FILE), format!("{}\n", key.public_key()))?;
    println!("public key: {}", key.public_key());
    println!("working copy: {}", key_path.display());
    println!(
        "backup: {} (move it off this PC, then delete it here)",
        dir.join(BACKUP_FILE).display()
    );
    Ok(())
}

/// Writes `file.sig`: the release signature of `file`.
pub fn sign(file: &Path, key_path: &Path) -> Result<(), Box<dyn Error>> {
    let key = ReleaseKey::load(key_path)?;
    let bytes = std::fs::read(file)?;
    let signature = key.sign(&bytes);
    let sig_path = signature_path(file);
    std::fs::write(&sig_path, format!("{}\n", hex(&signature)))?;
    println!("signed with {}: {}", key.public_key(), sig_path.display());
    Ok(())
}

/// Checks `file.sig` against `key` (the built-in one for release builds).
pub fn check(file: &Path, key: &PublicKey) -> Result<(), Box<dyn Error>> {
    let bytes = std::fs::read(file)?;
    let signature = read_signature(&signature_path(file))?;
    if verify(key, &bytes, &signature) {
        println!("signature ok ({key})");
        Ok(())
    } else {
        Err(format!("the signature of {} does not match {key}", file.display()).into())
    }
}

/// Makes the working copy again from the backup (after a reinstall).
pub fn restore(backup: &Path, key_path: &Path) -> Result<(), Box<dyn Error>> {
    let text = std::fs::read_to_string(backup)?;
    let passphrase = read_passphrase("Passphrase: ")?;
    let key = ReleaseKey::from_backup(&text, &passphrase)?;
    if let Some(dir) = key_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    key.store(key_path)?;
    println!("restored {} ({})", key_path.display(), key.public_key());
    Ok(())
}

pub fn signature_path(file: &Path) -> std::path::PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".sig");
    name.into()
}

/// A `.sig` file: the signature in hex.
pub fn read_signature(path: &Path) -> Result<[u8; SIGNATURE_LENGTH], Box<dyn Error>> {
    let text = std::fs::read_to_string(path)?;
    unhex(&text)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| format!("{} is not a signature", path.display()).into())
}

fn from_environment() -> bool {
    !crate::BETA && std::env::var_os("NITRA_RELEASE_PASSPHRASE").is_some()
}

/// One line typed with echo off (or `NITRA_RELEASE_PASSPHRASE` in a
/// development build).
fn read_passphrase(prompt: &str) -> io::Result<String> {
    if from_environment() {
        return std::env::var("NITRA_RELEASE_PASSPHRASE").map_err(io::Error::other);
    }
    print!("{prompt}");
    io::stdout().flush()?;
    // SAFETY: the process's own standard input handle; the console mode is
    // read before it is changed and restored right after the line is read.
    let (handle, mode) = unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE).map_err(io::Error::other)?;
        let mut mode = CONSOLE_MODE::default();
        GetConsoleMode(handle, &mut mode).map_err(|_| {
            io::Error::other("type the passphrase in a console (input is not a console)")
        })?;
        SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT).map_err(io::Error::other)?;
        (handle, mode)
    };
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    // SAFETY: the same handle; puts the mode back as it was.
    let _ = unsafe { SetConsoleMode(handle, mode) };
    println!();
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}
