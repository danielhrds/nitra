//! Conversions for fixed-size UTF-16 buffers returned by Windows APIs.

/// Converts a NUL-terminated (or full) UTF-16 buffer to a `String`,
/// replacing invalid code units.
pub fn wide_to_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn stops_at_first_nul() {
        let mut buffer = wide("AMD Radeon RX 7600");
        buffer.extend([0, b'x'.into(), 0]);
        assert_eq!(wide_to_string(&buffer), "AMD Radeon RX 7600");
    }

    #[test]
    fn uses_whole_buffer_without_nul() {
        assert_eq!(wide_to_string(&wide(r"\\.\DISPLAY1")), r"\\.\DISPLAY1");
    }

    #[test]
    fn empty_and_all_nul() {
        assert_eq!(wide_to_string(&[]), "");
        assert_eq!(wide_to_string(&[0; 8]), "");
    }
}
