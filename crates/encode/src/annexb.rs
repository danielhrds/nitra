//! Minimal H.264 Annex B inspection: split a byte stream into NAL units and
//! read what tests and reports need (NAL type, slice type). Not a decoder.

/// NAL unit types used here (ITU-T H.264, table 7-1).
pub mod nal_type {
    pub const NON_IDR_SLICE: u8 = 1;
    pub const IDR_SLICE: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const ACCESS_UNIT_DELIMITER: u8 = 9;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

/// Where one NAL unit sits in a stream: its start code begins at `code`,
/// its payload spans `payload..end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NalSpan {
    code: usize,
    payload: usize,
    end: usize,
}

/// NAL units of `stream` (3- or 4-byte start codes), empty ones skipped.
fn nal_spans(stream: &[u8]) -> Vec<NalSpan> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= stream.len() {
        if stream[index] == 0 && stream[index + 1] == 0 && stream[index + 2] == 1 {
            // A 4-byte start code has one more zero in front.
            let code = if index > 0 && stream[index - 1] == 0 {
                index - 1
            } else {
                index
            };
            starts.push((code, index + 3));
            index += 3;
        } else {
            index += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(position, &(code, payload))| {
            let mut end = starts
                .get(position + 1)
                .map_or(stream.len(), |&(next, _)| next);
            // Trailing zeros belong to no NAL unit (`trailing_zero_8bits`).
            while end > payload && stream[end - 1] == 0 && starts.get(position + 1).is_some() {
                end -= 1;
            }
            NalSpan { code, payload, end }
        })
        .filter(|span| span.end > span.payload)
        .collect()
}

/// NAL unit payloads (without start codes) in `stream`, which uses 3- or
/// 4-byte start codes (`00 00 01`, `00 00 00 01`).
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    nal_spans(stream)
        .into_iter()
        .map(|span| &stream[span.payload..span.end])
        .collect()
}

/// Splits a concatenated Annex B stream (e.g. a `.h264` file) into access
/// units, one per picture, start codes included. A new access unit begins
/// at an access unit delimiter, SEI, SPS or PPS, or at a slice whose
/// `first_mb_in_slice` is 0, once the current one already has a slice
/// (H.264 7.4.1.2.3, simplified for single-layer streams).
pub fn access_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start: Option<usize> = None;
    let mut has_slice = false;
    for span in nal_spans(stream) {
        let nal = &stream[span.payload..span.end];
        let kind = nal_unit_type(nal);
        let is_slice = kind == nal_type::NON_IDR_SLICE || kind == nal_type::IDR_SLICE;
        let begins_picture = match kind {
            nal_type::ACCESS_UNIT_DELIMITER | nal_type::SEI | nal_type::SPS | nal_type::PPS => true,
            _ if is_slice => first_mb_in_slice(nal) == Some(0),
            _ => false,
        };
        if begins_picture && has_slice {
            if let Some(begin) = start {
                units.push(&stream[begin..span.code]);
            }
            start = None;
            has_slice = false;
        }
        start.get_or_insert(span.code);
        has_slice |= is_slice;
    }
    if let Some(begin) = start {
        units.push(&stream[begin..]);
    }
    units
}

/// `first_mb_in_slice` of a slice NAL unit.
fn first_mb_in_slice(nal: &[u8]) -> Option<u32> {
    let rbsp = remove_emulation_prevention(nal.get(1..)?);
    BitReader::new(&rbsp).unsigned_exp_golomb()
}

/// `nal_unit_type` of a NAL unit payload.
pub fn nal_unit_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |header| header & 0x1F)
}

/// Whether the access unit contains an IDR slice.
pub fn is_keyframe(stream: &[u8]) -> bool {
    nal_units(stream)
        .iter()
        .any(|nal| nal_unit_type(nal) == nal_type::IDR_SLICE)
}

/// The `slice_type` of a slice NAL unit (types 1 and 5), or `None` for
/// other NAL units or truncated headers.
pub fn slice_type(nal: &[u8]) -> Option<SliceType> {
    let kind = nal_unit_type(nal);
    if kind != nal_type::NON_IDR_SLICE && kind != nal_type::IDR_SLICE {
        return None;
    }
    let rbsp = remove_emulation_prevention(&nal[1..]);
    let mut reader = BitReader::new(&rbsp);
    let _first_mb_in_slice = reader.unsigned_exp_golomb()?;
    let raw = reader.unsigned_exp_golomb()?;
    Some(match raw % 5 {
        0 => SliceType::P,
        1 => SliceType::B,
        2 => SliceType::I,
        3 => SliceType::Sp,
        _ => SliceType::Si,
    })
}

/// Removes `emulation_prevention_three_byte`s (`00 00 03` -> `00 00`).
fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        output.push(byte);
    }
    output
}

struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.position / 8)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Some(u32::from(bit))
    }

    /// ue(v): `leading_zero_bits` zeros, a one, then that many bits.
    fn unsigned_exp_golomb(&mut self) -> Option<u32> {
        let mut leading_zeros = 0;
        while self.bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        let mut suffix = 0_u32;
        for _ in 0..leading_zeros {
            suffix = (suffix << 1) | self.bit()?;
        }
        Some((1_u32 << leading_zeros) - 1 + suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS
            0, 0, 1, 0x68, 0xBB, // PPS
            0, 0, 0, 1, 0x65, 0x88, 0x84, // IDR slice
        ];
        let nals = nal_units(&stream);
        assert_eq!(
            nals,
            vec![&[0x67, 0xAA][..], &[0x68, 0xBB], &[0x65, 0x88, 0x84]]
        );
        let types: Vec<u8> = nals.iter().map(|nal| nal_unit_type(nal)).collect();
        assert_eq!(types, [nal_type::SPS, nal_type::PPS, nal_type::IDR_SLICE]);
        assert!(is_keyframe(&stream));
        assert!(!is_keyframe(&[0, 0, 1, 0x41, 0x9A]));
        assert!(nal_units(&[]).is_empty());
    }

    #[test]
    fn splits_a_stream_into_access_units() {
        let sps = [0, 0, 0, 1, 0x67, 0xAA];
        let pps = [0, 0, 0, 1, 0x68, 0xBB];
        // first_mb_in_slice = 0 ("1...").
        let idr = [0, 0, 1, 0x65, 0x88, 0x84];
        // Two slices of one picture: first_mb 0, then first_mb 3 ("00100").
        let p_first = [0, 0, 0, 1, 0x41, 0x9A];
        let p_second = [0, 0, 0, 1, 0x41, 0b0010_0100];
        let p_next = [0, 0, 0, 1, 0x41, 0x9B];
        let stream: Vec<u8> = [&sps[..], &pps, &idr, &p_first, &p_second, &p_next].concat();
        let units = access_units(&stream);
        assert_eq!(units.len(), 3, "{units:?}");
        assert_eq!(units[0], [&sps[..], &pps, &idr].concat());
        assert_eq!(units[1], [&p_first[..], &p_second].concat());
        assert_eq!(units[2], p_next);
        assert!(is_keyframe(units[0]));
        // Units concatenated give back the stream.
        assert_eq!(units.concat(), stream);
        assert!(access_units(&[]).is_empty());
    }

    #[test]
    fn headers_start_a_new_access_unit() {
        let p = [0, 0, 0, 1, 0x41, 0x9A];
        let sps = [0, 0, 0, 1, 0x67, 0xAA];
        let idr = [0, 0, 0, 1, 0x65, 0x88];
        let stream: Vec<u8> = [&p[..], &sps, &idr].concat();
        let units = access_units(&stream);
        assert_eq!(units, [&p[..], &[&sps[..], &idr].concat()[..]]);
    }

    #[test]
    fn exp_golomb_values() {
        // Bits: 1 | 010 | 011 | 00100 | 00101 -> 0, 1, 2, 3, 4.
        let data = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut reader = BitReader::new(&data);
        let values: Vec<u32> = (0..5)
            .map(|_| reader.unsigned_exp_golomb().unwrap())
            .collect();
        assert_eq!(values, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn slice_types_from_headers() {
        // first_mb_in_slice = 0 ("1"), then slice_type ue(v).
        // P = 5 ("00110"), I = 7 ("0001000"), B = 6 ("00111").
        let p_slice = [0x41, 0b1001_1000];
        let i_slice = [0x65, 0b1000_1000, 0b0000_0000];
        let b_slice = [0x41, 0b1001_1100];
        assert_eq!(slice_type(&p_slice), Some(SliceType::P));
        assert_eq!(slice_type(&i_slice), Some(SliceType::I));
        assert_eq!(slice_type(&b_slice), Some(SliceType::B));
        assert_eq!(slice_type(&[0x67, 0xFF]), None);
        assert_eq!(slice_type(&[0x41]), None);
    }

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(
            remove_emulation_prevention(&[0, 0, 3, 1, 0, 0, 3, 0]),
            [0, 0, 1, 0, 0, 0]
        );
        assert_eq!(remove_emulation_prevention(&[0, 3, 0]), [0, 3, 0]);
    }
}
