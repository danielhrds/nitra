//! DXGI adapter and output enumeration.

use std::fmt;

use serde::{Serialize, Serializer};
use windows::Win32::Foundation::{LUID, RECT};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_COLOR_SPACE_TYPE, DXGI_MODE_ROTATION,
    DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
    DXGI_MODE_ROTATION_ROTATE270,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIAdapter1,
    IDXGIDevice, IDXGIFactory1, IDXGIOutput, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, HMONITOR,
};
use windows::core::{Interface, PCWSTR, Result};

use crate::text::wide_to_string;

/// Locally unique adapter identifier; stable until reboot or driver restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AdapterLuid {
    pub high: i32,
    pub low: u32,
}

impl From<LUID> for AdapterLuid {
    fn from(luid: LUID) -> Self {
        Self {
            high: luid.HighPart,
            low: luid.LowPart,
        }
    }
}

impl fmt::Display for AdapterLuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Reinterpret the signed high part as its raw bits for display.
        write!(f, "{:08X}-{:08X}", self.high as u32, self.low)
    }
}

impl Serialize for AdapterLuid {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// GPU vendor derived from the PCI vendor id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Amd,
    Nvidia,
    Intel,
    Microsoft,
    Other,
}

impl Vendor {
    pub fn from_pci_id(vendor_id: u32) -> Self {
        match vendor_id {
            0x1002 | 0x1022 => Vendor::Amd,
            0x10DE => Vendor::Nvidia,
            0x8086 => Vendor::Intel,
            0x1414 => Vendor::Microsoft,
            _ => Vendor::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Vendor::Amd => "AMD",
            Vendor::Nvidia => "NVIDIA",
            Vendor::Intel => "Intel",
            Vendor::Microsoft => "Microsoft",
            Vendor::Other => "other",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AdapterInfo {
    pub index: u32,
    pub name: String,
    pub vendor: Vendor,
    pub vendor_id: u32,
    pub device_id: u32,
    pub luid: AdapterLuid,
    pub dedicated_video_memory_bytes: u64,
    pub shared_system_memory_bytes: u64,
    /// Software rasterizer such as the Microsoft Basic Render Driver.
    pub is_software: bool,
    /// User-mode driver version, e.g. `32.0.31021.5001`.
    pub driver_version: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DesktopRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl From<RECT> for DesktopRect {
    fn from(rect: RECT) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }
}

/// Current display mode as reported by GDI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_rate_hz: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutputInfo {
    pub index: u32,
    /// GDI device name, e.g. `\\.\DISPLAY1`.
    pub device_name: String,
    pub attached_to_desktop: bool,
    pub desktop: DesktopRect,
    pub rotation: &'static str,
    pub current_mode: Option<DisplayMode>,
    /// The fields below need `IDXGIOutput6` and are `None` without it.
    pub bits_per_color: Option<u32>,
    pub color_space: Option<&'static str>,
    pub hdr_active: Option<bool>,
    pub max_luminance_nits: Option<f32>,
}

/// An adapter with its outputs and the DXGI handle needed to create devices.
#[derive(Clone, Debug)]
pub struct Adapter {
    pub info: AdapterInfo,
    pub outputs: Vec<Output>,
    handle: IDXGIAdapter1,
}

impl Adapter {
    pub fn handle(&self) -> &IDXGIAdapter1 {
        &self.handle
    }
}

#[derive(Clone, Debug)]
pub struct Output {
    pub info: OutputInfo,
    handle: IDXGIOutput,
    monitor: HMONITOR,
}

impl Output {
    pub fn handle(&self) -> &IDXGIOutput {
        &self.handle
    }

    /// The Win32 monitor this output scans out to (e.g. for WGC capture).
    pub fn monitor(&self) -> HMONITOR {
        self.monitor
    }
}

/// Enumerates all DXGI adapters (hardware and software) and their outputs.
pub fn enumerate_adapters() -> Result<Vec<Adapter>> {
    // SAFETY: no pointers are passed; the factory is returned as an owned COM reference.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut adapters = Vec::new();
    for index in 0_u32.. {
        // SAFETY: valid factory; indices past the end return DXGI_ERROR_NOT_FOUND.
        let handle = match unsafe { factory.EnumAdapters1(index) } {
            Ok(handle) => handle,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error),
        };
        adapters.push(Adapter {
            info: adapter_info(index, &handle)?,
            outputs: enumerate_outputs(&handle)?,
            handle,
        });
    }
    Ok(adapters)
}

fn adapter_info(index: u32, adapter: &IDXGIAdapter1) -> Result<AdapterInfo> {
    // SAFETY: valid adapter; the descriptor is returned by value.
    let desc = unsafe { adapter.GetDesc1() }?;
    // SAFETY: valid adapter and a pointer to a static interface id. Asking for
    // IDXGIDevice returns the user-mode driver version (documented behavior).
    let driver_version = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
        .ok()
        .map(format_driver_version);

    Ok(AdapterInfo {
        index,
        name: wide_to_string(&desc.Description),
        vendor: Vendor::from_pci_id(desc.VendorId),
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        luid: desc.AdapterLuid.into(),
        dedicated_video_memory_bytes: desc.DedicatedVideoMemory as u64,
        shared_system_memory_bytes: desc.SharedSystemMemory as u64,
        is_software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        driver_version,
    })
}

/// Formats a packed user-mode driver version (four 16-bit fields).
fn format_driver_version(packed: i64) -> String {
    let bits = packed as u64;
    format!(
        "{}.{}.{}.{}",
        (bits >> 48) & 0xFFFF,
        (bits >> 32) & 0xFFFF,
        (bits >> 16) & 0xFFFF,
        bits & 0xFFFF
    )
}

fn enumerate_outputs(adapter: &IDXGIAdapter1) -> Result<Vec<Output>> {
    let mut outputs = Vec::new();
    for index in 0_u32.. {
        // SAFETY: valid adapter; indices past the end return DXGI_ERROR_NOT_FOUND.
        let handle = match unsafe { adapter.EnumOutputs(index) } {
            Ok(handle) => handle,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error),
        };
        // SAFETY: valid output; the descriptor is returned by value.
        let monitor = unsafe { handle.GetDesc() }?.Monitor;
        outputs.push(Output {
            info: output_info(index, &handle)?,
            handle,
            monitor,
        });
    }
    Ok(outputs)
}

fn output_info(index: u32, output: &IDXGIOutput) -> Result<OutputInfo> {
    if let Ok(output6) = output.cast::<IDXGIOutput6>() {
        // SAFETY: valid output; the descriptor is returned by value.
        let desc = unsafe { output6.GetDesc1() }?;
        return Ok(OutputInfo {
            index,
            device_name: wide_to_string(&desc.DeviceName),
            attached_to_desktop: desc.AttachedToDesktop.as_bool(),
            desktop: desc.DesktopCoordinates.into(),
            rotation: rotation_name(desc.Rotation),
            current_mode: current_display_mode(&desc.DeviceName),
            bits_per_color: Some(desc.BitsPerColor),
            color_space: Some(color_space_name(desc.ColorSpace)),
            hdr_active: Some(desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020),
            max_luminance_nits: Some(desc.MaxLuminance),
        });
    }

    // SAFETY: valid output; the descriptor is returned by value.
    let desc = unsafe { output.GetDesc() }?;
    Ok(OutputInfo {
        index,
        device_name: wide_to_string(&desc.DeviceName),
        attached_to_desktop: desc.AttachedToDesktop.as_bool(),
        desktop: desc.DesktopCoordinates.into(),
        rotation: rotation_name(desc.Rotation),
        current_mode: current_display_mode(&desc.DeviceName),
        bits_per_color: None,
        color_space: None,
        hdr_active: None,
        max_luminance_nits: None,
    })
}

/// Reads the current mode of a GDI display device; `None` if GDI refuses.
fn current_display_mode(device_name: &[u16; 32]) -> Option<DisplayMode> {
    if !device_name.contains(&0) {
        return None;
    }
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `device_name` is NUL-terminated (checked above) and outlives the
    // call; `mode` is a live DEVMODEW whose dmSize is set as the API requires.
    let found = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(device_name.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    found.as_bool().then_some(DisplayMode {
        width: mode.dmPelsWidth,
        height: mode.dmPelsHeight,
        refresh_rate_hz: mode.dmDisplayFrequency,
    })
}

fn rotation_name(rotation: DXGI_MODE_ROTATION) -> &'static str {
    match rotation {
        DXGI_MODE_ROTATION_IDENTITY => "identity",
        DXGI_MODE_ROTATION_ROTATE90 => "rotate90",
        DXGI_MODE_ROTATION_ROTATE180 => "rotate180",
        DXGI_MODE_ROTATION_ROTATE270 => "rotate270",
        _ => "unspecified",
    }
}

fn color_space_name(color_space: DXGI_COLOR_SPACE_TYPE) -> &'static str {
    match color_space {
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 => "srgb",
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 => "hdr10",
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709 => "scrgb",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendors_from_pci_ids() {
        assert_eq!(Vendor::from_pci_id(0x1002), Vendor::Amd);
        assert_eq!(Vendor::from_pci_id(0x10DE), Vendor::Nvidia);
        assert_eq!(Vendor::from_pci_id(0x8086), Vendor::Intel);
        assert_eq!(Vendor::from_pci_id(0x1414), Vendor::Microsoft);
        assert_eq!(Vendor::from_pci_id(0x5143), Vendor::Other);
    }

    #[test]
    fn luid_formats_as_hex_pair() {
        let luid = AdapterLuid {
            high: 0,
            low: 0x0001_2A3B,
        };
        assert_eq!(luid.to_string(), "00000000-00012A3B");
        let negative = AdapterLuid { high: -1, low: 1 };
        assert_eq!(negative.to_string(), "FFFFFFFF-00000001");
    }

    #[test]
    fn luid_and_vendor_serialize_as_strings() {
        let luid = serde_json::to_string(&AdapterLuid { high: 0, low: 0xAB }).unwrap();
        assert_eq!(luid, r#""00000000-000000AB""#);
        assert_eq!(serde_json::to_string(&Vendor::Amd).unwrap(), r#""amd""#);
    }

    #[test]
    fn driver_version_unpacks_four_fields() {
        let packed = (32_i64 << 48) | (31021_i64 << 16) | 5001;
        assert_eq!(format_driver_version(packed), "32.0.31021.5001");
        let packed = (31_i64 << 48) | (7_i64 << 32) | (24033_i64 << 16) | 1003;
        assert_eq!(format_driver_version(packed), "31.7.24033.1003");
    }

    #[test]
    fn rotation_and_color_space_names() {
        assert_eq!(rotation_name(DXGI_MODE_ROTATION_IDENTITY), "identity");
        assert_eq!(rotation_name(DXGI_MODE_ROTATION(0)), "unspecified");
        assert_eq!(
            color_space_name(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020),
            "hdr10"
        );
        assert_eq!(
            color_space_name(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709),
            "srgb"
        );
    }

    #[test]
    #[ignore = "requires a GPU and display"]
    fn finds_a_hardware_adapter() {
        let adapters = enumerate_adapters().unwrap();
        let hardware: Vec<_> = adapters
            .iter()
            .filter(|adapter| !adapter.info.is_software)
            .collect();
        assert!(!hardware.is_empty(), "{adapters:#?}");
        assert!(hardware[0].info.dedicated_video_memory_bytes > 0);
        assert!(hardware[0].info.driver_version.is_some());
    }
}
