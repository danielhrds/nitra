//! DXGI adapter/output enumeration, D3D11 device creation and debug-layer
//! helpers. Read-only with respect to the system: nothing here changes
//! display, driver or GPU settings.

mod adapter;
mod debug;
mod device;
mod shader;
mod text;

pub use adapter::{
    Adapter, AdapterInfo, AdapterLuid, DesktopRect, DisplayMode, Output, OutputInfo, Vendor,
    enumerate_adapters,
};
pub use debug::{DebugMessage, DebugSeverity, report_live_objects};
pub use device::{D3d11Device, DebugLayer, create_device};
pub use shader::{CompiledShader, ShaderError, compile_shader};
pub use text::wide_to_string;
