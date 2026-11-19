//! Synthetic frame source: renders a deterministic test pattern on the GPU
//! with the frame id and submit timestamp drawn as a visual frame stamp
//! (see `telemetry::frame_code`). Stands in for real capture in tests and
//! benchmarks.

mod pacer;
mod readback;

use std::fmt;

use gpu::{CompiledShader, D3d11Device, ShaderError, compile_shader};
use telemetry::frame_code::{self, FrameCodeError, FrameStamp};
use telemetry::qpc_now;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BUFFER_DESC, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11Buffer,
    ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11Texture2D,
    ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

pub use pacer::{Pacer, Slot};

const SHADER_SOURCE: &str = include_str!("pattern.hlsl");
/// Textures rendered in rotation; a frame stays intact for this many renders.
pub const RING_SIZE: usize = 3;
/// The motion pattern needs room for its moving squares and the frame code.
pub const MIN_WIDTH: u32 = frame_code::CODE_WIDTH;
pub const MIN_HEIGHT: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternMode {
    /// Fixed background, like a desktop; only the frame stamp changes.
    Static,
    /// Scrolling bands, moving squares and per-pixel noise, like a game.
    Motion,
}

impl PatternMode {
    pub fn name(self) -> &'static str {
        match self {
            PatternMode::Static => "static",
            PatternMode::Motion => "motion",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntheticConfig {
    pub width: u32,
    pub height: u32,
    pub mode: PatternMode,
}

#[derive(Debug)]
pub enum SyntheticError {
    FrameTooSmall { width: u32, height: u32 },
    Shader(ShaderError),
    FrameCode(FrameCodeError),
    Windows(windows::core::Error),
    Io(std::io::Error),
}

impl fmt::Display for SyntheticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyntheticError::FrameTooSmall { width, height } => write!(
                f,
                "synthetic frames must be at least {MIN_WIDTH}x{MIN_HEIGHT}, got {width}x{height}"
            ),
            SyntheticError::Shader(error) => error.fmt(f),
            SyntheticError::FrameCode(error) => error.fmt(f),
            SyntheticError::Windows(error) => error.fmt(f),
            SyntheticError::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SyntheticError {}

impl From<ShaderError> for SyntheticError {
    fn from(error: ShaderError) -> Self {
        SyntheticError::Shader(error)
    }
}

impl From<FrameCodeError> for SyntheticError {
    fn from(error: FrameCodeError) -> Self {
        SyntheticError::FrameCode(error)
    }
}

impl From<windows::core::Error> for SyntheticError {
    fn from(error: windows::core::Error) -> Self {
        SyntheticError::Windows(error)
    }
}

impl From<std::io::Error> for SyntheticError {
    fn from(error: std::io::Error) -> Self {
        SyntheticError::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, SyntheticError>;

/// A rendered frame. The texture belongs to the source's ring and is
/// overwritten `RING_SIZE` renders later.
#[derive(Clone, Debug)]
pub struct SyntheticFrame {
    pub texture: ID3D11Texture2D,
    pub stamp: FrameStamp,
}

/// Constant buffer layout; must match `FrameConstants` in pattern.hlsl.
#[repr(C)]
#[derive(Clone, Copy)]
struct FrameConstants {
    code_words: [u32; 4],
    frame_id: u32,
    mode: u32,
    width: u32,
    height: u32,
}

struct RenderTarget {
    texture: ID3D11Texture2D,
    view: ID3D11RenderTargetView,
}

/// Renders synthetic frames on one device. Owns the immediate context while
/// it exists: it must be used from one thread at a time.
pub struct SyntheticSource {
    device: D3d11Device,
    context: ID3D11DeviceContext,
    config: SyntheticConfig,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    constants: ID3D11Buffer,
    targets: Vec<RenderTarget>,
    next_frame_id: u32,
}

impl SyntheticSource {
    pub fn new(device: &D3d11Device, config: SyntheticConfig) -> Result<Self> {
        if config.width < MIN_WIDTH || config.height < MIN_HEIGHT {
            return Err(SyntheticError::FrameTooSmall {
                width: config.width,
                height: config.height,
            });
        }
        let raw = device.raw();

        let vs_code = compile_shader(SHADER_SOURCE, "pattern.hlsl", "vs_main", "vs_5_0")?;
        let ps_code = compile_shader(SHADER_SOURCE, "pattern.hlsl", "ps_main", "ps_5_0")?;
        let vertex_shader = create_vertex_shader(device, &vs_code)?;
        let pixel_shader = create_pixel_shader(device, &ps_code)?;

        let buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<FrameConstants>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let mut constants = None;
        // SAFETY: valid device; the descriptor and out-pointer reference live locals.
        unsafe { raw.CreateBuffer(&buffer_desc, None, Some(&mut constants)) }?;

        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: config.width,
            Height: config.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let targets = (0..RING_SIZE)
            .map(|_| create_render_target(device, &texture_desc))
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            device: device.clone(),
            context: device.immediate_context()?,
            config,
            vertex_shader,
            pixel_shader,
            constants: constants.expect("CreateBuffer succeeded"),
            targets,
            next_frame_id: 0,
        })
    }

    pub fn config(&self) -> SyntheticConfig {
        self.config
    }

    pub fn device(&self) -> &D3d11Device {
        &self.device
    }

    /// Renders the next frame and submits it to the GPU (flushes).
    /// The stamp's timestamp is taken right before the draw is recorded.
    pub fn render_next(&mut self) -> Result<SyntheticFrame> {
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        let target = &self.targets[frame_id as usize % RING_SIZE];

        let stamp = FrameStamp {
            frame_id,
            timestamp: qpc_now(),
        };
        let constants = FrameConstants {
            code_words: stamp.to_words(),
            frame_id,
            mode: match self.config.mode {
                PatternMode::Static => 0,
                PatternMode::Motion => 1,
            },
            width: self.config.width,
            height: self.config.height,
        };
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.config.width as f32,
            Height: self.config.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        let context = &self.context;
        // SAFETY: all objects belong to `self.device` and outlive the calls;
        // `constants` is a live, correctly laid out FrameConstants whose size
        // matches the buffer. The render target is unbound afterwards so the
        // texture can be read by other pipeline stages.
        unsafe {
            context.UpdateSubresource(
                &self.constants,
                0,
                None,
                (&constants as *const FrameConstants).cast(),
                0,
                0,
            );
            context.OMSetRenderTargets(Some(&[Some(target.view.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(None);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.Draw(3, 0);
            context.OMSetRenderTargets(None, None);
            context.Flush();
        }

        Ok(SyntheticFrame {
            texture: target.texture.clone(),
            stamp,
        })
    }

    /// Test/validation only: copies the frame-code region (1024x32 px) to the
    /// CPU, waits for the GPU and decodes the stamp. Stalls the pipeline.
    pub fn read_stamp(&self, frame: &SyntheticFrame) -> Result<FrameStamp> {
        readback::read_stamp(&self.device, &self.context, &frame.texture)
    }

    /// Diagnostics only: writes the whole frame as a 32-bit BMP. Reads the
    /// full frame back to the CPU, so it must never run in the media path.
    pub fn dump_bmp(&self, frame: &SyntheticFrame, path: &std::path::Path) -> Result<()> {
        readback::dump_bmp(&self.device, &self.context, &frame.texture, path)
    }
}

/// Test/validation only: reads the frame stamp from the top-left 1024x32
/// region of any BGRA8 texture of `device` (e.g. a capture of a window
/// showing synthetic frames). Copies only that region; stalls the GPU.
pub fn read_bgra_stamp(device: &D3d11Device, texture: &ID3D11Texture2D) -> Result<FrameStamp> {
    readback::read_stamp(device, &device.immediate_context()?, texture)
}
fn create_vertex_shader(device: &D3d11Device, code: &CompiledShader) -> Result<ID3D11VertexShader> {
    let mut shader = None;
    // SAFETY: valid device; the bytecode slice is live for the call.
    unsafe {
        device
            .raw()
            .CreateVertexShader(code.bytecode(), None, Some(&mut shader))
    }?;
    Ok(shader.expect("CreateVertexShader succeeded"))
}

fn create_pixel_shader(device: &D3d11Device, code: &CompiledShader) -> Result<ID3D11PixelShader> {
    let mut shader = None;
    // SAFETY: valid device; the bytecode slice is live for the call.
    unsafe {
        device
            .raw()
            .CreatePixelShader(code.bytecode(), None, Some(&mut shader))
    }?;
    Ok(shader.expect("CreatePixelShader succeeded"))
}

fn create_render_target(device: &D3d11Device, desc: &D3D11_TEXTURE2D_DESC) -> Result<RenderTarget> {
    let mut texture = None;
    // SAFETY: valid device; descriptor and out-pointer reference live locals.
    unsafe { device.raw().CreateTexture2D(desc, None, Some(&mut texture)) }?;
    let texture: ID3D11Texture2D = texture.expect("CreateTexture2D succeeded");
    let mut view = None;
    // SAFETY: the texture was created with the render-target bind flag.
    unsafe {
        device
            .raw()
            .CreateRenderTargetView(&texture, None, Some(&mut view))
    }?;
    Ok(RenderTarget {
        texture,
        view: view.expect("CreateRenderTargetView succeeded"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpu::{DebugLayer, create_device, enumerate_adapters};
    use telemetry::QpcTimestamp;

    #[test]
    fn shaders_compile() {
        compile_shader(SHADER_SOURCE, "pattern.hlsl", "vs_main", "vs_5_0").unwrap();
        compile_shader(SHADER_SOURCE, "pattern.hlsl", "ps_main", "ps_5_0").unwrap();
    }

    #[test]
    fn constant_buffer_size_is_a_multiple_of_16() {
        assert_eq!(size_of::<FrameConstants>() % 16, 0);
    }

    fn hardware_device() -> D3d11Device {
        let adapters = enumerate_adapters().unwrap();
        let adapter = adapters
            .iter()
            .find(|adapter| !adapter.info.is_software)
            .expect("hardware adapter");
        create_device(adapter, DebugLayer::IfAvailable).unwrap()
    }

    fn assert_no_debug_problems(device: &D3d11Device) {
        let problems: Vec<_> = device
            .take_debug_messages()
            .into_iter()
            .filter(|message| message.severity.is_problem())
            .collect();
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn too_small_frames_are_rejected() {
        let device = hardware_device();
        let config = SyntheticConfig {
            width: 800,
            height: 600,
            mode: PatternMode::Static,
        };
        assert!(matches!(
            SyntheticSource::new(&device, config),
            Err(SyntheticError::FrameTooSmall { .. })
        ));
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn stamps_read_back_in_both_modes() {
        let device = hardware_device();
        for mode in [PatternMode::Static, PatternMode::Motion] {
            let config = SyntheticConfig {
                width: 1920,
                height: 1080,
                mode,
            };
            let mut source = SyntheticSource::new(&device, config).unwrap();
            let mut previous: Option<FrameStamp> = None;
            for expected_id in 0..120 {
                let frame = source.render_next().unwrap();
                let read = source.read_stamp(&frame).unwrap();
                assert_eq!(read, frame.stamp, "{mode:?}");
                assert_eq!(read.frame_id, expected_id, "{mode:?}");
                if let Some(previous) = previous {
                    assert!(read.timestamp > previous.timestamp, "{mode:?}");
                }
                previous = Some(read);
            }
        }
        assert_no_debug_problems(&device);
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn paces_at_60_fps() {
        let device = hardware_device();
        let config = SyntheticConfig {
            width: 1920,
            height: 1080,
            mode: PatternMode::Motion,
        };
        let mut source = SyntheticSource::new(&device, config).unwrap();
        let mut pacer = Pacer::start_now(60);
        let mut skipped = 0;
        let mut first: Option<QpcTimestamp> = None;
        let mut last = None;
        for _ in 0..120 {
            let slot = pacer.next_slot(qpc_now());
            skipped += slot.skipped;
            pacer.wait(&slot);
            let frame = source.render_next().unwrap();
            first.get_or_insert(frame.stamp.timestamp);
            last = Some(frame.stamp.timestamp);
        }
        let elapsed = last
            .unwrap()
            .duration_since(first.unwrap())
            .as_micros(telemetry::qpc_frequency());
        // 119 intervals of 16 667 us; allow 2% for scheduling noise.
        let expected = 119.0 * 1_000_000.0 / 60.0;
        assert!(
            (elapsed - expected).abs() / expected < 0.02,
            "elapsed {elapsed} us, expected {expected} us"
        );
        assert!(skipped <= 2, "skipped {skipped} slots");
        assert_no_debug_problems(&device);
    }
}
