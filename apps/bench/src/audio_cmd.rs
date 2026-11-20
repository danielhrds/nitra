//! `bench audio`: the audio path in one process — capture → Opus → decode →
//! playback — with its cost, its delay and the playback clock's drift.

use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use audio::capture::{AudioCapture, CaptureSource};
use audio::opus::{CHANNELS, FRAME_SAMPLES, MAX_PACKET, OpusDecoder, OpusEncoder};
use audio::player::{AudioPlayer, PlayerStats};
use serde::Serialize;
use telemetry::{QpcDuration, Summary, qpc_frequency, qpc_now};

use crate::process_stats::ProcessStats;

pub struct AudioCommand {
    pub source: CaptureSource,
    pub seconds: u64,
    pub bitrate_bps: u32,
    pub target: Duration,
    /// Encode and decode only, play nothing.
    pub play: bool,
    pub summary_json: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct AudioSummary {
    source: String,
    seconds: f64,
    frames: u64,
    capture_dropped: u64,
    capture_restarts: u64,
    /// Mean Opus packet size as a bitrate.
    opus_kbps: f64,
    encode_us: Option<Micros>,
    decode_us: Option<Micros>,
    /// First sample captured to its frame decoded (capture framing + codec).
    capture_to_decoded_ms: Option<Micros>,
    /// Queue + device buffer while playing (the playback delay).
    playback_level_ms: Option<(f64, f64)>,
    ratio_range: Option<(f64, f64)>,
    underruns: u64,
    trimmed_frames: u64,
    device_drift_ppm: Option<f64>,
    cpu_percent_of_a_core: f64,
    private_mb: f64,
}

#[derive(Debug, Serialize)]
struct Micros {
    p50: f64,
    p95: f64,
    max: f64,
}

fn micros(samples: &[QpcDuration]) -> Option<Micros> {
    let s = Summary::from_durations(samples, qpc_frequency())?;
    Some(Micros {
        p50: s.p50_us,
        p95: s.p95_us,
        max: s.max_us,
    })
}

pub fn run(command: AudioCommand) -> Result<(), Box<dyn Error>> {
    let before = ProcessStats::current()?;
    let started = Instant::now();
    let capture = AudioCapture::start(command.source)?;
    let player = if command.play {
        Some(AudioPlayer::start(command.target)?)
    } else {
        None
    };
    let mut encoder = OpusEncoder::new(command.bitrate_bps)?;
    let mut decoder = OpusDecoder::new()?;
    let mut packet = [0_u8; MAX_PACKET];
    let mut pcm = vec![0_f32; FRAME_SAMPLES * CHANNELS];
    let (mut encode, mut decode, mut latency) = (Vec::new(), Vec::new(), Vec::new());
    let (mut frames, mut bytes) = (0_u64, 0_u64);
    let mut next_report = started + Duration::from_secs(10);
    println!(
        "audio: {:?} → Opus {} kbps → {}",
        command.source,
        command.bitrate_bps / 1000,
        if command.play {
            "playback"
        } else {
            "nothing (no playback)"
        }
    );
    while started.elapsed() < Duration::from_secs(command.seconds) {
        let Some(frame) = capture.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        let t0 = qpc_now();
        let n = encoder.encode(&frame.samples, &mut packet)?;
        let t1 = qpc_now();
        decoder.decode(Some(&packet[..n]), &mut pcm)?;
        let t2 = qpc_now();
        encode.push(t1.duration_since(t0));
        decode.push(t2.duration_since(t1));
        latency.push(t2.duration_since(frame.captured_at));
        frames += 1;
        bytes += n as u64;
        if let Some(player) = &player {
            player.push(&pcm);
        }
        if Instant::now() >= next_report {
            next_report += Duration::from_secs(10);
            if let Some(player) = &player {
                let s = player.stats();
                println!(
                    "  {:>4.0} s: level {:.1} ms (range {:.1}–{:.1}), ratio {:.5}–{:.5}, underruns {}, device drift {}",
                    started.elapsed().as_secs_f64(),
                    s.level_ms,
                    s.level_min_ms,
                    s.level_max_ms,
                    s.ratio_min,
                    s.ratio_max,
                    s.underruns,
                    s.device_drift_ppm
                        .map_or("?".into(), |p| format!("{p:+.1} ppm"))
                );
            }
        }
    }
    let report = capture.stop();
    let playback: Option<PlayerStats> = player.map(AudioPlayer::stop);
    let after = ProcessStats::current()?;
    let seconds = started.elapsed().as_secs_f64();
    let summary = AudioSummary {
        source: format!("{:?}", command.source),
        seconds,
        frames,
        capture_dropped: report.dropped,
        capture_restarts: report.restarts,
        opus_kbps: bytes as f64 * 8.0 / (frames.max(1) as f64 * 0.02) / 1000.0,
        encode_us: micros(&encode),
        decode_us: micros(&decode),
        capture_to_decoded_ms: micros(&latency).map(|m| Micros {
            p50: m.p50 / 1000.0,
            p95: m.p95 / 1000.0,
            max: m.max / 1000.0,
        }),
        playback_level_ms: playback
            .as_ref()
            .filter(|s| s.level_max_ms > 0.0)
            .map(|s| (s.level_min_ms, s.level_max_ms)),
        ratio_range: playback.as_ref().map(|s| (s.ratio_min, s.ratio_max)),
        underruns: playback.as_ref().map_or(0, |s| s.underruns),
        trimmed_frames: playback.as_ref().map_or(0, |s| s.trimmed_frames),
        device_drift_ppm: playback.as_ref().and_then(|s| s.device_drift_ppm),
        cpu_percent_of_a_core: (after.cpu_time_100ns - before.cpu_time_100ns) as f64
            / 1e7
            / seconds
            * 100.0,
        private_mb: after.private_bytes as f64 / (1024.0 * 1024.0),
    };
    if let Some(error) = report.error.or(playback.and_then(|s| s.error)) {
        return Err(error.into());
    }
    let json = serde_json::to_string_pretty(&summary)?;
    println!("{json}");
    if let Some(path) = command.summary_json {
        std::fs::write(path, json)?;
    }
    Ok(())
}
