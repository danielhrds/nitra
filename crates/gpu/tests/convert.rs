//! BGRA8 -> NV12 converters on a real GPU. Ignored by default; run with
//! `scripts/gpu-tests.ps1`.

use gpu::convert::{
    ConvertConfig, Converter, ShaderConverter, VideoProcessorConverter, bt709_limited,
    read_nv12_samples,
};
use gpu::{D3d11Device, DebugLayer};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

const WIDTH: u32 = 256;
const HEIGHT: u32 = 128;
/// Eight vertical bars of 32 px, RGB.
const BARS: [[u8; 3]; 8] = [
    [0, 0, 0],
    [255, 255, 255],
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [128, 128, 128],
    [255, 255, 0],
    [0, 255, 255],
];

fn hardware_device() -> D3d11Device {
    let adapters = gpu::enumerate_adapters().unwrap();
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    gpu::create_device(adapter, DebugLayer::IfAvailable).unwrap()
}

/// Uploads the bar pattern (test-only CPU -> GPU upload).
fn bar_texture(device: &D3d11Device) -> ID3D11Texture2D {
    let bar_width = WIDTH / BARS.len() as u32;
    let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for _ in 0..HEIGHT {
        for x in 0..WIDTH {
            let [r, g, b] = BARS[(x / bar_width) as usize];
            pixels.extend([b, g, r, 255]);
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: WIDTH,
        Height: HEIGHT,
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
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: WIDTH * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: `pixels` holds WIDTH x HEIGHT BGRA pixels and outlives the call.
    unsafe {
        device
            .raw()
            .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
    }
    .unwrap();
    texture.unwrap()
}

fn converters(
    device: &D3d11Device,
    output_width: u32,
    output_height: u32,
) -> Vec<Box<dyn Converter>> {
    let config = ConvertConfig {
        input_width: WIDTH,
        input_height: HEIGHT,
        output_width,
        output_height,
        pool_size: 3,
    };
    vec![
        Box::new(VideoProcessorConverter::new(device, config).unwrap()),
        Box::new(ShaderConverter::new(device, config).unwrap()),
    ]
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
fn both_methods_match_the_bt709_reference() {
    let device = hardware_device();
    let source = bar_texture(&device);
    for (output_width, output_height, tolerance) in [(WIDTH, HEIGHT, 2), (WIDTH / 2, HEIGHT / 2, 3)]
    {
        for mut converter in converters(&device, output_width, output_height) {
            let output = converter
                .convert(&source)
                .unwrap()
                .expect("a free output texture");
            let bar_width = output_width / BARS.len() as u32;
            let points: Vec<(u32, u32)> = (0..BARS.len() as u32)
                .map(|bar| (bar * bar_width + bar_width / 2, output_height / 2))
                .collect();
            let samples = read_nv12_samples(&device, &output, &points).unwrap();
            for (bar, (sample, rgb)) in samples.iter().zip(BARS).enumerate() {
                let expected = bt709_limited(rgb);
                let close = sample
                    .iter()
                    .zip(expected)
                    .all(|(&actual, expected)| actual.abs_diff(expected) <= tolerance);
                assert!(
                    close,
                    "{} {output_width}x{output_height} bar {bar} {rgb:?}: got {sample:?}, expected {expected:?}",
                    converter.method()
                );
            }
        }
    }
    assert_no_debug_problems(&device);
}

#[test]
#[ignore = "requires a GPU"]
fn sustained_conversion_neither_leaks_nor_drops() {
    let device = hardware_device();
    let source = bar_texture(&device);
    let handles = || {
        let mut count = 0;
        // SAFETY: pseudo-handle of the current process and a live out-pointer.
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }.unwrap();
        count
    };
    for mut converter in converters(&device, WIDTH / 2, HEIGHT / 2) {
        // The driver creates some handles once, on first use of a pipeline
        // (+11 seen within the first ~1000 shader conversions, then flat
        // over 8000): warm up before taking the baseline.
        for _ in 0..1_000 {
            drop(converter.convert(&source).unwrap());
        }
        let before = handles();
        for _ in 0..2_000 {
            let output = converter.convert(&source).unwrap();
            assert!(output.is_some(), "{} pool exhausted", converter.method());
        }
        let after = handles();
        assert_eq!(converter.pool_dropped(), 0, "{}", converter.method());
        assert!(
            after <= before + 2,
            "{}: handles {before} -> {after}",
            converter.method()
        );
    }
    assert_no_debug_problems(&device);
}
