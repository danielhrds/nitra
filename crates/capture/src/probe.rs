//! Capture capability checks that never start capturing.

use std::sync::OnceLock;

use serde::Serialize;
use windows::Foundation::Metadata::ApiInformation;
use windows::Graphics::Capture::GraphicsCaptureSession;
use windows::Win32::Graphics::Dxgi::{IDXGIOutput, IDXGIOutput1, IDXGIOutput5};
use windows::core::{HSTRING, Interface, Result};

const SESSION_TYPE: &str = "Windows.Graphics.Capture.GraphicsCaptureSession";

/// Windows.Graphics.Capture availability and optional session properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct WgcSupport {
    pub supported: bool,
    /// `IsBorderRequired`: the yellow capture border can be turned off.
    pub border_toggle: bool,
    /// `IsCursorCaptureEnabled`: cursor capture can be turned off.
    pub cursor_toggle: bool,
    /// `MinUpdateInterval`: the frame rate can be capped by the system.
    pub min_update_interval: bool,
}

/// Queries WGC support. The caller must have initialized COM/WinRT.
pub fn wgc_support() -> Result<WgcSupport> {
    let has_property = |name: &str| {
        ApiInformation::IsPropertyPresent(&HSTRING::from(SESSION_TYPE), &HSTRING::from(name))
            .unwrap_or(false)
    };
    Ok(WgcSupport {
        supported: wgc_is_supported()?,
        border_toggle: has_property("IsBorderRequired"),
        cursor_toggle: has_property("IsCursorCaptureEnabled"),
        min_update_interval: has_property("MinUpdateInterval"),
    })
}

/// `GraphicsCaptureSession::IsSupported`, queried once per process.
///
/// Measured on Windows 11 25H2 (build 26200): every call to `IsSupported`
/// leaks one process handle inside the OS (+50 handles over 50 calls, while
/// `ApiInformation` queries through the same WinRT path leak none). Support
/// cannot change while the process runs, so the first successful answer is
/// cached; failures are not cached and are retried on the next call.
fn wgc_is_supported() -> Result<bool> {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    if let Some(&supported) = SUPPORTED.get() {
        return Ok(supported);
    }
    let supported = GraphicsCaptureSession::IsSupported()?;
    Ok(*SUPPORTED.get_or_init(|| supported))
}

/// Which Desktop Duplication entry points an output exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DuplicationSupport {
    /// `IDXGIOutput1::DuplicateOutput` (BGRA only).
    pub output1: bool,
    /// `IDXGIOutput5::DuplicateOutput1` (format list, e.g. FP16 for HDR).
    pub output5: bool,
}

/// Checks the interfaces only; no duplication is created.
pub fn dxgi_duplication_support(output: &IDXGIOutput) -> DuplicationSupport {
    DuplicationSupport {
        output1: output.cast::<IDXGIOutput1>().is_ok(),
        output5: output.cast::<IDXGIOutput5>().is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires Windows 10 1903+ desktop session"]
    fn wgc_is_supported_on_windows_11() {
        let support = wgc_support().unwrap();
        assert!(support.supported, "{support:?}");
    }
}
