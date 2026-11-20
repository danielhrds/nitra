//! D3D11 device creation on a specific adapter.

use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_SDK_COMPONENT_MISSING, IDXGIAdapter1, IDXGIDevice,
};
use windows::core::{Interface, Result};

use crate::adapter::Adapter;
use crate::debug::{DebugMessage, drain_d3d11_messages};

/// Highest first; the media pipeline needs at least 11_0.
const FEATURE_LEVELS: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

/// Range accepted by `IDXGIDevice::SetGPUThreadPriority` (0 is normal).
pub const GPU_THREAD_PRIORITY_RANGE: std::ops::RangeInclusive<i32> = -7..=7;

/// Whether to request the D3D11 debug layer (needs the Windows "Graphics Tools" feature).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugLayer {
    Off,
    /// Enable it when installed; otherwise create the device without it.
    IfAvailable,
}

/// A D3D11 device with BGRA and video support.
#[derive(Clone, Debug)]
pub struct D3d11Device {
    device: ID3D11Device,
    feature_level: D3D_FEATURE_LEVEL,
    debug_layer_enabled: bool,
}

impl D3d11Device {
    pub fn raw(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn feature_level_name(&self) -> &'static str {
        feature_level_name(self.feature_level)
    }

    pub fn debug_layer_enabled(&self) -> bool {
        self.debug_layer_enabled
    }

    /// The device's immediate context. D3D11 contexts are not thread-safe:
    /// exactly one thread may use it at a time, which callers must ensure.
    pub fn immediate_context(&self) -> Result<ID3D11DeviceContext> {
        // SAFETY: valid device; returns an owned reference to its context.
        unsafe { self.device.GetImmediateContext() }
    }

    /// Serializes use of the immediate context across threads (required
    /// when other components, e.g. WGC or Media Foundation, use the device
    /// from their own threads). Idempotent.
    pub fn enable_multithread_protection(&self) -> Result<()> {
        let multithread: ID3D11Multithread = self.device.cast()?;
        // SAFETY: valid interface; the call only toggles an internal lock.
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
        Ok(())
    }

    /// Sets the GPU scheduling priority of this device's work relative to
    /// other devices (`IDXGIDevice::SetGPUThreadPriority`): negative lets
    /// other processes (e.g. a game) go first, 0 is normal. Raising it above
    /// 0 may need a privilege. The driver may ignore it (e.g. under
    /// hardware-accelerated GPU scheduling); measure.
    pub fn set_gpu_thread_priority(&self, priority: i32) -> Result<()> {
        if !GPU_THREAD_PRIORITY_RANGE.contains(&priority) {
            return Err(E_INVALIDARG.into());
        }
        let dxgi: IDXGIDevice = self.device.cast()?;
        // SAFETY: valid interface; plain setter with a value in its range.
        unsafe { dxgi.SetGPUThreadPriority(priority) }
    }

    /// The device's current GPU scheduling priority.
    pub fn gpu_thread_priority(&self) -> Result<i32> {
        let dxgi: IDXGIDevice = self.device.cast()?;
        // SAFETY: valid interface; plain getter.
        unsafe { dxgi.GetGPUThreadPriority() }
    }

    /// Removes and returns the messages the debug layer stored so far.
    /// Always empty when the debug layer is off.
    pub fn take_debug_messages(&self) -> Vec<DebugMessage> {
        if self.debug_layer_enabled {
            drain_d3d11_messages(&self.device)
        } else {
            Vec::new()
        }
    }
}

/// Creates a device on `adapter`. With [`DebugLayer::IfAvailable`] a missing
/// debug layer is logged and the device is created without it.
pub fn create_device(adapter: &Adapter, debug_layer: DebugLayer) -> Result<D3d11Device> {
    let base_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;

    if debug_layer == DebugLayer::IfAvailable {
        match create(adapter.handle(), base_flags | D3D11_CREATE_DEVICE_DEBUG) {
            Ok((device, feature_level)) => {
                return Ok(D3d11Device {
                    device,
                    feature_level,
                    debug_layer_enabled: true,
                });
            }
            Err(error) if error.code() == DXGI_ERROR_SDK_COMPONENT_MISSING => {
                tracing::warn!(
                    "D3D11 debug layer unavailable (Windows Graphics Tools not installed); \
                     creating the device without it"
                );
            }
            Err(error) => return Err(error),
        }
    }

    let (device, feature_level) = create(adapter.handle(), base_flags)?;
    Ok(D3d11Device {
        device,
        feature_level,
        debug_layer_enabled: false,
    })
}

fn create(
    adapter: &IDXGIAdapter1,
    flags: D3D11_CREATE_DEVICE_FLAG,
) -> Result<(ID3D11Device, D3D_FEATURE_LEVEL)> {
    let mut device = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();
    // SAFETY: the out-pointers reference live locals for the duration of the
    // call. With an explicit adapter the driver type must be UNKNOWN and no
    // software rasterizer module is passed. The immediate context is not
    // requested; callers that need it get it from the device.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(&FEATURE_LEVELS),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            None,
        )
    }?;
    let device = device.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
    Ok((device, feature_level))
}

fn feature_level_name(level: D3D_FEATURE_LEVEL) -> &'static str {
    match level {
        D3D_FEATURE_LEVEL_11_0 => "11_0",
        D3D_FEATURE_LEVEL_11_1 => "11_1",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::enumerate_adapters;

    #[test]
    fn feature_level_names() {
        assert_eq!(feature_level_name(D3D_FEATURE_LEVEL_11_1), "11_1");
        assert_eq!(feature_level_name(D3D_FEATURE_LEVEL_11_0), "11_0");
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn creates_device_on_hardware_adapter() {
        let adapters = enumerate_adapters().unwrap();
        let adapter = adapters
            .iter()
            .find(|adapter| !adapter.info.is_software)
            .expect("hardware adapter");
        let device = create_device(adapter, DebugLayer::IfAvailable).unwrap();
        assert_ne!(device.feature_level_name(), "other");
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn lowers_and_restores_gpu_thread_priority() {
        let adapters = enumerate_adapters().unwrap();
        let adapter = adapters
            .iter()
            .find(|adapter| !adapter.info.is_software)
            .expect("hardware adapter");
        let device = create_device(adapter, DebugLayer::Off).unwrap();
        assert_eq!(device.gpu_thread_priority().unwrap(), 0);
        device.set_gpu_thread_priority(-7).unwrap();
        assert_eq!(device.gpu_thread_priority().unwrap(), -7);
        device.set_gpu_thread_priority(0).unwrap();
        assert_eq!(device.gpu_thread_priority().unwrap(), 0);
        assert!(device.set_gpu_thread_priority(8).is_err());
        assert!(device.set_gpu_thread_priority(-8).is_err());
    }
}
