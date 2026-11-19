//! The window's swap chain and the video drawn into it.
//!
//! Video: the decoded NV12 slice is copied on the GPU into a texture of our
//! own (as `VideoPresenter` does), so the decoder surface goes back at once,
//! then drawn with the same `FrameRenderer` (NV12 -> RGB, scaled,
//! letterboxed) straight into the back buffer. Dear ImGui draws on top.

use decode::DecodedFrame;
use gpu::{D3d11Device, FrameFormat};
use present::FrameRenderer;
use windows::Win32::Foundation::{HANDLE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
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

/// The latest video picture, in a texture of our own.
struct Video {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

pub struct Gfx {
    device: D3d11Device,
    context: ID3D11DeviceContext,
    chain: IDXGISwapChain2,
    waitable: HANDLE,
    pub size: (u32, u32),
    target: Option<ID3D11RenderTargetView>,
    renderer: FrameRenderer,
    video: Option<Video>,
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
            renderer: FrameRenderer::new(device).map_err(|e| e.to_string())?,
            video: None,
        })
    }

    pub fn resize(&mut self, size: (u32, u32)) -> Result<(), String> {
        if size == self.size || size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        self.target = None;
        self.renderer.release_views();
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
    pub fn set_video(&mut self, frame: &DecodedFrame) -> Result<(), String> {
        if self
            .video
            .as_ref()
            .is_none_or(|video| (video.width, video.height) != (frame.width, frame.height))
        {
            self.video = Some(Video {
                texture: self.create_nv12(frame.width, frame.height)?,
                width: frame.width,
                height: frame.height,
            });
            self.renderer.release_views();
        }
        let video = self.video.as_ref().expect("created above");
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; live out-pointer.
        unsafe { frame.texture.GetDesc(&mut desc) };
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: frame.width,
            bottom: frame.height,
            back: 1,
        };
        // SAFETY: same device and format (NV12); the even-sized box lies in
        // both textures. Queued on the GPU after the decode.
        unsafe {
            self.context.CopySubresourceRegion(
                &video.texture,
                0,
                0,
                0,
                0,
                &frame.texture,
                frame.slice * desc.MipLevels,
                Some(&region),
            );
        }
        Ok(())
    }

    pub fn clear_video(&mut self) {
        self.video = None;
        self.renderer.release_views();
    }

    /// Waits (bounded) until the swap chain can take a frame.
    pub fn wait_ready(&self) {
        // SAFETY: valid waitable handle.
        let _ = unsafe { WaitForSingleObject(self.waitable, 100) } == WAIT_OBJECT_0;
    }

    /// Clears the back buffer, draws the video if any, and binds the back
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
        if let Some(video) = &self.video {
            self.renderer
                .render(
                    &video.texture,
                    (video.width, video.height),
                    &back,
                    self.size,
                )
                .map_err(|e| format!("video: {e}"))?;
        }
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

    fn create_nv12(&self, width: u32, height: u32) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: FrameFormat::Nv12.dxgi_format(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: valid device; descriptor and out-pointer are live locals.
        unsafe {
            self.device
                .raw()
                .CreateTexture2D(&desc, None, Some(&mut texture))
        }
        .map_err(|e| e.to_string())?;
        Ok(texture.expect("texture created"))
    }
}
