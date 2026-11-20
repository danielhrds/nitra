//! The window's swap chain. Video tiles are textures of their own
//! (`tiles.rs`) that Dear ImGui draws like any image.


use gpu::D3d11Device;

use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{

    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
    DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2,
    IDXGISwapChain2,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Interface;


pub struct Gfx {
    device: D3d11Device,
    context: ID3D11DeviceContext,
    chain: IDXGISwapChain2,
    waitable: HANDLE,
    pub size: (u32, u32),
    target: Option<ID3D11RenderTargetView>,
}

impl Gfx {
    pub fn new(device: &D3d11Device, window: HWND, size: (u32, u32)) -> Result<Self, String> {
        let flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags.0 as u32,
            ..Default::default()
        };
        // SAFETY: valid device and window; the factory is the one that
        // created the device's adapter.
        let (chain, waitable) = unsafe {
            let dxgi: IDXGIDevice = device.raw().cast().map_err(|e| e.to_string())?;
            let factory: IDXGIFactory2 = dxgi
                .GetAdapter()
                .and_then(|adapter| adapter.GetParent())
                .map_err(|e| e.to_string())?;
            let chain = factory
                .CreateSwapChainForHwnd(device.raw(), window, &desc, None, None)
                .map_err(|e| e.to_string())?;
            factory
                .MakeWindowAssociation(window, DXGI_MWA_NO_ALT_ENTER)
                .map_err(|e| e.to_string())?;
            let chain: IDXGISwapChain2 = chain.cast().map_err(|e| e.to_string())?;
            chain.SetMaximumFrameLatency(1).map_err(|e| e.to_string())?;
            let waitable = chain.GetFrameLatencyWaitableObject();
            (chain, waitable)
        };
        Ok(Self {
            device: device.clone(),
            context: device.immediate_context().map_err(|e| e.to_string())?,
            chain,
            waitable,
            size,
            target: None,
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

    /// Keeps a GPU copy of `frame` as the picture to draw (the decoder's
    /// surface is free once `frame` is dropped).


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
        // SAFETY: valid swap chain; sync interval 1.
        let result = unsafe { self.chain.Present(1, DXGI_PRESENT(0)) };
        if result == DXGI_ERROR_DEVICE_REMOVED || result == DXGI_ERROR_DEVICE_RESET {
            return Err(format!("GPU device lost ({result:?})"));
        }
        result.ok().map_err(|e| format!("Present: {e}"))
    }

}
