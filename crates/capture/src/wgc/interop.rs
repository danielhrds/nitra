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

/// A top-level window someone would pick to share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareableWindow {
    pub hwnd: isize,
    pub title: String,
    pub process_id: u32,
    /// Its size on screen, frame included (what WGC captures).
    pub width: u32,
    pub height: u32,
}

/// The windows a person would pick to share, in Z order (front first):
/// visible, titled, not cloaked (hidden UWP apps and other virtual desktops
/// stay listed as visible), not minimized (WGC delivers no frames for them),
/// not tool or owned windows (palettes, dialogs), and not of
/// `exclude_process` (the app itself).
pub fn shareable_windows(exclude_process: u32) -> Vec<ShareableWindow> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Dwm::{
        DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowThreadProcessId, IsIconic,
        WS_EX_TOOLWINDOW,
    };

    struct Search {
        exclude_process: u32,
        found: Vec<ShareableWindow>,
    }

    unsafe extern "system" fn visit(window: HWND, state: LPARAM) -> BOOL {
        // SAFETY: `state` is the address of the `Search` below, which
        // outlives the synchronous EnumWindows call.
        let search = unsafe { &mut *(state.0 as *mut Search) };
        // SAFETY (whole block): plain queries on a window handle supplied by
        // EnumWindows, with out-pointers to locals of the right size.
        unsafe {
            let length = GetWindowTextLengthW(window);
            let tool = GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0;
            let owned = GetWindow(window, GW_OWNER).is_ok_and(|o| !o.is_invalid());
            if !IsWindowVisible(window).as_bool()
                || IsIconic(window).as_bool()
                || length <= 0
                || tool
                || owned
            {
                return true.into();
            }
            let mut cloaked = 0_u32;
            let cloak_read = DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED,
                (&raw mut cloaked).cast(),
                size_of::<u32>() as u32,
            );
            if cloak_read.is_ok() && cloaked != 0 {
                return true.into();
            }
            let mut process_id = 0;
            GetWindowThreadProcessId(window, Some(&mut process_id));
            if process_id == search.exclude_process {
                return true.into();
            }
            let mut bounds = RECT::default();
            let _ = DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&raw mut bounds).cast(),
                size_of::<RECT>() as u32,
            );
            let (width, height) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
            if width <= 0 || height <= 0 {
                return true.into();
            }
            let mut buffer = vec![0_u16; length as usize + 1];
            let copied = GetWindowTextW(window, &mut buffer);
            search.found.push(ShareableWindow {
                hwnd: window.0 as isize,
                title: String::from_utf16_lossy(&buffer[..copied.max(0) as usize]),
                process_id,
                width: width as u32,
                height: height as u32,
            });
        }
        true.into()
    }

    let mut search = Search {
        exclude_process,
        found: Vec::new(),
    };
    // SAFETY: the callback only dereferences `search` during this call.
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

    #[test]
    fn shareable_windows_leave_out_the_excluded_process() {
        // Whatever is open on this PC: the list is well formed, and asking
        // to leave out a process removes exactly its windows.
        let all = shareable_windows(0);
        for window in &all {
            assert!(!window.title.is_empty() && window.width > 0 && window.height > 0);
        }
        if let Some(first) = all.first() {
            let without = shareable_windows(first.process_id);
            assert!(without.iter().all(|w| w.process_id != first.process_id));
        }
    }
}
