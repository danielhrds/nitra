//! The window's swap chain. Video tiles are textures of their own
//! (`tiles.rs`) that Dear ImGui draws like any image.
//!
//! The swap chain is shown through DirectComposition, as browsers and
//! Discord do, not bound to the window: the desktop compositor always
//! composes it, so the monitor keeps its own refresh rate (and the mouse
//! its smoothness) while the window, maximized, draws at most 60 times a
//! second. A swap chain bound to a window that fills the monitor is treated
//! like a game: with variable refresh, the monitor follows its frame rate.

use gpu::D3d11Device;

use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
    DXGI_SCALING_NONE, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2, IDXGISwapChain2,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Interface;

/// How a lost GPU device reads in errors (the window restarts the app).
pub const DEVICE_LOST: &str = "GPU device lost";

pub struct Gfx {
    device: D3d11Device,
    context: ID3D11DeviceContext,
    chain: IDXGISwapChain2,
    waitable: HANDLE,
    pub size: (u32, u32),
    target: Option<ID3D11RenderTargetView>,
    /// Testing: `ZIZ_TEST_DEVICE_LOST=<seconds>` makes `present` report a
    /// lost device after that long (the recovery cannot be tested by
    /// resetting the driver). Unset in normal use.
    lose_device_at: Option<std::time::Instant>,
    /// What shows the swap chain in the window; kept alive with it (None
    /// with the window-bound swap chain).
    _composition: Option<Composition>,
}

/// The DirectComposition objects that put the swap chain in the window.
struct Composition {
    _device: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
}

impl Gfx {
    pub fn new(device: &D3d11Device, window: HWND, size: (u32, u32)) -> Result<Self, String> {
        // Testing: `ZIZ_TEST_HWND_SWAPCHAIN=1` binds the swap chain to the
        // window as before (to compare). Unset in normal use.
        let composed = std::env::var_os("ZIZ_TEST_HWND_SWAPCHAIN").is_none();
        let flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0.max(1),
            Height: size.1.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            // Composition swap chains do not take SCALING_NONE; the buffers
            // always match the window, so nothing is stretched.
            Scaling: if composed {
                DXGI_SCALING_STRETCH
            } else {
                DXGI_SCALING_NONE
            },
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags.0 as u32,
            ..Default::default()
        };
        // SAFETY: valid device and window; the factory is the one that
        // created the device's adapter. The composition objects are kept
        // in `Gfx` as long as the swap chain they show.
        let (chain, waitable, composition) = unsafe {
            let dxgi: IDXGIDevice = device.raw().cast().map_err(|e| e.to_string())?;
            let factory: IDXGIFactory2 = dxgi
                .GetAdapter()
                .and_then(|adapter| adapter.GetParent())
                .map_err(|e| e.to_string())?;
            let (chain, composition) = if composed {
                let chain = factory
                    .CreateSwapChainForComposition(device.raw(), &desc, None)
                    .map_err(|e| format!("composition swap chain: {e}"))?;
                let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)
                    .map_err(|e| format!("DirectComposition: {e}"))?;
                // Below the window's children: watched screens are child
                // windows drawn over the interface.
                let target = dcomp
                    .CreateTargetForHwnd(window, false)
                    .map_err(|e| e.to_string())?;
                let visual = dcomp.CreateVisual().map_err(|e| e.to_string())?;
                visual.SetContent(&chain).map_err(|e| e.to_string())?;
                target.SetRoot(&visual).map_err(|e| e.to_string())?;
                dcomp.Commit().map_err(|e| e.to_string())?;
                let composition = Composition {
                    _device: dcomp,
                    _target: target,
                    _visual: visual,
                };
                (chain, Some(composition))
            } else {
                let chain = factory
                    .CreateSwapChainForHwnd(device.raw(), window, &desc, None, None)
                    .map_err(|e| e.to_string())?;
                (chain, None)
            };
            factory
                .MakeWindowAssociation(window, DXGI_MWA_NO_ALT_ENTER)
                .map_err(|e| e.to_string())?;
            let chain: IDXGISwapChain2 = chain.cast().map_err(|e| e.to_string())?;
            chain.SetMaximumFrameLatency(1).map_err(|e| e.to_string())?;
            let waitable = chain.GetFrameLatencyWaitableObject();
            (chain, waitable, composition)
        };
        tracing::info!(composed, "window swap chain");
        Ok(Self {
            device: device.clone(),
            context: device.immediate_context().map_err(|e| e.to_string())?,
            chain,
            waitable,
            size,
            target: None,
            lose_device_at: std::env::var("ZIZ_TEST_DEVICE_LOST")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .map(|s| std::time::Instant::now() + std::time::Duration::from_secs_f64(s)),
            _composition: composition,
        })
    }

    pub fn resize(&mut self, size: (u32, u32)) -> Result<(), String> {
        if size == self.size || size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        self.target = None;
        // SAFETY: no outstanding back buffer references (views dropped).
        unsafe {
            self.chain.ResizeBuffers(
                0,
                size.0,
                size.1,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            )
        }
        .map_err(|e| format!("ResizeBuffers: {e}"))?;
        self.size = size;
        Ok(())
    }

    /// Waits (bounded) until the swap chain can take a frame.
    pub fn wait_ready(&self) {
        // SAFETY: valid waitable handle.
        let _ = unsafe { WaitForSingleObject(self.waitable, 100) } == WAIT_OBJECT_0;
    }

    /// Clears the back buffer and binds the back
    /// buffer as the render target for Dear ImGui.
    pub fn begin(&mut self, background: [f32; 4]) -> Result<(), String> {
        // SAFETY: valid swap chain; buffer 0 is the current back buffer.
        let back: ID3D11Texture2D =
            unsafe { self.chain.GetBuffer(0) }.map_err(|e| e.to_string())?;
        if self.target.is_none() {
            let mut view = None;
            // SAFETY: valid device and back buffer; live out-pointer.
            unsafe {
                self.device
                    .raw()
                    .CreateRenderTargetView(&back, None, Some(&mut view))
            }
            .map_err(|e| e.to_string())?;
            self.target = view;
        }
        let target = self.target.as_ref().expect("created above");
        // SAFETY: valid view of this device.
        unsafe { self.context.ClearRenderTargetView(target, &background) };
        // SAFETY: valid view of this device.
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(target.clone())]), None)
        };
        Ok(())
    }

    pub fn present(&self) -> Result<(), String> {
        if self
            .lose_device_at
            .is_some_and(|at| std::time::Instant::now() >= at)
        {
            return Err(format!("{DEVICE_LOST} (simulated for testing)"));
        }
        // SAFETY: valid swap chain; sync interval 1.
        let result = unsafe { self.chain.Present(1, DXGI_PRESENT(0)) };
        if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
            return Err(format!("{DEVICE_LOST} ({result:?})"));
        }
        result.ok().map_err(|e| format!("Present: {e}"))
    }
}
