//! Decoder capability checks: Media Foundation decoder MFTs and the D3D11
//! (DXVA) decoder profiles a device exposes.

use gpu::D3d11Device;
use mf::{MftInfo, enumerate_mfts, video_type};
use serde::Serialize;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, D3D11_DECODER_PROFILE_H264_VLD_FGT,
    D3D11_DECODER_PROFILE_H264_VLD_NOFGT, D3D11_DECODER_PROFILE_H264_VLD_WITHFMOASO_NOFGT,
    D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
    D3D11_DECODER_PROFILE_MPEG2_VLD, D3D11_DECODER_PROFILE_VC1_D2010,
    D3D11_DECODER_PROFILE_VP9_VLD_10BIT_PROFILE2, D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0,
    ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::{
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFVideoFormat_H264,
};
use windows::core::{GUID, Interface, Result};

/// All H.264 decoder MFTs (software and hardware), best match first.
///
/// The Microsoft H.264 decoder is a software MFT that uses the GPU through
/// DXVA when given a D3D11 device, so `hardware: false` is expected here;
/// hardware decode support is what [`d3d11_decode_support`] reports.
/// The caller must have initialized COM and started Media Foundation.
pub fn h264_decoder_mfts() -> Result<Vec<MftInfo>> {
    enumerate_mfts(
        MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_SYNCMFT.0
                | MFT_ENUM_FLAG_ASYNCMFT.0
                | MFT_ENUM_FLAG_HARDWARE.0
                | MFT_ENUM_FLAG_SORTANDFILTER.0,
        ),
        Some(video_type(MFVideoFormat_H264)),
        None,
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DecoderProfile {
    /// Short name for well-known profiles; `None` for others.
    pub name: Option<&'static str>,
    pub guid: String,
}

/// Hardware decode capabilities of one D3D11 device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct D3d11DecodeSupport {
    pub profiles: Vec<DecoderProfile>,
    /// H.264 (VLD, no film grain) can decode to NV12 surfaces.
    pub h264_nv12: bool,
}

const KNOWN_PROFILES: [(GUID, &str); 10] = [
    (D3D11_DECODER_PROFILE_H264_VLD_NOFGT, "h264_vld_nofgt"),
    (D3D11_DECODER_PROFILE_H264_VLD_FGT, "h264_vld_fgt"),
    (
        D3D11_DECODER_PROFILE_H264_VLD_WITHFMOASO_NOFGT,
        "h264_vld_withfmoaso_nofgt",
    ),
    (D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, "hevc_vld_main"),
    (D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, "hevc_vld_main10"),
    (D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0, "vp9_vld_profile0"),
    (
        D3D11_DECODER_PROFILE_VP9_VLD_10BIT_PROFILE2,
        "vp9_vld_10bit_profile2",
    ),
    (D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, "av1_vld_profile0"),
    (D3D11_DECODER_PROFILE_MPEG2_VLD, "mpeg2_vld"),
    (D3D11_DECODER_PROFILE_VC1_D2010, "vc1_d2010"),
];

fn profile_name(guid: &GUID) -> Option<&'static str> {
    KNOWN_PROFILES
        .iter()
        .find(|(known, _)| known == guid)
        .map(|&(_, name)| name)
}

/// Lists the decoder profiles of `device` and checks H.264 → NV12.
/// Only queries capabilities; no decoder is created.
pub fn d3d11_decode_support(device: &D3d11Device) -> Result<D3d11DecodeSupport> {
    let video_device: ID3D11VideoDevice = device.raw().cast()?;

    // SAFETY: valid video device; indices below the reported count are valid.
    let count = unsafe { video_device.GetVideoDecoderProfileCount() };
    let mut profiles = Vec::with_capacity(count as usize);
    let mut has_h264 = false;
    for index in 0..count {
        // SAFETY: as above.
        let guid = unsafe { video_device.GetVideoDecoderProfile(index) }?;
        has_h264 |= guid == D3D11_DECODER_PROFILE_H264_VLD_NOFGT;
        profiles.push(DecoderProfile {
            name: profile_name(&guid),
            guid: format!("{guid:?}"),
        });
    }

    let h264_nv12 = has_h264 && {
        // SAFETY: valid video device; the profile GUID is a static constant.
        unsafe {
            video_device
                .CheckVideoDecoderFormat(&D3D11_DECODER_PROFILE_H264_VLD_NOFGT, DXGI_FORMAT_NV12)
        }
        .is_ok_and(|supported| supported.as_bool())
    };

    Ok(D3d11DecodeSupport {
        profiles,
        h264_nv12,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_profiles_have_unique_names_and_guids() {
        for (index, (guid, name)) in KNOWN_PROFILES.iter().enumerate() {
            assert_eq!(profile_name(guid), Some(*name));
            for (other_guid, other_name) in &KNOWN_PROFILES[index + 1..] {
                assert_ne!(guid, other_guid);
                assert_ne!(name, other_name);
            }
        }
        assert_eq!(profile_name(&GUID::zeroed()), None);
    }

    #[test]
    #[ignore = "requires a GPU with H.264 decode"]
    fn hardware_adapter_decodes_h264_to_nv12() {
        let adapters = gpu::enumerate_adapters().unwrap();
        let adapter = adapters
            .iter()
            .find(|adapter| !adapter.info.is_software)
            .expect("hardware adapter");
        let device = gpu::create_device(adapter, gpu::DebugLayer::Off).unwrap();
        let support = d3d11_decode_support(&device).unwrap();
        assert!(support.h264_nv12, "{support:?}");
    }
}
