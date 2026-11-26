//! AMD AMF hardware H.264 encoder (`AMFVideoEncoderVCE_AVC`) fed with D3D11
//! NV12 textures wrapped as AMF surfaces, so frames never leave the GPU.
//!
//! The context is bound to the caller's D3D11 device (the one the textures
//! come from). `SubmitInput` and `QueryOutput` never wait: a full input
//! queue becomes `Submit::Busy`, and `QueryTimeout` stays 0.

use std::time::{Duration, Instant};

use amf::{Component, Context, Output, Submitted, Value, avc};
use gpu::{D3d11Device, FrameFormat, GpuFrame};
use telemetry::{QpcTimestamp, qpc_now};
use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;
use windows::core::PCWSTR;

use crate::contract::{EncodeError, EncodedChunk, EncoderConfig, InFlight, Submit, VideoEncoder};

const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

impl From<amf::AmfError> for EncodeError {
    fn from(error: amf::AmfError) -> Self {
        EncodeError::Windows(error.to_string())
    }
}

/// Which encoder settings were accepted and read back with the value set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppliedSettings {
    pub ultra_low_latency_usage: bool,
    pub low_latency_mode: bool,
    pub high_profile: bool,
    pub constant_bitrate: bool,
    pub no_b_frames: bool,
    /// Accepted and read back, but not honored with this usage: periodic
    /// IDR frames are forced per frame instead (see `submit`).
    pub idr_period: bool,
    pub header_spacing: bool,
    pub target_bitrate: bool,
    pub peak_bitrate: bool,
    pub no_skipped_frames: bool,
    pub max_reference_frames: bool,
    /// The quality preset the usage selected (not set by us).
    pub quality_preset: Option<i64>,
}

struct Pending {
    frame: GpuFrame,
    submitted_at: QpcTimestamp,
}

pub struct AmfEncoder {
    config: EncoderConfig,
    runtime_version: u64,
    applied: AppliedSettings,
    // Field order is drop order: the component is terminated before its
    // context, and both before the textures still in flight return to their
    // pools.
    component: Component,
    context: Context,
    in_flight: InFlight<Pending>,
    next_frame: i64,
    keyframe_requested: bool,
}

impl AmfEncoder {
    /// Creates the AMF H.264 encoder on the adapter of `device`.
    pub fn new(device: &D3d11Device, config: EncoderConfig) -> Result<Self, EncodeError> {
        config.validate()?;
        device.enable_multithread_protection()?;
        let runtime = amf::runtime().map_err(|error| EncodeError::NotFound(error.to_string()))?;
        let context = runtime.create_context()?;
        context.init_dx11(device.raw())?;
        let component = runtime
            .create_component(&context, avc::ENCODER)
            .map_err(|error| EncodeError::NotFound(error.to_string()))?;

        let int = |value: u32| Value::Int64(i64::from(value));
        let bitrate = int(config.bitrate_bps);
        let mut applied = AppliedSettings {
            // Usage first: it resets the other settings to its defaults.
            ultra_low_latency_usage: apply(
                &component,
                avc::USAGE,
                Value::Int64(avc::USAGE_ULTRA_LOW_LATENCY),
            ),
            ..AppliedSettings::default()
        };
        applied.high_profile = apply(&component, avc::PROFILE, Value::Int64(avc::PROFILE_HIGH));
        component.set_property(
            avc::FRAME_SIZE,
            Value::Size {
                width: dimension(config.width)?,
                height: dimension(config.height)?,
            },
        )?;
        component.set_property(
            avc::FRAME_RATE,
            Value::Rate {
                num: config.fps,
                den: 1,
            },
        )?;
        applied.low_latency_mode = apply(&component, avc::LOW_LATENCY_MODE, Value::Bool(true));
        applied.constant_bitrate = apply(
            &component,
            avc::RATE_CONTROL_METHOD,
            Value::Int64(avc::RATE_CONTROL_CBR),
        );
        applied.no_b_frames = apply(&component, avc::B_PIC_PATTERN, Value::Int64(0));
        applied.target_bitrate = apply(&component, avc::TARGET_BITRATE, bitrate);
        applied.peak_bitrate = apply(&component, avc::PEAK_BITRATE, bitrate);
        applied.no_skipped_frames = apply(
            &component,
            avc::RATE_CONTROL_SKIP_FRAME_ENABLE,
            Value::Bool(false),
        );
        if config.keyframe_interval > 0 {
            let interval = int(config.keyframe_interval);
            applied.idr_period = apply(&component, avc::IDR_PERIOD, interval);
            // SPS/PPS on every periodic IDR frame too.
            applied.header_spacing = apply(&component, avc::HEADER_INSERTION_SPACING, interval);
        }
        if config.max_reference_frames > 0 {
            applied.max_reference_frames = apply(
                &component,
                avc::MAX_NUM_REFRAMES,
                int(config.max_reference_frames),
            );
        }
        component.set_property(avc::QUERY_TIMEOUT, Value::Int64(0))?;

        component.init(
            amf::SURFACE_NV12,
            dimension(config.width)?,
            dimension(config.height)?,
        )?;
        applied.quality_preset = match component.get_property(avc::QUALITY_PRESET) {
            Ok(Value::Int64(preset)) => Some(preset),
            _ => None,
        };

        Ok(Self {
            runtime_version: runtime.version(),
            applied,
            component,
            context,
            in_flight: InFlight::new(config.max_in_flight),
            config,
            next_frame: 0,
            keyframe_requested: false,
        })
    }

    /// The AMF runtime version, e.g. `1.5.2.0`.
    pub fn runtime_version(&self) -> String {
        amf::format_version(self.runtime_version)
    }

    pub fn applied_settings(&self) -> AppliedSettings {
        self.applied
    }

    fn pts(&self, frame_index: i64) -> i64 {
        frame_index * amf::PTS_PER_SECOND / i64::from(self.config.fps)
    }

    fn take_chunk(&mut self, buffer: &amf::Buffer) -> Result<EncodedChunk, EncodeError> {
        let output_at = qpc_now();
        let data = buffer.bytes().to_vec();
        let idr = matches!(
            buffer.get_property(avc::OUTPUT_DATA_TYPE),
            Ok(Value::Int64(avc::OUTPUT_DATA_TYPE_IDR))
        );
        let pending = self
            .in_flight
            .take(buffer.pts())
            .or_else(|| self.in_flight.take_oldest())
            .ok_or_else(|| {
                EncodeError::Windows("encoder output without a matching input".to_owned())
            })?;
        Ok(EncodedChunk {
            keyframe: idr || crate::annexb::is_keyframe(&data),
            data,
            frame_id: pending.frame.frame_id,
            captured_at: pending.frame.captured_at,
            submitted_at: pending.submitted_at,
            output_at,
        })
        // `pending` drops here: the input texture returns to its pool.
    }
}

impl VideoEncoder for AmfEncoder {
    fn name(&self) -> &'static str {
        "amf"
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
        if self.in_flight.is_full() {
            return Ok(Submit::Busy(frame));
        }

        let surface = self.context.create_surface_from_dx11(&frame.texture)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; live out-pointer.
        unsafe { frame.texture.GetDesc(&mut desc) };
        if (desc.Width, desc.Height) != (frame.width, frame.height) {
            surface.set_crop(dimension(frame.width)?, dimension(frame.height)?)?;
        }
        let pts = self.pts(self.next_frame);
        surface.set_pts(pts);
        surface.set_duration(self.pts(1));
        // The periodic IDR is forced here: with the ultra-low-latency usage
        // the encoder accepts and reads back `IDRPeriod` but never inserts
        // the periodic IDR frames (AMF runtime 1.5.2, RX 7600).
        let interval = i64::from(self.config.keyframe_interval);
        let keyframe = self.keyframe_requested
            || self.next_frame == 0
            || (interval > 0 && self.next_frame % interval == 0);
        if keyframe {
            surface.set_property(avc::FORCE_PICTURE_TYPE, Value::Int64(avc::PICTURE_TYPE_IDR))?;
            surface.set_property(avc::INSERT_SPS, Value::Bool(true))?;
            surface.set_property(avc::INSERT_PPS, Value::Bool(true))?;
        }

        let submitted_at = qpc_now();
        match self.component.submit_input(&surface)? {
            // The surface (and its texture reference) is simply dropped.
            Submitted::InputFull => return Ok(Submit::Busy(frame)),
            Submitted::Accepted => {}
        }
        if keyframe {
            self.keyframe_requested = false;
        }
        self.next_frame += 1;
        // The encoder holds its own reference to the surface; the lease kept
        // in `in_flight` until the output comes back stops the pool from
        // rewriting the texture meanwhile.
        if self
            .in_flight
            .push(
                pts,
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
        match self.component.query_output()? {
            Output::Buffer(buffer) => self.take_chunk(&buffer).map(Some),
            Output::Pending | Output::EndOfStream => Ok(None),
        }
    }

    fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        let bitrate = Value::Int64(i64::from(bitrate_bps));
        self.component.set_property(avc::TARGET_BITRATE, bitrate)?;
        self.component.set_property(avc::PEAK_BITRATE, bitrate)?;
        self.config.bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn drain(&mut self) -> Result<Vec<EncodedChunk>, EncodeError> {
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut chunks = Vec::new();
        while self.component.drain()? == Submitted::InputFull {
            if Instant::now() >= deadline {
                return Err(EncodeError::Windows("drain refused for 2 s".to_owned()));
            }
            if let Some(chunk) = self.poll()? {
                chunks.push(chunk);
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        while Instant::now() < deadline {
            match self.component.query_output()? {
                Output::Buffer(buffer) => chunks.push(self.take_chunk(&buffer)?),
                Output::Pending => std::thread::sleep(Duration::from_millis(1)),
                Output::EndOfStream => break,
            }
        }
        // Accept input again (a drained encoder refuses it until flushed).
        self.component.flush()?;
        Ok(chunks)
    }
}

/// Sets a property and reads it back; true when both agree.
fn apply(component: &Component, name: PCWSTR, value: Value) -> bool {
    component.set_property(name, value).is_ok()
        && component.get_property(name).is_ok_and(|read| read == value)
}

fn dimension(value: u32) -> Result<i32, EncodeError> {
    i32::try_from(value).map_err(|_| EncodeError::InvalidConfig(format!("size {value} too large")))
}
