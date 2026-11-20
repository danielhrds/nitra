//! DXGI adapter/output enumeration, D3D11 device creation and debug-layer
//! helpers. Read-only with respect to the system: nothing here changes
//! display, driver or GPU settings.

mod adapter;
pub mod convert;
mod debug;
mod device;
mod frame;
mod overlay;
mod pool;
mod shader;
mod text;
mod timer;

pub use adapter::{
    Adapter, AdapterInfo, AdapterLuid, DesktopRect, DisplayMode, Output, OutputInfo, Vendor,
    enumerate_adapters,
};
pub use debug::{DebugMessage, DebugSeverity, report_live_objects};
pub use device::{D3d11Device, DebugLayer, GPU_THREAD_PRIORITY_RANGE, create_device};
pub use frame::{
    FrameFormat, GpuFrame, TexturePool, create_nv12_pool, create_texture_pool, read_pixel,
};
pub use overlay::{FormatOverlay, OverlaySupport, overlay_support};
pub use pool::{Lease, Pool};
pub use shader::{CompiledShader, ShaderError, compile_shader};
pub use text::wide_to_string;
pub use timer::{GpuTimer, TimerToken};
