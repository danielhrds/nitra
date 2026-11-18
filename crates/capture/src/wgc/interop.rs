//! Win32/WinRT interop needed by WGC: capture items from HMONITOR/HWND,
//! the WinRT wrapper of our D3D11 device, and frame surface access.

use gpu::D3d11Device;
use telemetry::{QpcFrequency, QpcTimestamp};
use windows::Graphics::Capture::{Direct3D11CaptureFrame, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
};
use windows::core::{BOOL, Interface, Result};

/// WinRT timespans count 100 ns units.
const TIMESPAN_TICKS_PER_SECOND: i128 = 10_000_000;

pub(super) fn item_for_monitor(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: valid interop factory; an invalid monitor handle makes the call fail.
    unsafe { interop.CreateForMonitor(monitor) }
}

pub(super) fn item_for_window(window: HWND) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    // SAFETY: valid interop factory; an invalid window handle makes the call fail.
    unsafe { interop.CreateForWindow(window) }
}

/// Wraps our D3D11 device for WinRT APIs (WGC frame pools).
pub(super) fn winrt_device(device: &D3d11Device) -> Result<IDirect3DDevice> {
    let dxgi_device: IDXGIDevice = device.raw().cast()?;
    // SAFETY: valid DXGI device; returns an owned WinRT object.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }?;
    inspectable.cast()
}

/// The D3D11 texture behind a captured frame. Valid until the frame is closed.
pub(super) fn surface_texture(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D> {
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    // SAFETY: valid access interface; returns an owned reference.
    unsafe { access.GetInterface() }
}

/// Converts a WGC `SystemRelativeTime` (QPC time expressed in 100 ns units)
/// back to QPC ticks.
pub fn timespan_to_qpc(duration_100ns: i64, frequency: QpcFrequency) -> QpcTimestamp {
    let ticks = i128::from(duration_100ns) * i128::from(frequency.ticks_per_second())
        / TIMESPAN_TICKS_PER_SECOND;
    QpcTimestamp::from_ticks(ticks as i64)
}

/// First visible top-level window whose title contains `needle`
/// (case-insensitive), with its full title.
pub fn find_window_by_title(needle: &str) -> Option<(HWND, String)> {
    struct Search {
        needle: String,
        found: Option<(HWND, String)>,
    }

    unsafe extern "system" fn visit(window: HWND, state: LPARAM) -> BOOL {
        // SAFETY: `state` is the address of the `Search` below, which
        // outlives the synchronous EnumWindows call.
        let search = unsafe { &mut *(state.0 as *mut Search) };
        // SAFETY: plain queries on a window handle supplied by EnumWindows.
        let (visible, length) = unsafe { (IsWindowVisible(window), GetWindowTextLengthW(window)) };
        if !visible.as_bool() || length <= 0 {
            return true.into();
        }
        let mut buffer = vec![0_u16; length as usize + 1];
        // SAFETY: the buffer has room for the title and its terminator.
        let copied = unsafe { GetWindowTextW(window, &mut buffer) };
        let title = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
        if title.to_lowercase().contains(&search.needle) {
            search.found = Some((window, title));
            return false.into();
        }
        true.into()
    }

    let mut search = Search {
        needle: needle.to_lowercase(),
        found: None,
    };
    // SAFETY: the callback only dereferences `search` during this call.
    // EnumWindows reports an error when the callback stops early; that is
    // the "found" case, so the result is ignored.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timespan_converts_to_qpc_ticks() {
        let ten_mhz = QpcFrequency::new(10_000_000).unwrap();
        assert_eq!(timespan_to_qpc(123_456_789, ten_mhz).ticks(), 123_456_789);
        let three_mhz = QpcFrequency::new(3_000_000).unwrap();
        // 1 s = 10 000 000 units = 3 000 000 ticks.
        assert_eq!(timespan_to_qpc(10_000_000, three_mhz).ticks(), 3_000_000);
    }
}
