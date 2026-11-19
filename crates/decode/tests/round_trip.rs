//! Encode synthetic stamped frames on the GPU, decode them with the Media
//! Foundation decoder and read every frame's stamp back from a small region
//! of the decoded NV12 texture. Ignored by default; run with
//! `scripts/gpu-tests.ps1`.

use std::time::{Duration, Instant};

use capture::synthetic::{PatternMode, SyntheticConfig, SyntheticSource};
use decode::media_foundation::MediaFoundationDecoder;
use decode::{DecodeSubmit, DecodedFrame, VideoDecoder};
use encode::{EncodedChunk, EncoderConfig, EncoderPreference, Submit, VideoEncoder};
use gpu::convert::{ConvertConfig, Converter, ShaderConverter, read_nv12_luma_region};
use gpu::{AdapterInfo, D3d11Device, DebugLayer, FrameFormat, GpuFrame};
use telemetry::frame_code::{self, read_frame_stamp};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

struct Rig {
    device: D3d11Device,
    adapter: AdapterInfo,
    source: SyntheticSource,
    converter: ShaderConverter,
    _media_foundation: mf::MediaFoundation,
}

fn rig() -> Rig {
    // SAFETY: joins the MTA for this test thread (it stays joined).
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let media_foundation = mf::MediaFoundation::start().unwrap();
    let adapter = gpu::enumerate_adapters()
        .unwrap()
        .into_iter()
        .find(|adapter| !adapter.info.is_software)
        .expect("hardware adapter");
    let device = gpu::create_device(&adapter, DebugLayer::IfAvailable).unwrap();
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
    Rig {
        device,
        adapter: adapter.info,
        source,
        converter,
        _media_foundation: media_foundation,
    }
}

impl Rig {
    fn encoder(&self, preference: EncoderPreference) -> Box<dyn VideoEncoder> {
        let (encoder, selection) = encode::create_encoder(
            &self.device,
            &self.adapter,
            EncoderConfig {
                width: WIDTH,
                height: HEIGHT,
                fps: 60,
                bitrate_bps: 10_000_000,
                keyframe_interval: 120,
                max_in_flight: 4,
            },
            preference,
        )
        .unwrap();
        eprintln!("encoder: {:?} {}", selection.kind, selection.detail);
        encoder
    }

    /// Renders, converts and encodes `count` frames; returns the access units.
    fn encode(&mut self, encoder: &mut dyn VideoEncoder, count: usize) -> Vec<EncodedChunk> {
        let mut chunks = Vec::new();
        for _ in 0..count {
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
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match encoder.submit(frame).unwrap() {
                    Submit::Accepted => break,
                    Submit::Busy(returned) => {
                        assert!(Instant::now() < deadline, "encoder busy for 2 s");
                        frame = returned;
                        while let Some(chunk) = encoder.poll().unwrap() {
                            chunks.push(chunk);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            while let Some(chunk) = encoder.poll().unwrap() {
                chunks.push(chunk);
            }
        }
        chunks.extend(encoder.drain().unwrap());
        chunks
    }
}

/// What a decoded frame looked like, read while it was held.
#[derive(Debug)]
struct Seen {
    frame_id: u64,
    stamp_id: Option<u32>,
    size: (u32, u32),
    latency: Duration,
}

/// Reads the stamp from the top-left region of a decoded frame only.
fn inspect(device: &D3d11Device, frame: &DecodedFrame, ticks_per_us: f64) -> Seen {
    let luma = read_nv12_luma_region(
        device,
        &frame.texture,
        frame.slice,
        frame_code::CODE_WIDTH,
        frame_code::CODE_HEIGHT,
    )
    .unwrap();
    let width = frame_code::CODE_WIDTH;
    let stamp = read_frame_stamp(|x, y| luma[(y * width + x) as usize]);
    let micros = frame
        .decoded_at
        .ticks()
        .saturating_sub(frame.submitted_at.ticks()) as f64
        / ticks_per_us;
    Seen {
        frame_id: frame.frame_id,
        stamp_id: stamp.ok().map(|stamp| stamp.frame_id),
        size: (frame.width, frame.height),
        latency: Duration::from_secs_f64(micros.max(0.0) / 1e6),
    }
}

/// Feeds every access unit to the decoder and inspects each frame as it
/// comes out (frames are released right away, as a presenter would).
fn decode_all(device: &D3d11Device, chunks: &[EncodedChunk]) -> Vec<Seen> {
    let ticks_per_us = telemetry::qpc_frequency().ticks_per_second() as f64 / 1e6;
    let mut decoder = MediaFoundationDecoder::new(device).unwrap();
    let mut seen = Vec::new();
    for chunk in chunks {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match decoder
                .submit(&chunk.data, chunk.frame_id, chunk.captured_at)
                .unwrap()
            {
                DecodeSubmit::Accepted => break,
                DecodeSubmit::Busy => {
                    assert!(Instant::now() < deadline, "decoder busy for 2 s");
                    while let Some(frame) = decoder.poll().unwrap() {
                        seen.push(inspect(device, &frame, ticks_per_us));
                    }
                }
            }
        }
        while let Some(frame) = decoder.poll().unwrap() {
            seen.push(inspect(device, &frame, ticks_per_us));
        }
    }
    for frame in decoder.drain().unwrap() {
        seen.push(inspect(device, &frame, ticks_per_us));
    }
    seen
}

fn assert_no_debug_problems(device: &D3d11Device) {
    let problems: Vec<_> = device
        .take_debug_messages()
        .into_iter()
        .filter(|message| message.severity.is_problem())
        .collect();
    assert!(problems.is_empty(), "{problems:#?}");
}

fn decodes_every_frame_in_order(preference: EncoderPreference) {
    let mut rig = rig();
    let mut encoder = rig.encoder(preference);
    let chunks = rig.encode(encoder.as_mut(), 300);
    drop(encoder);
    assert_eq!(chunks.len(), 300);

    let seen = decode_all(&rig.device, &chunks);
    let mut latencies: Vec<Duration> = seen.iter().map(|frame| frame.latency).collect();
    latencies.sort();
    eprintln!(
        "decoded {} frames; submit->decoded p50 {:?}, max {:?}",
        seen.len(),
        latencies.get(latencies.len() / 2),
        latencies.last()
    );
    assert_eq!(seen.len(), 300, "frames out");
    let ids: Vec<u64> = seen.iter().map(|frame| frame.frame_id).collect();
    assert_eq!(ids, (0..300).collect::<Vec<u64>>());
    for frame in &seen {
        assert_eq!(frame.size, (WIDTH, HEIGHT), "frame {}", frame.frame_id);
        assert_eq!(
            frame.stamp_id.map(u64::from),
            Some(frame.frame_id),
            "stamp of frame {}",
            frame.frame_id
        );
    }
    assert_no_debug_problems(&rig.device);
}

#[test]
#[ignore = "requires a GPU with hardware H.264 encode and decode"]
fn media_foundation_stream_decodes_every_frame_in_order() {
    decodes_every_frame_in_order(EncoderPreference::MediaFoundation);
}

#[test]
#[ignore = "requires a GPU with hardware H.264 encode and decode"]
fn auto_encoder_stream_decodes_every_frame_in_order() {
    decodes_every_frame_in_order(EncoderPreference::Auto);
}

#[test]
#[ignore = "requires a GPU with hardware H.264 encode and decode"]
fn sustained_decoding_does_not_leak_handles() {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let handles = || {
        let mut count = 0;
        // SAFETY: pseudo-handle of the current process and a live out-pointer.
        unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) }.unwrap();
        count
    };
    let mut rig = rig();
    let mut encoder = rig.encoder(EncoderPreference::Auto);
    let chunks = rig.encode(encoder.as_mut(), 300);
    drop(encoder);
    // Warm up once, then decode the same stream three more times.
    assert_eq!(decode_all(&rig.device, &chunks).len(), 300);
    let before = handles();
    for _ in 0..3 {
        assert_eq!(decode_all(&rig.device, &chunks).len(), 300);
    }
    let after = handles();
    assert!(after <= before + 5, "handles {before} -> {after}");
    assert_no_debug_problems(&rig.device);
}
