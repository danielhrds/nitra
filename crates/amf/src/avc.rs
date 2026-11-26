//! The H.264 (AVC) encoder component: its id, property names and enum
//! values, from `components/VideoEncoderVCE.h` (MIT, see `sys`).
//!
//! Static properties are set before `Init`; dynamic ones at any time.
//! Per-frame properties go on the input surface; output properties are set
//! by the encoder on each output buffer.

use windows::core::{PCWSTR, w};

/// `AMFVideoEncoderVCE_AVC`.
pub const ENCODER: PCWSTR = w!("AMFVideoEncoderVCE_AVC");

// Static.
/// `AMF_VIDEO_ENCODER_USAGE_ENUM`; configures every other default, so it
/// is set first.
pub const USAGE: PCWSTR = w!("Usage");
pub const PROFILE: PCWSTR = w!("Profile");
pub const QUALITY_PRESET: PCWSTR = w!("QualityPreset");
pub const FRAME_SIZE: PCWSTR = w!("FrameSize");
pub const LOW_LATENCY_MODE: PCWSTR = w!("LowLatencyInternal");
pub const RATE_CONTROL_METHOD: PCWSTR = w!("RateControlMethod");

// Dynamic.
pub const FRAME_RATE: PCWSTR = w!("FrameRate");
pub const B_PIC_PATTERN: PCWSTR = w!("BPicturesPattern");
pub const IDR_PERIOD: PCWSTR = w!("IDRPeriod");
pub const TARGET_BITRATE: PCWSTR = w!("TargetBitrate");
pub const PEAK_BITRATE: PCWSTR = w!("PeakBitrate");
pub const RATE_CONTROL_SKIP_FRAME_ENABLE: PCWSTR = w!("RateControlSkipFrameEnable");
pub const FILLER_DATA_ENABLE: PCWSTR = w!("FillerDataEnable");
pub const HEADER_INSERTION_SPACING: PCWSTR = w!("HeaderInsertionSpacing");
/// The most reference frames the stream may use (`AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES`).
pub const MAX_NUM_REFRAMES: PCWSTR = w!("MaxNumRefFrames");

// Misc.
/// Milliseconds `QueryOutput` may wait; 0 (the default) never waits.
pub const QUERY_TIMEOUT: PCWSTR = w!("QueryTimeout");

// Per frame (input surface).
pub const FORCE_PICTURE_TYPE: PCWSTR = w!("ForcePictureType");
pub const INSERT_SPS: PCWSTR = w!("InsertSPS");
pub const INSERT_PPS: PCWSTR = w!("InsertPPS");

// Output buffer.
pub const OUTPUT_DATA_TYPE: PCWSTR = w!("OutputDataType");

// AMF_VIDEO_ENCODER_USAGE_ENUM.
pub const USAGE_TRANSCODING: i64 = 0;
pub const USAGE_ULTRA_LOW_LATENCY: i64 = 2;
pub const USAGE_LOW_LATENCY: i64 = 3;

// AMF_VIDEO_ENCODER_PROFILE_ENUM.
pub const PROFILE_MAIN: i64 = 77;
pub const PROFILE_HIGH: i64 = 100;

// AMF_VIDEO_ENCODER_QUALITY_PRESET_ENUM.
pub const QUALITY_PRESET_BALANCED: i64 = 0;
pub const QUALITY_PRESET_SPEED: i64 = 1;
pub const QUALITY_PRESET_QUALITY: i64 = 2;

// AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_ENUM.
pub const RATE_CONTROL_CBR: i64 = 1;

// AMF_VIDEO_ENCODER_PICTURE_TYPE_ENUM.
pub const PICTURE_TYPE_NONE: i64 = 0;
pub const PICTURE_TYPE_IDR: i64 = 2;

// AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_ENUM.
pub const OUTPUT_DATA_TYPE_IDR: i64 = 0;
pub const OUTPUT_DATA_TYPE_I: i64 = 1;
pub const OUTPUT_DATA_TYPE_P: i64 = 2;
pub const OUTPUT_DATA_TYPE_B: i64 = 3;
