//! Both hardware H.264 encoders (Media Foundation MFT and AMF) on a real
//! GPU, fed with synthetic frames converted to NV12 and checked the same
//! way. Ignored by default; run with `scripts/gpu-tests.ps1`.

use std::time::{Duration, Instant};

use capture::synthetic::{PatternMode, SyntheticConfig, SyntheticSource};
use encode::amf::AmfEncoder;
use encode::annexb::{self, SliceType, nal_type};
use encode::media_foundation::MediaFoundationEncoder;
use encode::{EncodedChunk, EncoderConfig, Submit, VideoEncoder};
use gpu::convert::{ConvertConfig, Converter, ShaderConverter};
use gpu::{D3d11Device, DebugLayer, FrameFormat, GpuFrame};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const GOP: u32 = 600;
/// Not a multiple of common default GOP sizes (30, 60), so a keyframe here
/// can only come from `request_keyframe`.
const REQUESTED_KEYFRAME: u64 = 167;

#[derive(Clone, Copy, Debug)]
enum Backend {
    MediaFoundation,
    Amf,
}

struct Rig {
    // Field order is drop order: the encoder goes before the device.
    encoder: Box<dyn VideoEncoder>,
    converter: ShaderConverter,
    source: SyntheticSource,
    device: D3d11Device,
    _media_foundation: mf::MediaFoundation,
}

fn config() -> EncoderConfig {
    EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        fps: 60,
        bitrate_bps: 10_000_000,
        // 10 s: long enough that a requested keyframe is distinguishable.
        keyframe_interval: GOP,
        max_in_flight: 4,
    }
}

fn rig(backend: Backend) -> Rig {
    rig_with(backend, config())
}

fn rig_with(backend: Backend, config: EncoderConfig) -> Rig {
    // SAFETY: joins the MTA for this test thread (it stays joined).
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let media_foundation = mf::MediaFoundation::start().unwrap();
    let adapters = gpu::enumerate_adapters().unwrap();
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    let device = gpu::create_device(adapter, DebugLayer::IfAvailable).unwrap();
    let source = SyntheticSource::new(
        &device,
        SyntheticConfig {
            width: WIDTH,
            height: HEIGHT,
            mode: PatternMode::Motion,
        },
    )
    .unwrap();
    let converter = ShaderConverter::new(
        &device,
        ConvertConfig {
            input_width: WIDTH,
            input_height: HEIGHT,
            output_width: WIDTH,
            output_height: HEIGHT,
            pool_size: 6,
        },
    )
    .unwrap();
    let encoder: Box<dyn VideoEncoder> = match backend {
        Backend::MediaFoundation => {
            let encoder = MediaFoundationEncoder::new(&device, adapter.info.luid, config).unwrap();
            eprintln!(
                "encoder: {:?} ({} candidate(s) for the adapter), applied {:?}",
                encoder.info().name,
                encoder.candidates(),
                encoder.applied_settings()
            );
            Box::new(encoder)
        }
        Backend::Amf => {
            let encoder = AmfEncoder::new(&device, config).unwrap();
            eprintln!(
                "encoder: AMF runtime {}, applied {:?}",
                encoder.runtime_version(),
                encoder.applied_settings()
            );
            Box::new(encoder)
        }
    };
    Rig {
        encoder,
        converter,
        source,
        device,
        _media_foundation: media_foundation,
    }
}

impl Rig {
    /// Renders, converts and submits one frame, waiting while the encoder is
    /// busy; returns the access units that came out meanwhile.
    fn encode_one(&mut self) -> Vec<EncodedChunk> {
        let synthetic = self.source.render_next().unwrap();
        let texture = self
            .converter
            .convert(&synthetic.texture)
            .unwrap()
            .expect("free NV12 texture");
        let mut frame = GpuFrame {
            texture,
            format: FrameFormat::Nv12,
            width: WIDTH,
            height: HEIGHT,
            frame_id: u64::from(synthetic.stamp.frame_id),
            captured_at: synthetic.stamp.timestamp,
        };
        let mut chunks = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.encoder.submit(frame).unwrap() {
                Submit::Accepted => break,
                Submit::Busy(returned) => {
                    assert!(Instant::now() < deadline, "encoder busy for 2 s");
                    frame = returned;
                    self.collect(&mut chunks);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        self.collect(&mut chunks);
        chunks
    }

    fn collect(&mut self, chunks: &mut Vec<EncodedChunk>) {
        while let Some(chunk) = self.encoder.poll().unwrap() {
            chunks.push(chunk);
        }
    }
}

fn assert_no_debug_problems(device: &D3d11Device) {
    let problems: Vec<_> = device
        .take_debug_messages()
        .into_iter()
        .filter(|message| message.severity.is_problem())
        .collect();
    assert!(problems.is_empty(), "{problems:#?}");
}

fn encodes_every_frame_in_order_without_b_frames(backend: Backend) {
    let mut rig = rig(backend);
    let mut chunks = Vec::new();
    for index in 0..300 {
        if index == REQUESTED_KEYFRAME {
            rig.encoder.request_keyframe();
        }
        chunks.extend(rig.encode_one());
    }
    chunks.extend(rig.encoder.drain().unwrap());

    assert_eq!(chunks.len(), 300, "access units out");
    let ids: Vec<u64> = chunks.iter().map(|chunk| chunk.frame_id).collect();
    assert_eq!(ids, (0..300).collect::<Vec<u64>>());

    let first = &chunks[0];
    assert!(first.keyframe, "first access unit must be a keyframe");
    let first_types: Vec<u8> = annexb::nal_units(&first.data)
        .iter()
        .map(|nal| annexb::nal_unit_type(nal))
        .collect();
    assert!(first_types.contains(&nal_type::SPS), "{first_types:?}");
    assert!(first_types.contains(&nal_type::PPS), "{first_types:?}");
    assert!(
        first_types.contains(&nal_type::IDR_SLICE),
        "{first_types:?}"
    );

    for chunk in &chunks {
        for nal in annexb::nal_units(&chunk.data) {
            assert_ne!(
                annexb::slice_type(nal),
                Some(SliceType::B),
                "frame {}",
                chunk.frame_id
            );
        }
        assert!(chunk.output_at >= chunk.submitted_at);
    }

    let keyframes: Vec<u64> = chunks
        .iter()
        .filter(|chunk| chunk.keyframe)
        .map(|chunk| chunk.frame_id)
        .collect();
    eprintln!("keyframes at {keyframes:?}");
    // Only the first frame and the requested one: the 10 s GOP is honored
    // and the request works on its own.
    assert_eq!(keyframes, [0, REQUESTED_KEYFRAME]);
    let total_bytes: usize = chunks.iter().map(|chunk| chunk.data.len()).sum();
    eprintln!(
        "total {total_bytes} bytes for 300 frames ({:.1} Mbit/s at 60 fps)",
        total_bytes as f64 * 8.0 / 5.0 / 1e6
    );
    assert_no_debug_problems(&rig.device);
}

fn inserts_periodic_keyframes(backend: Backend) {
    let mut rig = rig_with(
        backend,
        EncoderConfig {
            keyframe_interval: 60,
            ..config()
        },
    );
    let mut chunks = Vec::new();
    for _ in 0..150 {
        chunks.extend(rig.encode_one());
    }
    chunks.extend(rig.encoder.drain().unwrap());
    assert_eq!(chunks.len(), 150, "access units out");
    let keyframes: Vec<u64> = chunks
        .iter()
        .filter(|chunk| chunk.keyframe)
        .map(|chunk| chunk.frame_id)
        .collect();
    assert_eq!(keyframes, [0, 60, 120]);
    for chunk in chunks.iter().filter(|chunk| chunk.keyframe) {
        let types: Vec<u8> = annexb::nal_units(&chunk.data)
            .iter()
            .map(|nal| annexb::nal_unit_type(nal))
            .collect();
        assert!(
            types.contains(&nal_type::SPS) && types.contains(&nal_type::IDR_SLICE),
            "frame {}: {types:?}",
            chunk.frame_id
        );
    }
}

fn sustained_encoding_does_not_leak_handles(backend: Backend) {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let handles = || {
        let mut count = 0;
        // SAFETY: pseudo-handle of the current process and a live out-pointer.
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }.unwrap();
        count
    };
    let mut rig = rig(backend);
    let mut out = 0;
    for _ in 0..300 {
        out += rig.encode_one().len();
    }
    let before = handles();
    for _ in 0..600 {
        out += rig.encode_one().len();
    }
    let after = handles();
    out += rig.encoder.drain().unwrap().len();
    assert_eq!(out, 900);
    assert!(after <= before + 3, "handles {before} -> {after}");
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU with a hardware H.264 encoder"]
fn media_foundation_encodes_every_frame_in_order_without_b_frames() {
    encodes_every_frame_in_order_without_b_frames(Backend::MediaFoundation);
}

#[test]
#[ignore = "requires a GPU with a hardware H.264 encoder"]
fn media_foundation_inserts_periodic_keyframes() {
    inserts_periodic_keyframes(Backend::MediaFoundation);
}

#[test]
#[ignore = "requires a GPU with a hardware H.264 encoder"]
fn media_foundation_sustained_encoding_does_not_leak_handles() {
    sustained_encoding_does_not_leak_handles(Backend::MediaFoundation);
}

#[test]
#[ignore = "requires an AMD GPU and driver"]
fn amf_encodes_every_frame_in_order_without_b_frames() {
    encodes_every_frame_in_order_without_b_frames(Backend::Amf);
}

#[test]
#[ignore = "requires an AMD GPU and driver"]
fn amf_inserts_periodic_keyframes() {
    inserts_periodic_keyframes(Backend::Amf);
}

#[test]
#[ignore = "requires an AMD GPU and driver"]
fn amf_sustained_encoding_does_not_leak_handles() {
    sustained_encoding_does_not_leak_handles(Backend::Amf);
}
