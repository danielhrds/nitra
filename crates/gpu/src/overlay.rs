//! Hardware overlay (multiplane overlay, MPO) support of an output, as the
//! driver reports it: whether a swap chain in a given format can be shown on
//! its own plane, scanned out without the compositor redrawing it.

use serde::Serialize;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_NV12, DXGI_FORMAT_YUY2,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_CURSOR_STRETCHED,
    DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_FULLSCREEN,
    DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_WINDOWED, DXGI_OVERLAY_COLOR_SPACE_SUPPORT_FLAG_PRESENT,
    DXGI_OVERLAY_SUPPORT_FLAG_DIRECT, DXGI_OVERLAY_SUPPORT_FLAG_SCALING, IDXGIOutput3,
    IDXGIOutput4, IDXGIOutput6,
};
use windows::core::Interface;

use crate::adapter::Output;
use crate::device::D3d11Device;

/// Overlay support for one swap chain format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FormatOverlay {
    /// Shown on an overlay plane at its own size.
    pub direct: bool,
    /// Shown on an overlay plane scaled to the window.
    pub scaling: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct OverlaySupport {
    /// `None` without `IDXGIOutput3` (or when the query fails).
    pub nv12: Option<FormatOverlay>,
    pub yuy2: Option<FormatOverlay>,
    pub bgra: Option<FormatOverlay>,
    /// NV12 in BT.709 studio range (what the decoder outputs) on an overlay
    /// plane; `None` without `IDXGIOutput4`.
    pub nv12_bt709: Option<bool>,
    /// Hardware composition (`IDXGIOutput6`): fullscreen, windowed, and a
    /// stretched cursor.
    pub composition_fullscreen: Option<bool>,
    pub composition_windowed: Option<bool>,
    pub composition_cursor_stretched: Option<bool>,
}

/// Asks the driver what `output` can put on an overlay plane for swap
/// chains of `device`. Read-only.
pub fn overlay_support(output: &Output, device: &D3d11Device) -> OverlaySupport {
    let handle = output.handle();
    let mut support = OverlaySupport::default();
    if let Ok(output3) = handle.cast::<IDXGIOutput3>() {
        let check = |format: DXGI_FORMAT| {
            // SAFETY: valid output and device; the flags are returned by value.
            unsafe { output3.CheckOverlaySupport(format, device.raw()) }
                .ok()
                .map(|flags| FormatOverlay {
                    direct: flags & DXGI_OVERLAY_SUPPORT_FLAG_DIRECT.0 as u32 != 0,
                    scaling: flags & DXGI_OVERLAY_SUPPORT_FLAG_SCALING.0 as u32 != 0,
                })
        };
        support.nv12 = check(DXGI_FORMAT_NV12);
        support.yuy2 = check(DXGI_FORMAT_YUY2);
        support.bgra = check(DXGI_FORMAT_B8G8R8A8_UNORM);
    }
    if let Ok(output4) = handle.cast::<IDXGIOutput4>() {
        // SAFETY: valid output and device; the flags are returned by value.
        support.nv12_bt709 = unsafe {
            output4.CheckOverlayColorSpaceSupport(
                DXGI_FORMAT_NV12,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                device.raw(),
            )
        }
        .ok()
        .map(|flags| flags & DXGI_OVERLAY_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 != 0);
    }
    if let Ok(output6) = handle.cast::<IDXGIOutput6>() {
        // SAFETY: valid output; the flags are returned by value.
        if let Ok(flags) = unsafe { output6.CheckHardwareCompositionSupport() } {
            let has = |flag: i32| Some(flags & flag as u32 != 0);
            support.composition_fullscreen =
                has(DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_FULLSCREEN.0);
            support.composition_windowed = has(DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_WINDOWED.0);
            support.composition_cursor_stretched =
                has(DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_CURSOR_STRETCHED.0);
        }
    }
    support
}
