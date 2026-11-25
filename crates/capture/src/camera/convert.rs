//! A camera frame from CPU memory to a BGRA texture: one copy into dynamic
//! textures laid out like the camera's bytes, then one draw that turns them
//! into BGRA on the GPU. The only place a camera's pixels touch the CPU
//! (the exception agreed for cameras: they hand their frames over in
//! memory).

use gpu::{CompiledShader, D3d11Device, compile_shader};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
    D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT, ID3D11Buffer, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::core::{Error, Result};

use super::PixelFormat;

const SHADER_SOURCE: &str = include_str!("camera.hlsl");

/// A frame as the camera wrote it: the first row starts at `first_row` in
/// `data`, and each next row `pitch` bytes after the previous one (negative
/// for an image stored bottom-up).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawFrame<'a> {
    pub data: &'a [u8],
    pub first_row: usize,
    pub pitch: isize,
}

impl RawFrame<'_> {
    /// Row `index` (counted from the top), `bytes` long, if it lies inside
    /// `data`.
    fn row(&self, index: usize, bytes: usize) -> Option<&[u8]> {
        let start =
            (self.first_row as isize).checked_add(self.pitch.checked_mul(index as isize)?)?;
        let start = usize::try_from(start).ok()?;
        self.data.get(start..start.checked_add(bytes)?)
    }
}

/// One texture the frame's bytes are copied into.
struct Plane {
    texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    /// Bytes of one row, and rows, of this plane.
    row_bytes: usize,
    rows: usize,
    /// The plane's first row, in rows of the whole frame.
    first_row: usize,
}

/// Constant buffer layout; must match `Colors` in camera.hlsl.
#[repr(C)]
struct Colors {
    bt709: u32,
    padding: [u32; 3],
}

pub(crate) struct Converter {
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    planes: Vec<Plane>,
    target: ID3D11Texture2D,
    target_view: ID3D11RenderTargetView,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    colors: ID3D11Buffer,
}

impl Converter {
    /// For `width` x `height` frames in `format` (never MJPEG: Media
    /// Foundation decodes that to YUY2 first). `bt709`: the camera says its
    /// YCbCr uses the BT.709 matrix rather than BT.601.
    pub fn new(
        device: &D3d11Device,
        format: PixelFormat,
        width: u32,
        height: u32,
        bt709: bool,
    ) -> Result<Self> {
        let invalid = |what: &str| Error::new(E_INVALIDARG, format!("camera frame: {what}"));
        let even =
            width.is_multiple_of(2) && (format != PixelFormat::Nv12 || height.is_multiple_of(2));
        if width == 0 || height == 0 || !even {
            return Err(invalid(&format!("{format:?} at {width}x{height}")));
        }
        let (w, h) = (width as usize, height as usize);
        // Each plane: its texture format, size in texels, bytes per row, and
        // where it starts.
        let layout: Vec<(DXGI_FORMAT, u32, u32, usize, usize)> = match format {
            PixelFormat::Yuy2 => vec![(DXGI_FORMAT_R8G8B8A8_UNORM, width / 2, height, w * 2, 0)],
            PixelFormat::Nv12 => vec![
                (DXGI_FORMAT_R8_UNORM, width, height, w, 0),
                (DXGI_FORMAT_R8G8_UNORM, width / 2, height / 2, w, h),
            ],
            PixelFormat::Rgb32 => vec![(DXGI_FORMAT_B8G8R8A8_UNORM, width, height, w * 4, 0)],
            PixelFormat::Rgb24 => vec![(DXGI_FORMAT_R8_UNORM, width * 3, height, w * 3, 0)],
            PixelFormat::Mjpeg => return Err(invalid("MJPEG must be decoded first")),
        };
        let planes = layout
            .into_iter()
            .map(|(dxgi, tw, th, row_bytes, first_row)| {
                let (texture, view) = dynamic_texture(device, dxgi, tw, th)?;
                Ok(Plane {
                    texture,
                    view,
                    row_bytes,
                    rows: th as usize,
                    first_row,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let raw = device.raw();
        let target_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
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
        let mut target = None;
        // SAFETY: valid device; descriptor and out-pointer reference live locals.
        unsafe { raw.CreateTexture2D(&target_desc, None, Some(&mut target)) }?;
        let target: ID3D11Texture2D = target.expect("CreateTexture2D succeeded");
        let mut target_view = None;
        // SAFETY: the texture was created with the render-target bind flag.
        unsafe { raw.CreateRenderTargetView(&target, None, Some(&mut target_view)) }?;

        let entry = match format {
            PixelFormat::Yuy2 => "ps_yuy2",
            PixelFormat::Nv12 => "ps_nv12",
            PixelFormat::Rgb32 => "ps_rgb32",
            PixelFormat::Rgb24 | PixelFormat::Mjpeg => "ps_rgb24",
        };
        let compile = |entry: &str, target: &str| -> Result<CompiledShader> {
            compile_shader(SHADER_SOURCE, "camera.hlsl", entry, target)
                .map_err(|error| Error::new(E_INVALIDARG, error.to_string()))
        };
        let (vs_code, ps_code) = (compile("vs_main", "vs_5_0")?, compile(entry, "ps_5_0")?);
        let (mut vertex_shader, mut pixel_shader) = (None, None);
        // SAFETY: valid device; the bytecode slices live for the calls.
        unsafe {
            raw.CreateVertexShader(vs_code.bytecode(), None, Some(&mut vertex_shader))?;
            raw.CreatePixelShader(ps_code.bytecode(), None, Some(&mut pixel_shader))?;
        }

        let colors = Colors {
            bt709: u32::from(bt709),
            padding: [0; 3],
        };
        let buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<Colors>() as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: (&colors as *const Colors).cast(),
            ..Default::default()
        };
        let mut buffer = None;
        // SAFETY: valid device; `initial` points at a live `Colors` of
        // exactly `ByteWidth` bytes, copied during the call.
        unsafe { raw.CreateBuffer(&buffer_desc, Some(&initial), Some(&mut buffer)) }?;

        Ok(Self {
            context: device.immediate_context()?,
            width,
            height,
            planes,
            target,
            target_view: target_view.expect("CreateRenderTargetView succeeded"),
            vertex_shader: vertex_shader.expect("CreateVertexShader succeeded"),
            pixel_shader: pixel_shader.expect("CreatePixelShader succeeded"),
            colors: buffer.expect("CreateBuffer succeeded"),
        })
    }

    /// The BGRA result of the last [`Self::convert`].
    pub fn target(&self) -> &ID3D11Texture2D {
        &self.target
    }

    /// Copies `frame` to the GPU and converts it into [`Self::target`].
    /// Fails, touching nothing on the GPU, if `frame` is too small.
    pub fn convert(&mut self, frame: &RawFrame<'_>) -> Result<()> {
        // Every row must be there before anything is written.
        for plane in &self.planes {
            let last = plane.first_row + plane.rows - 1;
            if frame.row(plane.first_row, plane.row_bytes).is_none()
                || frame.row(last, plane.row_bytes).is_none()
            {
                return Err(Error::new(
                    E_INVALIDARG,
                    format!(
                        "camera frame too small: {} bytes, pitch {}",
                        frame.data.len(),
                        frame.pitch
                    ),
                ));
            }
        }
        for plane in &self.planes {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: a dynamic texture of this device with CPU write access;
            // the out-pointer references a live local.
            unsafe {
                self.context.Map(
                    &plane.texture,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
            }?;
            for row in 0..plane.rows {
                let source = frame
                    .row(plane.first_row + row, plane.row_bytes)
                    .expect("rows checked above (rows in between lie between the first and last)");
                // SAFETY: the mapped texture has `rows` rows of `RowPitch`
                // bytes, each at least `row_bytes` long (the texture's width
                // in texels times its texel size is exactly `row_bytes`);
                // the source row is `row_bytes` long. They do not overlap.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        source.as_ptr(),
                        mapped
                            .pData
                            .cast::<u8>()
                            .add(row * mapped.RowPitch as usize),
                        plane.row_bytes,
                    );
                }
            }
            // SAFETY: balances the successful Map above.
            unsafe { self.context.Unmap(&plane.texture, 0) };
        }

        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.width as f32,
            Height: self.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let views: Vec<Option<ID3D11ShaderResourceView>> =
            self.planes.iter().map(|p| Some(p.view.clone())).collect();
        let context = &self.context;
        // SAFETY: every object belongs to this device and outlives the calls.
        // The planes and target are unbound afterwards, so other stages (the
        // copy into the capture pool) can use the target.
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(self.target_view.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(None);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.colors.clone())]));
            context.PSSetShaderResources(0, Some(&views));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&vec![None; views.len()]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

/// A texture the CPU writes each frame and shaders read.
fn dynamic_texture(
    device: &D3d11Device,
    format: DXGI_FORMAT,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: valid device; descriptor and out-pointer reference live locals.
    unsafe {
        device
            .raw()
            .CreateTexture2D(&desc, None, Some(&mut texture))
    }?;
    let texture: ID3D11Texture2D = texture.expect("CreateTexture2D succeeded");
    let mut view = None;
    // SAFETY: the texture was created with the shader-resource bind flag.
    unsafe {
        device
            .raw()
            .CreateShaderResourceView(&texture, None, Some(&mut view))
    }?;
    Ok((texture, view.expect("CreateShaderResourceView succeeded")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpu::{DebugLayer, create_device, enumerate_adapters, read_pixel};

    #[test]
    fn shaders_compile() {
        for entry in ["ps_yuy2", "ps_nv12", "ps_rgb32", "ps_rgb24"] {
            compile_shader(SHADER_SOURCE, "camera.hlsl", entry, "ps_5_0").unwrap();
        }
        compile_shader(SHADER_SOURCE, "camera.hlsl", "vs_main", "vs_5_0").unwrap();
    }

    #[test]
    fn colors_are_a_multiple_of_16_bytes() {
        assert_eq!(size_of::<Colors>() % 16, 0);
    }

    #[test]
    fn rows_outside_the_data_are_refused() {
        let data = [0_u8; 40];
        let top_down = RawFrame {
            data: &data,
            first_row: 0,
            pitch: 10,
        };
        assert_eq!(top_down.row(3, 10).map(|r| r.len()), Some(10));
        assert!(top_down.row(4, 10).is_none());
        assert!(top_down.row(3, 11).is_none());
        let bottom_up = RawFrame {
            data: &data,
            first_row: 30,
            pitch: -10,
        };
        assert!(bottom_up.row(3, 10).is_some());
        assert!(bottom_up.row(4, 10).is_none());
    }

    fn hardware_device() -> D3d11Device {
        let adapters = enumerate_adapters().unwrap();
        let adapter = adapters
            .iter()
            .find(|adapter| !adapter.info.is_software)
            .expect("hardware adapter");
        create_device(adapter, DebugLayer::IfAvailable).unwrap()
    }

    /// Converts `bytes` and reads pixels back (`[x, y]` → BGRA).
    fn convert(
        format: PixelFormat,
        width: u32,
        height: u32,
        frame: RawFrame<'_>,
        at: &[(u32, u32)],
    ) -> Vec<[u8; 4]> {
        let device = hardware_device();
        let mut converter = Converter::new(&device, format, width, height, false).unwrap();
        converter.convert(&frame).unwrap();
        let pixels = at
            .iter()
            .map(|&(x, y)| read_pixel(&device, converter.target(), x, y).unwrap())
            .collect();
        let problems: Vec<_> = device
            .take_debug_messages()
            .into_iter()
            .filter(|m| m.severity.is_problem())
            .collect();
        assert!(problems.is_empty(), "{problems:#?}");
        pixels
    }

    fn close(actual: [u8; 4], expected: [u8; 4]) -> bool {
        actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 2)
    }

    // Limited-range BT.601 primaries (Y, Cb, Cr) and their BGRA.
    const WHITE: ([u8; 3], [u8; 4]) = ([235, 128, 128], [255, 255, 255, 255]);
    const BLACK: ([u8; 3], [u8; 4]) = ([16, 128, 128], [0, 0, 0, 255]);
    const RED: ([u8; 3], [u8; 4]) = ([81, 90, 240], [0, 0, 255, 255]);
    const BLUE: ([u8; 3], [u8; 4]) = ([41, 240, 110], [255, 0, 0, 255]);

    #[test]
    #[ignore = "requires a GPU"]
    fn yuy2_turns_into_the_right_colors() {
        // 4x2 with padding in each row (pitch 12 for 8 bytes): white and
        // black share a chroma pair on top, red and blue fill the bottom.
        let pair = |a: [u8; 3], b: [u8; 3]| [a[0], a[1], b[0], a[2]];
        let mut data = Vec::new();
        data.extend(pair(WHITE.0, BLACK.0));
        data.extend(pair(WHITE.0, BLACK.0));
        data.extend([0xEE; 4]);
        data.extend(pair(RED.0, RED.0));
        data.extend(pair(BLUE.0, BLUE.0));
        data.extend([0xEE; 4]);
        let frame = RawFrame {
            data: &data,
            first_row: 0,
            pitch: 12,
        };
        let pixels = convert(
            PixelFormat::Yuy2,
            4,
            2,
            frame,
            &[(0, 0), (1, 0), (0, 1), (3, 1)],
        );
        for (actual, expected) in pixels.into_iter().zip([WHITE.1, BLACK.1, RED.1, BLUE.1]) {
            assert!(close(actual, expected), "{actual:?} vs {expected:?}");
        }
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn nv12_turns_into_the_right_colors() {
        // 4x2: left half red, right half blue (one chroma sample each).
        let (r, b) = (RED.0, BLUE.0);
        let data = [
            r[0], r[0], b[0], b[0], // luma, row 0
            r[0], r[0], b[0], b[0], // luma, row 1
            r[1], r[2], b[1], b[2], // chroma
        ];
        let frame = RawFrame {
            data: &data,
            first_row: 0,
            pitch: 4,
        };
        let pixels = convert(PixelFormat::Nv12, 4, 2, frame, &[(1, 1), (2, 0)]);
        assert!(close(pixels[0], RED.1), "{:?}", pixels[0]);
        assert!(close(pixels[1], BLUE.1), "{:?}", pixels[1]);
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn rgb_formats_keep_their_colors_and_bottom_up_is_flipped() {
        // RGB24, 2x2, stored bottom-up: the last row in memory is the top.
        let data = [
            0, 0, 255, 0, 255, 0, // bottom row: red, green
            255, 0, 0, 255, 255, 255, // top row: blue, white
        ];
        let frame = RawFrame {
            data: &data,
            first_row: 6,
            pitch: -6,
        };
        let pixels = convert(
            PixelFormat::Rgb24,
            2,
            2,
            frame,
            &[(0, 0), (1, 0), (0, 1), (1, 1)],
        );
        assert_eq!(
            pixels,
            [
                [255, 0, 0, 255],
                [255, 255, 255, 255],
                [0, 0, 255, 255],
                [0, 255, 0, 255]
            ]
        );
        // RGB32: the unused byte (here 7) becomes an opaque alpha.
        let data = [10, 20, 30, 7, 40, 50, 60, 7];
        let frame = RawFrame {
            data: &data,
            first_row: 0,
            pitch: 8,
        };
        let pixels = convert(PixelFormat::Rgb32, 2, 1, frame, &[(0, 0), (1, 0)]);
        assert_eq!(pixels, [[10, 20, 30, 255], [40, 50, 60, 255]]);
    }

    #[test]
    #[ignore = "requires a GPU"]
    fn a_short_frame_is_refused() {
        let device = hardware_device();
        let mut converter = Converter::new(&device, PixelFormat::Yuy2, 4, 2, false).unwrap();
        let data = [0_u8; 15];
        let frame = RawFrame {
            data: &data,
            first_row: 0,
            pitch: 8,
        };
        assert!(converter.convert(&frame).is_err());
        assert!(Converter::new(&device, PixelFormat::Yuy2, 3, 2, false).is_err());
        assert!(Converter::new(&device, PixelFormat::Mjpeg, 4, 2, false).is_err());
    }
}
