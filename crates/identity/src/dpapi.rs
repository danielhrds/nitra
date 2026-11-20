//! DPAPI (CryptProtectData): encrypts data with a key Windows derives from
//! the user's logon credentials, so only this user on this PC can decrypt
//! it. Copying the file to another PC or user does not reveal the secret.

use std::io;

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;
use zeroize::Zeroizing;

pub fn protect(data: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
    let input = blob(data);
    let entropy = blob(entropy);
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: the input blobs point to live slices that DPAPI only reads;
    // `output` receives a LocalAlloc buffer that `take` copies and frees.
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(io::Error::other)?;
        Ok(take(output).to_vec())
    }
}

/// The decrypted data, wiped from memory when dropped.
pub fn unprotect(data: &[u8], entropy: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
    let input = blob(data);
    let entropy = blob(entropy);
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(io::Error::other)?;
        Ok(take(output))
    }
}

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        // DPAPI takes a mutable pointer but does not write input blobs.
        pbData: data.as_ptr().cast_mut(),
    }
}

/// Copies a DPAPI output buffer, then wipes and frees it.
///
/// # Safety
/// `output` was filled by a successful DPAPI call (a LocalAlloc buffer of
/// `cbData` bytes, or null).
unsafe fn take(output: CRYPT_INTEGER_BLOB) -> Zeroizing<Vec<u8>> {
    if output.pbData.is_null() {
        return Zeroizing::new(Vec::new());
    }
    // SAFETY: per the contract the buffer holds `cbData` bytes.
    let bytes = unsafe { std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize) };
    let copy = Zeroizing::new(bytes.to_vec());
    zeroize::Zeroize::zeroize(bytes);
    // SAFETY: the buffer came from LocalAlloc and is not used after this.
    unsafe { LocalFree(Some(HLOCAL(output.pbData.cast()))) };
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_needs_the_same_entropy() {
        let protected = protect(b"secret bytes", b"purpose").unwrap();
        assert_ne!(protected.as_slice(), b"secret bytes");
        assert_eq!(
            unprotect(&protected, b"purpose").unwrap().as_slice(),
            b"secret bytes"
        );
        assert!(unprotect(&protected, b"other purpose").is_err());
        assert!(unprotect(b"not a DPAPI blob", b"purpose").is_err());
    }
}
