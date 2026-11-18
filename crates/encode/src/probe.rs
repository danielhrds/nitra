//! Encoder capability checks: Media Foundation hardware MFTs and the AMF runtime.

use mf::{MftInfo, enumerate_mfts, video_type};
use serde::Serialize;
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::Media::MediaFoundation::{
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFVideoFormat_H264,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{Result, s, w};

/// Hardware H.264 encoder MFTs, best match first.
/// The caller must have initialized COM and started Media Foundation.
pub fn h264_hardware_encoders() -> Result<Vec<MftInfo>> {
    enumerate_mfts(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
        None,
        Some(video_type(MFVideoFormat_H264)),
    )
}

/// State of the AMD AMF runtime (`amfrt64.dll`, installed with the AMD driver).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AmfRuntime {
    NotInstalled,
    Present {
        version: String,
    },
    /// The DLL loaded but did not export or answer `AMFQueryVersion`.
    QueryFailed {
        detail: String,
    },
}

/// `AMF_RESULT AMF_CDECL_CALL AMFQueryVersion(amf_uint64* pVersion)`.
type AmfQueryVersion = unsafe extern "C" fn(version: *mut u64) -> i32;
const AMF_OK: i32 = 0;

/// Loads the AMF runtime from System32 only, reads its version and unloads it.
pub fn amf_runtime() -> AmfRuntime {
    // SAFETY: the file name is a static NUL-terminated string; restricting the
    // search to System32 prevents loading a planted DLL from other folders.
    let Ok(module) =
        (unsafe { LoadLibraryExW(w!("amfrt64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) })
    else {
        return AmfRuntime::NotInstalled;
    };
    let module = LoadedLibrary(module);

    // SAFETY: valid module handle and a static NUL-terminated symbol name.
    let Some(symbol) = (unsafe { GetProcAddress(module.0, s!("AMFQueryVersion")) }) else {
        return AmfRuntime::QueryFailed {
            detail: "AMFQueryVersion not exported".to_owned(),
        };
    };
    // SAFETY: AMFQueryVersion has the signature documented in AMF's
    // core/Factory.h; cdecl and the system ABI are identical on x86-64.
    let query_version: AmfQueryVersion = unsafe { std::mem::transmute(symbol) };

    let mut packed = 0_u64;
    // SAFETY: the pointer refers to a live local u64; the module stays loaded
    // until `module` is dropped at the end of this function.
    let result = unsafe { query_version(&mut packed) };
    if result == AMF_OK {
        AmfRuntime::Present {
            version: format_amf_version(packed),
        }
    } else {
        AmfRuntime::QueryFailed {
            detail: format!("AMFQueryVersion returned {result}"),
        }
    }
}

/// Unloads the library on drop.
struct LoadedLibrary(HMODULE);

impl Drop for LoadedLibrary {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful LoadLibraryExW and no code
        // or data from the module is used after this point.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

/// AMF packs major.minor.release.build into four 16-bit fields.
fn format_amf_version(packed: u64) -> String {
    format!(
        "{}.{}.{}.{}",
        (packed >> 48) & 0xFFFF,
        (packed >> 32) & 0xFFFF,
        (packed >> 16) & 0xFFFF,
        packed & 0xFFFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amf_version_unpacks_four_fields() {
        let packed = (1_u64 << 48) | (4 << 32) | (36 << 16) | 2;
        assert_eq!(format_amf_version(packed), "1.4.36.2");
    }

    #[test]
    fn amf_status_serializes_with_tag() {
        let json = serde_json::to_string(&AmfRuntime::Present {
            version: "1.4.36.0".to_owned(),
        })
        .unwrap();
        assert_eq!(json, r#"{"status":"present","version":"1.4.36.0"}"#);
        assert_eq!(
            serde_json::to_string(&AmfRuntime::NotInstalled).unwrap(),
            r#"{"status":"not_installed"}"#
        );
    }

    #[test]
    #[ignore = "requires the AMD driver"]
    fn amf_runtime_is_present_on_amd() {
        assert!(
            matches!(amf_runtime(), AmfRuntime::Present { .. }),
            "{:?}",
            amf_runtime()
        );
    }
}
