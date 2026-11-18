//! Media Foundation hardware H.264 encoder (asynchronous MFT) fed with D3D11
//! NV12 textures through an `IMFDXGIDeviceManager`, so frames never leave
//! the GPU.
//!
//! Hardware MFTs are asynchronous: they announce with events when they can
//! take input (`METransformNeedInput`) and when output is ready
//! (`METransformHaveOutput`). `submit` and `poll` read those events without
//! waiting and only call `ProcessInput`/`ProcessOutput` when allowed.
//!
//! The caller must have initialized COM (MTA) and started Media Foundation
//! (`mf::MediaFoundation`) on the calling thread.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use gpu::{AdapterLuid, D3d11Device, FrameFormat, GpuFrame};
use mf::{MftInfo, hardware_activates_for_adapter, video_type};
use telemetry::{QpcTimestamp, qpc_now};
use windows::Win32::Foundation::LUID;
use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFAttributes, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSampleExtension_CleanPoint, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_High,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface};

use crate::contract::{EncodeError, EncodedChunk, EncoderConfig, InFlight, Submit, VideoEncoder};

/// Media Foundation sample times are in 100 ns units.
const TICKS_PER_SECOND: i64 = 10_000_000;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Which optional encoder settings the MFT accepted (`ICodecAPI::SetValue`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppliedSettings {
    pub low_latency: bool,
    pub constant_bitrate: bool,
    pub no_b_frames: bool,
    pub gop_size: bool,
    pub mean_bitrate: bool,
}

struct Pending {
    frame: GpuFrame,
    submitted_at: QpcTimestamp,
}

pub struct MediaFoundationEncoder {
    config: EncoderConfig,
    info: MftInfo,
    candidates: usize,
    activate: IMFActivate,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    _device_manager: IMFDXGIDeviceManager,
    applied: AppliedSettings,
    provides_samples: bool,
    output_buffer_size: u32,
    in_flight: InFlight<Pending>,
    need_input: u32,
    have_output: u32,
    drain_complete: bool,
    next_sample: i64,
    keyframe_requested: bool,
}

impl MediaFoundationEncoder {
    /// Creates the hardware H.264 encoder of the adapter `adapter_luid`
    /// (the one `device` was created on).
    pub fn new(
        device: &D3d11Device,
        adapter_luid: AdapterLuid,
        config: EncoderConfig,
    ) -> Result<Self, EncodeError> {
        config.validate()?;
        device.enable_multithread_protection()?;
        let luid = LUID {
            LowPart: adapter_luid.low,
            HighPart: adapter_luid.high,
        };
        let candidates = hardware_activates_for_adapter(
            MFT_CATEGORY_VIDEO_ENCODER,
            video_type(MFVideoFormat_H264),
            luid,
        )?;
        let candidate_count = candidates.len();
        let (activate, info) = candidates.into_iter().next().ok_or_else(|| {
            EncodeError::NotFound(format!(
                "no hardware H.264 encoder for adapter {adapter_luid}"
            ))
        })?;

        // SAFETY: valid activation object; creates the transform.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }?;
        // SAFETY: valid transform; attribute keys are static GUIDs.
        let attributes: IMFAttributes = unsafe { transform.GetAttributes() }?;
        if unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 0 {
            return Err(EncodeError::Unsupported(format!(
                "{:?} is not an asynchronous MFT",
                info.name
            )));
        }
        // SAFETY: as above. Async MFTs refuse calls until unlocked.
        unsafe {
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        let mut reset_token = 0_u32;
        let mut device_manager = None;
        // SAFETY: out-pointers reference live locals.
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager) }?;
        let device_manager: IMFDXGIDeviceManager =
            device_manager.expect("MFCreateDXGIDeviceManager succeeded");
        // SAFETY: valid manager and device (created with video support and
        // multithread protection); the manager keeps the device alive.
        unsafe {
            device_manager.ResetDevice(device.raw(), reset_token)?;
            transform.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                device_manager.as_raw() as usize,
            )?;
        }

        let codec = transform.cast::<ICodecAPI>().ok();
        let mut applied = AppliedSettings::default();
        if let Some(codec) = &codec {
            applied.low_latency =
                set_codec_value(codec, &CODECAPI_AVLowLatencyMode, variant_bool(true));
            applied.constant_bitrate = set_codec_value(
                codec,
                &CODECAPI_AVEncCommonRateControlMode,
                variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            applied.no_b_frames = set_codec_value(
                codec,
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                variant_u32(0_u32),
            );
            if config.keyframe_interval > 0 {
                applied.gop_size = set_codec_value(
                    codec,
                    &CODECAPI_AVEncMPVGOPSize,
                    variant_u32(config.keyframe_interval),
                );
            }
            applied.mean_bitrate = set_codec_value(
                codec,
                &CODECAPI_AVEncCommonMeanBitRate,
                variant_u32(config.bitrate_bps),
            );
        }

        // Output type first, then input type, as encoders require.
        let output_type = media_type(MFVideoFormat_H264, &config)?;
        // SAFETY: valid media type; attribute keys are static GUIDs.
        unsafe {
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)?;
            output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            transform.SetOutputType(0, &output_type, 0)?;
        }
        let input_type = media_type(MFVideoFormat_NV12, &config)?;
        // SAFETY: valid transform and media type.
        unsafe { transform.SetInputType(0, &input_type, 0) }?;

        let (provides_samples, output_buffer_size) = output_stream_info(&transform)?;
        let events: IMFMediaEventGenerator = transform.cast()?;
        // SAFETY: valid transform; starts accepting input.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        Ok(Self {
            info,
            candidates: candidate_count,
            activate,
            transform,
            events,
            codec,
            _device_manager: device_manager,
            applied,
            provides_samples,
            output_buffer_size,
            in_flight: InFlight::new(config.max_in_flight),
            config,
            need_input: 0,
            have_output: 0,
            drain_complete: false,
            next_sample: 0,
            keyframe_requested: false,
        })
    }

    /// The MFT in use (friendly name, CLSID, vendor id).
    pub fn info(&self) -> &MftInfo {
        &self.info
    }

    /// How many hardware H.264 encoders `MFTEnum2` listed for the adapter.
    pub fn candidates(&self) -> usize {
        self.candidates
    }

    pub fn applied_settings(&self) -> AppliedSettings {
        self.applied
    }

    /// Reads every queued MFT event without waiting.
    fn pump_events(&mut self) -> Result<(), EncodeError> {
        loop {
            // SAFETY: valid event generator; NO_WAIT makes the call non-blocking.
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    // SAFETY: valid event.
                    let kind = unsafe { event.GetType() }?;
                    if kind == METransformNeedInput.0 as u32 {
                        self.need_input += 1;
                    } else if kind == METransformHaveOutput.0 as u32 {
                        self.have_output += 1;
                    } else if kind == METransformDrainComplete.0 as u32 {
                        self.drain_complete = true;
                    }
                }
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn take_output(&mut self) -> Result<Option<EncodedChunk>, EncodeError> {
        let provided = if self.provides_samples {
            None
        } else {
            Some(empty_sample(self.output_buffer_size)?)
        };
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(provided),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0_u32;
        // SAFETY: one output buffer for stream 0; the MFT fills `pSample`
        // (or uses ours) and may set `pEvents`; both are taken back below.
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        // SAFETY: each field is taken or dropped exactly once.
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        unsafe { ManuallyDrop::drop(&mut output.pEvents) };

        match result {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder changed its output type (e.g. after the first
                // frame): accept its preferred type and try again later.
                // SAFETY: valid transform; type index 0 is its preference.
                unsafe {
                    let preferred = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &preferred, 0)?;
                }
                (self.provides_samples, self.output_buffer_size) =
                    output_stream_info(&self.transform)?;
                return Ok(None);
            }
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                self.have_output = self.have_output.saturating_sub(1);
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        self.have_output = self.have_output.saturating_sub(1);
        let sample = sample
            .ok_or_else(|| EncodeError::Windows("ProcessOutput returned no sample".to_owned()))?;
        let output_at = qpc_now();

        // SAFETY: valid sample; attribute keys are static GUIDs.
        let (sample_time, keyframe) = unsafe {
            (
                sample.GetSampleTime()?,
                sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0,
            )
        };
        let data = sample_bytes(&sample)?;
        let pending = self
            .in_flight
            .take(sample_time)
            .or_else(|| self.in_flight.take_oldest())
            .ok_or_else(|| {
                EncodeError::Windows("encoder output without a matching input".to_owned())
            })?;
        Ok(Some(EncodedChunk {
            keyframe: keyframe || crate::annexb::is_keyframe(&data),
            data,
            frame_id: pending.frame.frame_id,
            captured_at: pending.frame.captured_at,
            submitted_at: pending.submitted_at,
            output_at,
        }))
        // `pending` drops here: the input texture returns to its pool.
    }
}

impl VideoEncoder for MediaFoundationEncoder {
    fn name(&self) -> &'static str {
        "media-foundation"
    }

    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn submit(&mut self, frame: GpuFrame) -> Result<Submit, EncodeError> {
        if frame.format != FrameFormat::Nv12
            || (frame.width, frame.height) != (self.config.width, self.config.height)
        {
            return Err(EncodeError::InvalidConfig(format!(
                "expected {}x{} NV12, got {}x{} {:?}",
                self.config.width, self.config.height, frame.width, frame.height, frame.format
            )));
        }
        self.pump_events()?;
        if self.need_input == 0 || self.in_flight.is_full() {
            return Ok(Submit::Busy(frame));
        }
        if self.keyframe_requested {
            if let Some(codec) = &self.codec {
                set_codec_value(codec, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(1_u32));
            }
            self.keyframe_requested = false;
        }

        let fps = i64::from(self.config.fps);
        let sample_time = self.next_sample * TICKS_PER_SECOND / fps;
        // SAFETY: the texture is a valid NV12 texture of this device; the
        // sample keeps a reference to it, and the lease (kept in `in_flight`
        // until the output comes back) keeps the pool from reusing it.
        let sample = unsafe {
            let buffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &*frame.texture, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(sample_time)?;
            sample.SetSampleDuration(TICKS_PER_SECOND / fps)?;
            sample
        };
        let submitted_at = qpc_now();
        // SAFETY: valid transform and sample; allowed by a NeedInput event.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }?;
        self.need_input -= 1;
        self.next_sample += 1;
        if self
            .in_flight
            .push(
                sample_time,
                Pending {
                    frame,
                    submitted_at,
                },
            )
            .is_err()
        {
            unreachable!("checked not full above");
        }
        Ok(Submit::Accepted)
    }

    fn poll(&mut self) -> Result<Option<EncodedChunk>, EncodeError> {
        self.pump_events()?;
        if self.have_output == 0 {
            return Ok(None);
        }
        self.take_output()
    }

    fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| EncodeError::Unsupported("encoder has no ICodecAPI".to_owned()))?;
        if !set_codec_value(
            codec,
            &CODECAPI_AVEncCommonMeanBitRate,
            variant_u32(bitrate_bps),
        ) {
            return Err(EncodeError::Unsupported(
                "bitrate change refused".to_owned(),
            ));
        }
        self.config.bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn drain(&mut self) -> Result<Vec<EncodedChunk>, EncodeError> {
        // SAFETY: valid transform; asks it to output every pending frame.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }?;
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut chunks = Vec::new();
        while !self.drain_complete && Instant::now() < deadline {
            self.pump_events()?;
            while self.have_output > 0 {
                if let Some(chunk) = self.take_output()? {
                    chunks.push(chunk);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(chunks)
    }
}

impl Drop for MediaFoundationEncoder {
    fn drop(&mut self) {
        // SAFETY: valid transform and activation object; ends streaming and
        // releases the MFT before the textures still in flight are returned.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// Sets one `ICodecAPI` value; returns whether the encoder accepted it.
fn set_codec_value(codec: &ICodecAPI, key: &GUID, value: VARIANT) -> bool {
    // SAFETY: valid codec interface; key and value are live for the call.
    unsafe { codec.SetValue(key, &value) }.is_ok()
}

/// A `VT_UI4` VARIANT (plain data: nothing to clear afterwards).
fn variant_u32(value: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}

/// A `VT_BOOL` VARIANT (plain data: nothing to clear afterwards).
fn variant_bool(value: bool) -> VARIANT {
    let flag = if value { VARIANT_TRUE } else { VARIANT_FALSE };
    variant(VT_BOOL, VARIANT_0_0_0 { boolVal: flag })
}

fn variant(kind: VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: kind,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

/// A video media type of `subtype` with the configured size and rate.
fn media_type(subtype: GUID, config: &EncoderConfig) -> Result<IMFMediaType, EncodeError> {
    // SAFETY: creates an empty media type; keys are static GUIDs.
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(config.width, config.height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(config.fps, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(media_type)
    }
}

/// Media Foundation packs two u32 values (size, ratio) into one u64.
fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// Whether the MFT allocates output samples, and the buffer size to use if not.
fn output_stream_info(transform: &IMFTransform) -> Result<(bool, u32), EncodeError> {
    // SAFETY: valid transform; stream 0 exists once types are set.
    let info = unsafe { transform.GetOutputStreamInfo(0) }?;
    let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
    Ok((provides, info.cbSize.max(1024 * 1024)))
}

fn empty_sample(size: u32) -> Result<IMFSample, EncodeError> {
    // SAFETY: plain object creation.
    unsafe {
        let buffer = MFCreateMemoryBuffer(size)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

/// Copies the encoded bytes out of a sample.
fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, EncodeError> {
    // SAFETY: valid sample; the buffer is locked while its bytes are copied
    // and unlocked right after.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut length = 0_u32;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
        buffer.Unlock()?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_size_and_ratio() {
        assert_eq!(pack(1920, 1080), (1920_u64 << 32) | 1080);
        assert_eq!(pack(60, 1), (60_u64 << 32) | 1);
    }
}
