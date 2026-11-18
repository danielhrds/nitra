//! Media Foundation helpers shared by the `encode` and `decode` crates:
//! runtime start/stop and MFT enumeration.
//!
//! The caller must have initialized COM on the calling thread (MTA).

use serde::Serialize;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MF_TRANSFORM_FLAGS_Attribute, MF_VERSION, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFShutdown, MFStartup, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO,
    MFT_TRANSFORM_CLSID_Attribute, MFTEnumEx,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Result};

/// Keeps the Media Foundation runtime started; shuts it down on drop.
#[derive(Debug)]
pub struct MediaFoundation(());

impl MediaFoundation {
    pub fn start() -> Result<Self> {
        // SAFETY: plain runtime initialization; balanced by MFShutdown in Drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }?;
        Ok(Self(()))
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // SAFETY: balances the successful MFStartup in `start`.
        // A shutdown failure cannot be acted upon here, so it is ignored.
        let _ = unsafe { MFShutdown() };
    }
}

/// What a registered transform reports about itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MftInfo {
    pub name: Option<String>,
    pub clsid: Option<String>,
    /// Registered as a hardware MFT (driver-provided).
    pub hardware: bool,
    /// Hardware MFTs always use the asynchronous model (Media Foundation
    /// docs, "Hardware MFTs") even when their registration flags omit it;
    /// for other MFTs this is the registration flag.
    pub asynchronous: bool,
    /// For hardware MFTs, e.g. `VEN_1002`.
    pub hardware_vendor_id: Option<String>,
}

/// Input/output filter for a video subtype such as `MFVideoFormat_H264`.
pub fn video_type(subtype: GUID) -> MFT_REGISTER_TYPE_INFO {
    MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    }
}

/// Lists transforms of `category` matching the optional input/output types.
/// Transforms are only described, never instantiated.
pub fn enumerate_mfts(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<MFT_REGISTER_TYPE_INFO>,
    output: Option<MFT_REGISTER_TYPE_INFO>,
) -> Result<Vec<MftInfo>> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: the type filters and out-pointers reference live locals.
    unsafe {
        MFTEnumEx(
            category,
            flags,
            input.as_ref().map(|info| info as *const _),
            output.as_ref().map(|info| info as *const _),
            &mut activates,
            &mut count,
        )
    }?;
    if activates.is_null() {
        return Ok(Vec::new());
    }

    // SAFETY: on success MFTEnumEx returns a CoTaskMemAlloc'd array of `count`
    // owned IMFActivate references. Each is moved out (and released when
    // dropped) before the array itself is freed below.
    let entries = unsafe { std::slice::from_raw_parts_mut(activates, count as usize) };
    let infos = entries
        .iter_mut()
        .filter_map(Option::take)
        .map(|activate| mft_info(&activate))
        .collect();
    // SAFETY: the array came from MFTEnumEx and is no longer referenced.
    unsafe { CoTaskMemFree(Some(activates.cast())) };
    Ok(infos)
}

fn mft_info(activate: &IMFActivate) -> MftInfo {
    // SAFETY: valid activation object; attribute keys are static GUIDs.
    let flags = unsafe { activate.GetUINT32(&MF_TRANSFORM_FLAGS_Attribute) }.unwrap_or(0);
    // SAFETY: as above.
    let clsid = unsafe { activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok();
    let hardware = has_flag(flags, MFT_ENUM_FLAG_HARDWARE);
    MftInfo {
        name: string_attribute(activate, &MFT_FRIENDLY_NAME_Attribute),
        clsid: clsid.map(|clsid| format!("{clsid:?}")),
        hardware,
        asynchronous: hardware || has_flag(flags, MFT_ENUM_FLAG_ASYNCMFT),
        hardware_vendor_id: string_attribute(activate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute),
    }
}

fn has_flag(flags: u32, flag: MFT_ENUM_FLAG) -> bool {
    flags & flag.0 as u32 != 0
}

fn string_attribute(activate: &IMFActivate, key: &GUID) -> Option<String> {
    // SAFETY: valid activation object; `key` is a static GUID.
    let length = unsafe { activate.GetStringLength(key) }.ok()? as usize;
    let mut buffer = vec![0_u16; length + 1];
    // SAFETY: `buffer` has room for the string plus its terminating NUL.
    unsafe { activate.GetString(key, &mut buffer, None) }.ok()?;
    Some(String::from_utf16_lossy(&buffer[..length]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_SYNCMFT;

    #[test]
    fn flags_are_decoded() {
        let flags = (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_ASYNCMFT.0) as u32;
        assert!(has_flag(flags, MFT_ENUM_FLAG_HARDWARE));
        assert!(has_flag(flags, MFT_ENUM_FLAG_ASYNCMFT));
        assert!(!has_flag(flags, MFT_ENUM_FLAG_SYNCMFT));
    }
}
