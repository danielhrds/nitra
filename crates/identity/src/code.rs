//! Key codes: bytes in base32 (RFC 4648 alphabet, lowercase, no padding),
//! grouped in fours with dashes so people can read and compare them. The
//! alphabet has no 0/1/8/9, which avoids most confusions with O, I, L, B, g.

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

pub fn encode(bytes: &[u8]) -> String {
    let mut plain = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let (mut buffer, mut bits) = (0_u32, 0);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            plain.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        plain.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    let mut grouped = String::with_capacity(plain.len() * 5 / 4);
    for (i, c) in plain.chars().enumerate() {
        if i > 0 && i % 4 == 0 {
            grouped.push('-');
        }
        grouped.push(c);
    }
    grouped
}

/// Decodes a code, ignoring dashes, spaces and case. `None` for any other
/// character or for leftover bits that are not zero (a mistyped last
/// character).
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * 5 / 8);
    let (mut buffer, mut bits) = (0_u32, 0);
    for c in text.chars().filter(|c| *c != '-' && !c.is_whitespace()) {
        let lower = c.to_ascii_lowercase() as u8;
        let value = ALPHABET.iter().position(|&a| a == lower)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
        }
        buffer &= (1 << bits) - 1;
    }
    (buffer == 0).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4648_vectors() {
        // RFC 4648 section 10, lowercase and without padding, grouped.
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "my");
        assert_eq!(encode(b"fo"), "mzxq");
        assert_eq!(encode(b"foo"), "mzxw-6");
        assert_eq!(encode(b"foob"), "mzxw-6yq");
        assert_eq!(encode(b"fooba"), "mzxw-6ytb");
        assert_eq!(encode(b"foobar"), "mzxw-6ytb-oi");
        for text in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(decode(&encode(text.as_bytes())).unwrap(), text.as_bytes());
        }
    }

    #[test]
    fn bad_characters_and_trailing_bits_are_rejected() {
        assert!(decode("mzxw-0ytb").is_none(), "0 is not in the alphabet");
        assert!(decode("mz").is_none(), "non-zero leftover bits");
        assert_eq!(decode("MZXW 6YTB OI").unwrap(), b"foobar");
    }
}
