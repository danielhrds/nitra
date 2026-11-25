//! The preview texture shared between two devices, on a real GPU. Ignored
//! by default; run with `scripts/gpu-tests.ps1`.

use gpu::convert::{PreviewReader, PreviewWriter, read_nv12_samples};
use gpu::{D3d11Device, DebugLayer};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

fn hardware_device() -> D3d11Device {
    let adapters = gpu::enumerate_adapters().unwrap();
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    gpu::create_device(adapter, DebugLayer::IfAvailable).unwrap()
}

/// A square of one color (test-only CPU -> GPU upload).
fn square(device: &D3d11Device, side: u32, bgra: [u8; 4]) -> ID3D11Texture2D {
    let pixels: Vec<u8> = bgra.repeat((side * side) as usize);
    let desc = D3D11_TEXTURE2D_DESC {
        Width: side,
        Height: side,
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
        SysMemPitch: side * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: `pixels` holds side x side BGRA pixels and outlives the call.
    unsafe {
        device
            .raw()
            .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
    }
    .unwrap();
    texture.unwrap()
}

/// BT.709 limited-range luma of an RGB color (what the NV12 holds).
fn luma([r, g, b]: [f64; 3]) -> u8 {
    (16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b)).round() as u8
}

#[test]
#[ignore = "requires a GPU"]
fn a_frame_scaled_on_one_device_is_read_on_another() {
    let (writer_device, reader_device) = (hardware_device(), hardware_device());
    let mut writer = PreviewWriter::new(&writer_device, 640, 360).unwrap();
    let reader = PreviewReader::open(&reader_device, writer.handle()).unwrap();
    // The luma at the middle and in both side bars.
    let lumas = |reader: &PreviewReader| -> Vec<u8> {
        read_nv12_samples(
            &reader_device,
            reader.texture(),
            &[(320, 180), (20, 180), (620, 180)],
        )
        .unwrap()
        .iter()
        .map(|[y, _, _]| *y)
        .collect()
    };
    let near = |a: u8, b: u8| a.abs_diff(b) <= 4;
    let black = luma([0.0, 0.0, 0.0]);
    // A square source: red in the middle, black bars at the sides.
    let red = square(&writer_device, 1000, [0, 0, 255, 255]);
    assert!(writer.write(&red).unwrap());
    assert!(reader.refresh());
    let y = lumas(&reader);
    assert!(near(y[0], luma([1.0, 0.0, 0.0])), "middle {y:?}");
    assert!(near(y[1], black) && near(y[2], black), "bars {y:?}");
    // Another size and color: the writer follows it.
    let green = square(&writer_device, 500, [0, 255, 0, 255]);
    assert!(writer.write(&green).unwrap());
    assert!(reader.refresh());
    let y = lumas(&reader);
    assert!(near(y[0], luma([0.0, 1.0, 0.0])), "middle {y:?}");
}
