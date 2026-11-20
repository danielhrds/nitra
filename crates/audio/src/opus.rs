//! Opus encoder and decoder (libopus, linked statically; see `build.rs`).
//! 48 kHz, stereo, 20 ms frames of interleaved `f32` samples.

use std::ffi::{c_int, c_void};
use std::fmt;
use std::ptr::NonNull;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one 20 ms frame.
pub const FRAME_SAMPLES: usize = 960;
/// Largest packet libopus produces for one frame (RFC 6716 limit).
pub const MAX_PACKET: usize = 1275;

const OPUS_OK: c_int = 0;
const OPUS_APPLICATION_AUDIO: c_int = 2049;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;

// libopus' C API (opus.h). Encoder and decoder states are opaque.
unsafe extern "C" {
    fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encode_float(
        st: *mut c_void,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;
    fn opus_encoder_ctl(st: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encoder_destroy(st: *mut c_void);
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_decode_float(
        st: *mut c_void,
        data: *const u8,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_decoder_destroy(st: *mut c_void);
    fn opus_strerror(error: c_int) -> *const std::ffi::c_char;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpusError(pub String);

impl OpusError {
    fn from_code(code: c_int) -> Self {
        // SAFETY: opus_strerror returns a static, NUL-terminated string for any code.
        let text = unsafe { std::ffi::CStr::from_ptr(opus_strerror(code)) };
        Self(format!("opus error {code}: {}", text.to_string_lossy()))
    }
}

impl fmt::Display for OpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OpusError {}

pub struct OpusEncoder(NonNull<c_void>);

// SAFETY: the encoder state is plain memory owned by this value; libopus
// keeps no thread affinity. `&mut self` on every call prevents shared use.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    /// A 48 kHz stereo encoder for general audio (game and app sound).
    pub fn new(bitrate_bps: u32) -> Result<Self, OpusError> {
        let mut error = 0;
        // SAFETY: valid arguments; `error` is a live local.
        let raw = unsafe {
            opus_encoder_create(
                SAMPLE_RATE as i32,
                CHANNELS as c_int,
                OPUS_APPLICATION_AUDIO,
                &mut error,
            )
        };
        let raw = NonNull::new(raw).ok_or_else(|| OpusError::from_code(error))?;
        let encoder = Self(raw);
        encoder.ctl(OPUS_SET_BITRATE_REQUEST, bitrate_bps as c_int)?;
        Ok(encoder)
    }

    /// Adds redundancy (in-band FEC) sized for this much expected loss.
    pub fn set_expected_loss(&mut self, percent: u8) -> Result<(), OpusError> {
        self.ctl(OPUS_SET_INBAND_FEC_REQUEST, c_int::from(percent > 0))?;
        self.ctl(
            OPUS_SET_PACKET_LOSS_PERC_REQUEST,
            c_int::from(percent.min(100)),
        )
    }

    /// Encodes one frame (`FRAME_SAMPLES * CHANNELS` interleaved samples)
    /// into `out`; returns the packet length.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, OpusError> {
        assert_eq!(pcm.len(), FRAME_SAMPLES * CHANNELS, "one 20 ms frame");
        // SAFETY: `pcm` holds exactly one frame; `out` is writable for its length.
        let n = unsafe {
            opus_encode_float(
                self.0.as_ptr(),
                pcm.as_ptr(),
                FRAME_SAMPLES as c_int,
                out.as_mut_ptr(),
                out.len().min(i32::MAX as usize) as i32,
            )
        };
        usize::try_from(n).map_err(|_| OpusError::from_code(n))
    }

    fn ctl(&self, request: c_int, value: c_int) -> Result<(), OpusError> {
        // SAFETY: valid encoder; these requests take one `opus_int32` argument.
        let code = unsafe { opus_encoder_ctl(self.0.as_ptr(), request, value) };
        if code == OPUS_OK {
            Ok(())
        } else {
            Err(OpusError::from_code(code))
        }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create, destroyed exactly once.
        unsafe { opus_encoder_destroy(self.0.as_ptr()) };
    }
}

pub struct OpusDecoder(NonNull<c_void>);

// SAFETY: as for the encoder.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self, OpusError> {
        let mut error = 0;
        // SAFETY: valid arguments; `error` is a live local.
        let raw = unsafe { opus_decoder_create(SAMPLE_RATE as i32, CHANNELS as c_int, &mut error) };
        NonNull::new(raw)
            .map(Self)
            .ok_or_else(|| OpusError::from_code(error))
    }

    /// Decodes one packet into `out` (room for one frame, interleaved);
    /// `None` conceals a lost packet. Returns samples per channel written.
    pub fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<usize, OpusError> {
        self.decode_inner(packet, out, false)
    }

    /// Rebuilds the frame lost just before `next_packet` from the FEC data
    /// carried in it.
    pub fn decode_lost_from(
        &mut self,
        next_packet: &[u8],
        out: &mut [f32],
    ) -> Result<usize, OpusError> {
        self.decode_inner(Some(next_packet), out, true)
    }

    fn decode_inner(
        &mut self,
        packet: Option<&[u8]>,
        out: &mut [f32],
        fec: bool,
    ) -> Result<usize, OpusError> {
        assert!(out.len() >= FRAME_SAMPLES * CHANNELS, "room for one frame");
        let (data, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len() as i32));
        // SAFETY: `data` is null (concealment) or a live slice of `len` bytes;
        // `out` holds at least one frame for every channel.
        let n = unsafe {
            opus_decode_float(
                self.0.as_ptr(),
                data,
                len,
                out.as_mut_ptr(),
                FRAME_SAMPLES as c_int,
                c_int::from(fec),
            )
        };
        usize::try_from(n).map_err(|_| OpusError::from_code(n))
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create, destroyed exactly once.
        unsafe { opus_decoder_destroy(self.0.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One 20 ms frame of a 440 Hz tone starting at sample `start`.
    fn tone(start: usize) -> Vec<f32> {
        (0..FRAME_SAMPLES)
            .flat_map(|i| {
                let t = (start + i) as f32 / SAMPLE_RATE as f32;
                let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                [s, s]
            })
            .collect()
    }

    fn energy(samples: &[f32]) -> f32 {
        samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32
    }

    #[test]
    fn a_tone_survives_the_round_trip() {
        let mut encoder = OpusEncoder::new(128_000).unwrap();
        let mut decoder = OpusDecoder::new().unwrap();
        let mut packet = [0_u8; MAX_PACKET];
        let mut out = vec![0_f32; FRAME_SAMPLES * CHANNELS];
        let mut decoded = Vec::new();
        for frame in 0..50 {
            let input = tone(frame * FRAME_SAMPLES);
            let n = encoder.encode(&input, &mut packet).unwrap();
            assert!(n > 0 && n <= MAX_PACKET);
            assert_eq!(
                decoder.decode(Some(&packet[..n]), &mut out).unwrap(),
                FRAME_SAMPLES
            );
            decoded.extend_from_slice(&out);
        }
        // After the codec's start-up, the energy matches the input's (0.125).
        let e = energy(&decoded[decoded.len() / 2..]);
        assert!((0.10..0.15).contains(&e), "energy {e}");
    }

    #[test]
    fn a_lost_packet_is_concealed_or_rebuilt() {
        let mut encoder = OpusEncoder::new(128_000).unwrap();
        encoder.set_expected_loss(10).unwrap();
        let mut decoder = OpusDecoder::new().unwrap();
        let mut packets = Vec::new();
        for frame in 0..20 {
            let mut packet = [0_u8; MAX_PACKET];
            let n = encoder
                .encode(&tone(frame * FRAME_SAMPLES), &mut packet)
                .unwrap();
            packets.push(packet[..n].to_vec());
        }
        let mut out = vec![0_f32; FRAME_SAMPLES * CHANNELS];
        for packet in &packets[..10] {
            decoder.decode(Some(packet), &mut out).unwrap();
        }
        // Packet 10 lost: concealment still yields a whole frame of sound.
        assert_eq!(decoder.decode(None, &mut out).unwrap(), FRAME_SAMPLES);
        assert!(energy(&out) > 0.01);
        // Packet 12 lost, 13 arrived: FEC rebuilds 12 from 13.
        decoder.decode(Some(&packets[11]), &mut out).unwrap();
        assert_eq!(
            decoder.decode_lost_from(&packets[13], &mut out).unwrap(),
            FRAME_SAMPLES
        );
        assert!(energy(&out) > 0.01);
    }

    #[test]
    fn a_wrong_bitrate_is_an_error_not_a_crash() {
        let mut encoder = OpusEncoder::new(128_000).unwrap();
        assert!(encoder.ctl(OPUS_SET_BITRATE_REQUEST, -5).is_err());
        assert!(encoder.set_expected_loss(5).is_ok());
    }
}
