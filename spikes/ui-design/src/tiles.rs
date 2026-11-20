//! One texture per screen, drawn by Dear ImGui like any image (rounded
//! corners, scaled into cards and tiles).
//!
//! Each decoded picture is copied on the GPU into an NV12 texture of our own
//! (the decoder surface goes back at once), then converted and scaled by the
//! video processor into a BGRA texture at the size the UI asked for last
//! frame (sizes lag one frame, so a texture referenced by this frame's draw
//! data is never released before it is drawn).

use decode::DecodedFrame;
use gpu::{D3d11Device, FrameFormat};
use present::FrameRenderer;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;

struct Output {
    texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    size: (u32, u32),
}

#[derive(Default)]
struct Tile {
    picture: Option<(ID3D11Texture2D, (u32, u32))>,
    output: Option<Output>,
    renderer: Option<FrameRenderer>,
    /// Largest size asked for during the current frame.
    requested: (u32, u32),
    dirty: bool,
}

pub struct Tiles {
    device: D3d11Device,
    context: ID3D11DeviceContext,
    tiles: Vec<Tile>,
    /// Pictures converted (video processor runs), for the summary.
    pub conversions: u64,
}

impl Tiles {
    pub fn new(device: &D3d11Device, count: usize) -> Result<Self, String> {
        Ok(Self {
            device: device.clone(),
            context: device.immediate_context().map_err(|e| e.to_string())?,
            tiles: (0..count).map(|_| Tile::default()).collect(),
            conversions: 0,
        })
    }

    /// Picture `index`'s size, once one arrived.
    pub fn picture_size(&self, index: usize) -> Option<(u32, u32)> {
        self.tiles[index].picture.as_ref().map(|(_, size)| *size)
    }

    pub fn set_frame(&mut self, index: usize, frame: &DecodedFrame) -> Result<(), String> {
        let size = (frame.width, frame.height);
        let tile = &mut self.tiles[index];
        if tile.picture.as_ref().is_none_or(|(_, current)| *current != size) {
            let texture = create(&self.device, size, FrameFormat::Nv12.dxgi_format(), true)?;
            tile.picture = Some((texture, size));
            tile.renderer = None;
        }
        let (texture, _) = tile.picture.as_ref().expect("created above");
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; live out-pointer.
        unsafe { frame.texture.GetDesc(&mut desc) };
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: frame.width, bottom: frame.height, back: 1 };
        // SAFETY: same device and format (NV12), the box lies inside both.
        unsafe {
            self.context.CopySubresourceRegion(texture, 0, 0, 0, 0, &frame.texture, frame.slice * desc.MipLevels, Some(&region));
        }
        tile.dirty = true;
        Ok(())
    }

    /// Before the UI is built: (re)creates outputs at the sizes asked for
    /// last frame. True if any size changed (draw another frame).
    pub fn begin_frame(&mut self) -> Result<bool, String> {
        let mut changed = false;
        for tile in &mut self.tiles {
            let wanted = tile.requested;
            tile.requested = (0, 0);
            if wanted.0 < 2 || wanted.1 < 2 || tile.output.as_ref().is_some_and(|o| o.size == wanted) {
                continue;
            }
            let texture = create(&self.device, wanted, DXGI_FORMAT_B8G8R8A8_UNORM, false)?;
            let mut view = None;
            // SAFETY: valid device and texture; live out-pointer.
            unsafe { self.device.raw().CreateShaderResourceView(&texture, None, Some(&mut view)) }.map_err(|e| e.to_string())?;
            tile.output = Some(Output { texture, view: view.expect("view created"), size: wanted });
            tile.renderer = None;
            tile.dirty = true;
            changed = true;
        }
        Ok(changed)
    }

    /// During the UI build: the texture to draw for `index` (Dear ImGui's
    /// DX11 backend takes a shader resource view pointer), and records the
    /// size wanted (in pixels) for the next frames.
    pub fn texture(&mut self, index: usize, wanted: (u32, u32)) -> Option<u64> {
        let tile = &mut self.tiles[index];
        tile.requested = (tile.requested.0.max(wanted.0), tile.requested.1.max(wanted.1));
        tile.picture.as_ref()?;
        tile.output.as_ref().map(|o| o.view.as_raw() as u64)
    }

    /// After the UI build, before Dear ImGui renders: converts new pictures.
    pub fn render(&mut self) -> Result<(), String> {
        for tile in &mut self.tiles {
            let (Some((picture, size)), Some(output), true) = (&tile.picture, &tile.output, tile.dirty) else { continue };
            if tile.renderer.is_none() {
                tile.renderer = Some(FrameRenderer::new(&self.device).map_err(|e| e.to_string())?);
            }
            tile.renderer
                .as_mut()
                .expect("created above")
                .render(picture, *size, &output.texture, output.size)
                .map_err(|e| format!("video: {e}"))?;
            tile.dirty = false;
            self.conversions += 1;
        }
        Ok(())
    }
}

fn create(device: &D3d11Device, (width, height): (u32, u32), format: DXGI_FORMAT, nv12: bool) -> Result<ID3D11Texture2D, String> {
    let bind = if nv12 {
        D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0
    } else {
        D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: valid device; descriptor and out-pointer are live locals.
    unsafe { device.raw().CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(|e| e.to_string())?;
    Ok(texture.expect("texture created"))
}
