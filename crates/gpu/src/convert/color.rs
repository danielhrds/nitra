//! CPU reference for the RGB to YCbCr conversion both converters perform.

/// Converts 8-bit gamma-encoded RGB to 8-bit BT.709 limited-range YCbCr
/// ("studio swing": Y in 16..=235, Cb/Cr in 16..=240), rounding to nearest.
pub fn bt709_limited([r, g, b]: [u8; 3]) -> [u8; 3] {
    let (r, g, b) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - luma) / 1.8556;
    let cr = (r - luma) / 1.5748;
    let quantize = |value: f64| value.round().clamp(0.0, 255.0) as u8;
    [
        quantize(16.0 + 219.0 * luma),
        quantize(128.0 + 224.0 * cb),
        quantize(128.0 + 224.0 * cr),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_values() {
        // Published BT.709 limited-range values for 100% colors.
        assert_eq!(bt709_limited([0, 0, 0]), [16, 128, 128]);
        assert_eq!(bt709_limited([255, 255, 255]), [235, 128, 128]);
        assert_eq!(bt709_limited([255, 0, 0]), [63, 102, 240]);
        assert_eq!(bt709_limited([0, 255, 0]), [173, 42, 26]);
        assert_eq!(bt709_limited([0, 0, 255]), [32, 240, 118]);
    }

    #[test]
    fn grays_have_neutral_chroma() {
        for level in [0, 64, 128, 200, 255] {
            let [_, cb, cr] = bt709_limited([level, level, level]);
            assert_eq!((cb, cr), (128, 128), "level {level}");
        }
    }
}
