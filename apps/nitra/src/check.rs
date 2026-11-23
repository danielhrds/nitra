//! Checks on the received stream that need no decoder: the access units are
//! well-formed H.264 Annex B, keyframes can start a decoder, media time
//! moves forward.

use encode::annexb::{nal_unit_type, nal_units};
use serde::Serialize;

const NON_IDR_SLICE: u8 = 1;
const IDR_SLICE: u8 = 5;
const SPS: u8 = 7;
const PPS: u8 = 8;

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct StreamCheck {
    pub frames: u64,
    pub keyframes: u64,
    /// No NAL unit, or no picture slice.
    pub malformed: u64,
    /// Keyframes without SPS and PPS before the slice (a decoder joining
    /// there cannot start).
    pub keyframes_without_parameter_sets: u64,
    /// Media time not after the previous frame's.
    pub time_regressions: u64,
    #[serde(skip)]
    last_rtp_time: Option<u64>,
}

impl StreamCheck {
    pub fn frame(&mut self, data: &[u8], rtp_time: u64) {
        self.frames += 1;
        if self.last_rtp_time.is_some_and(|last| rtp_time <= last) {
            self.time_regressions += 1;
        }
        self.last_rtp_time = Some(rtp_time);
        let kinds: Vec<u8> = nal_units(data).iter().map(|n| nal_unit_type(n)).collect();
        if !kinds.iter().any(|&k| k == NON_IDR_SLICE || k == IDR_SLICE) {
            self.malformed += 1;
            return;
        }
        if let Some(idr) = kinds.iter().position(|&k| k == IDR_SLICE) {
            self.keyframes += 1;
            let before = &kinds[..idr];
            if !(before.contains(&SPS) && before.contains(&PPS)) {
                self.keyframes_without_parameter_sets += 1;
            }
        }
    }

    /// Anything wrong at all.
    pub fn problems(&self) -> u64 {
        self.malformed + self.keyframes_without_parameter_sets + self.time_regressions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(kinds: &[u8]) -> Vec<u8> {
        kinds
            .iter()
            .flat_map(|&k| [0, 0, 0, 1, 0x60 | k, 0x88, 0x84])
            .collect()
    }

    #[test]
    fn a_well_formed_stream_has_no_problems() {
        let mut check = StreamCheck::default();
        check.frame(&unit(&[SPS, PPS, IDR_SLICE]), 0);
        check.frame(&unit(&[NON_IDR_SLICE]), 1500);
        check.frame(&unit(&[NON_IDR_SLICE]), 3000);
        assert_eq!(check.frames, 3);
        assert_eq!(check.keyframes, 1);
        assert_eq!(check.problems(), 0);
    }

    #[test]
    fn problems_are_counted() {
        let mut check = StreamCheck::default();
        check.frame(&unit(&[IDR_SLICE]), 3000); // no parameter sets
        check.frame(&unit(&[NON_IDR_SLICE]), 1500); // time went back
        check.frame(&[1, 2, 3], 4500); // no NAL unit
        check.frame(&unit(&[SPS, PPS]), 6000); // no slice
        assert_eq!(check.keyframes_without_parameter_sets, 1);
        assert_eq!(check.time_regressions, 1);
        assert_eq!(check.malformed, 2);
    }
}
