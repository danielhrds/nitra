//! Viewer audio: Opus packets from the network thread -> decoder -> player,
//! on a thread of its own (audio never waits for video, nor video for
//! audio). Short losses are concealed by the decoder; a longer gap starts
//! over (the player rebuilds its queue). Records when each packet is heard,
//! against its media time, for the A/V offset.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::Duration;

use audio::opus::{CHANNELS, FRAME_SAMPLES, OpusDecoder};
use audio::player::{AudioPlayer, PlayerStats};
use telemetry::{QpcTimestamp, qpc_frequency, qpc_now};
use transport::AudioPacket;

/// Packets waiting for the decoder (1 s).
const QUEUE: usize = 50;
/// Media time of one packet at 48 kHz.
const PACKET_TICKS: u64 = FRAME_SAMPLES as u64;
/// Up to this many lost packets in a row are concealed (100 ms).
const MAX_CONCEALED: u64 = 5;
const MAX_HEARD: usize = 1 << 16;
/// Delays kept between two reads of [`LiveAudio`] (~10 s of packets).
const MAX_DELAYS: usize = 512;
/// How often the player's counters are copied for [`LiveAudio`] (in packets: 1 s).
const STATS_EVERY: u64 = 50;

/// What the audio thread shares while it plays, for the live numbers.
#[derive(Debug, Default)]
pub struct LiveAudio {
    /// From a packet's arrival to it being heard (queue, device buffer and
    /// the device's own latency), in microseconds, since the last `take`.
    pub heard_after_us: Vec<u32>,
    pub packets: u64,
    pub concealed: u64,
    pub gaps: u64,
    pub catch_ups: u64,
    pub underruns: u64,
}

impl LiveAudio {
    /// Takes the delays, keeping the running totals.
    pub fn take_delays(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.heard_after_us)
    }
}

#[derive(Debug, Default)]
pub struct AudioReport {
    pub packets: u64,
    /// Dropped before the decoder (its queue was full).
    pub dropped_queue: u64,
    /// Older than what was already played (late or duplicated).
    pub late: u64,
    pub concealed: u64,
    /// Losses too long to conceal: playback started over.
    pub gaps: u64,
    pub decode_errors: u64,
    pub player: Option<PlayerStats>,
    /// (media time at 48 kHz, when it is heard — or decoded, without a player).
    pub heard: Vec<(u64, QpcTimestamp)>,
}

pub struct AudioOut {
    packets: SyncSender<AudioPacket>,
    live: Arc<std::sync::Mutex<LiveAudio>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<AudioReport>>,
    dropped: u64,
}

impl AudioOut {
    /// `play`: open the default output (else decode only, e.g. headless or muted).
    /// `volume`: percent (0 to 100), read for every packet (the app's slider).
    pub fn start(play: bool, target: Duration, volume: Arc<AtomicU8>) -> Result<Self, String> {
        let player = if play {
            Some(AudioPlayer::start(target)?)
        } else {
            None
        };
        let decoder = OpusDecoder::new().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let live = Arc::default();
        let thread = {
            let (stop, live) = (Arc::clone(&stop), Arc::clone(&live));
            std::thread::Builder::new()
                .name("audio-out".into())
                .spawn(move || run(&rx, decoder, player, &stop, &live, &volume))
                .map_err(|e| e.to_string())?
        };
        Ok(Self {
            packets: tx,
            live,
            stop,
            thread: Some(thread),
            dropped: 0,
        })
    }

    /// The numbers the audio thread shares while it plays.
    pub fn live(&self) -> Arc<std::sync::Mutex<LiveAudio>> {
        Arc::clone(&self.live)
    }

    /// A sender for the network thread.
    pub fn sender(&self) -> SyncSender<AudioPacket> {
        self.packets.clone()
    }

    pub fn finish(mut self, dropped_by_network: u64) -> AudioReport {
        self.stop.store(true, Ordering::Relaxed);
        let mut report = self
            .thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default();
        report.dropped_queue = self.dropped + dropped_by_network;
        report
    }
}

/// The amplitude scale for a volume in percent: the square, so the slider
/// follows loudness as heard (50% is about half as loud, -12 dB) rather than
/// barely changing until near the bottom. Never above 1: no clipping.
pub fn gain(percent: u8) -> f32 {
    let v = f32::from(percent.min(100)) / 100.0;
    v * v
}

/// Scales decoded samples to `percent` (100: untouched; 0: silence).
fn apply_volume(pcm: &mut [f32], percent: u8) {
    if percent >= 100 {
        return;
    }
    let gain = gain(percent);
    for sample in pcm {
        *sample *= gain;
    }
}

/// Hands a packet to the audio thread without ever waiting; false if dropped.
pub fn offer(packets: &SyncSender<AudioPacket>, packet: AudioPacket) -> bool {
    !matches!(packets.try_send(packet), Err(TrySendError::Full(_)))
}

fn run(
    packets: &Receiver<AudioPacket>,
    mut decoder: OpusDecoder,
    player: Option<AudioPlayer>,
    stop: &AtomicBool,
    live: &std::sync::Mutex<LiveAudio>,
    volume: &AtomicU8,
) -> AudioReport {
    let mut report = AudioReport::default();
    let mut pcm = vec![0_f32; FRAME_SAMPLES * CHANNELS];
    let mut expected: Option<u64> = None;
    let out = |pcm: &[f32], rtp: u64, report: &mut AudioReport| {
        let heard = match &player {
            Some(player) => {
                let delay = player.push(pcm);
                let mut live = live.lock().expect("live audio lock");
                if live.heard_after_us.len() < MAX_DELAYS {
                    live.heard_after_us
                        .push(delay.as_micros().min(u128::from(u32::MAX)) as u32);
                }
                let ticks = delay.as_secs_f64() * qpc_frequency().ticks_per_second() as f64;
                QpcTimestamp::from_ticks(qpc_now().ticks() + ticks as i64)
            }
            None => qpc_now(),
        };
        if report.heard.len() < MAX_HEARD {
            report.heard.push((rtp, heard));
        }
    };
    while !stop.load(Ordering::Relaxed) {
        let packet = match packets.recv_timeout(Duration::from_millis(100)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        report.packets += 1;
        if let Some(next) = expected {
            if packet.rtp_time < next {
                report.late += 1;
                continue;
            }
            let missing = (packet.rtp_time - next) / PACKET_TICKS;
            if missing > MAX_CONCEALED {
                report.gaps += 1;
            } else {
                for i in 0..missing {
                    // The next packet's FEC rebuilds the last lost one; the
                    // others are concealed.
                    let rebuilt = if i + 1 == missing {
                        decoder.decode_lost_from(&packet.data, &mut pcm)
                    } else {
                        decoder.decode(None, &mut pcm)
                    };
                    if rebuilt.is_ok() {
                        apply_volume(&mut pcm, volume.load(Ordering::Relaxed));
                        report.concealed += 1;
                        out(&pcm, next + i * PACKET_TICKS, &mut report);
                    }
                }
            }
        }
        match decoder.decode(Some(&packet.data), &mut pcm) {
            Ok(_) => {
                apply_volume(&mut pcm, volume.load(Ordering::Relaxed));
                out(&pcm, packet.rtp_time, &mut report);
            }
            Err(error) => {
                report.decode_errors += 1;
                tracing::debug!(%error, "audio decode failed");
            }
        }
        expected = Some(packet.rtp_time + PACKET_TICKS);
        if report.packets.is_multiple_of(STATS_EVERY) {
            let player = player.as_ref().map(AudioPlayer::stats);
            let mut live = live.lock().expect("live audio lock");
            live.packets = report.packets;
            live.concealed = report.concealed;
            live.gaps = report.gaps;
            live.catch_ups = player.as_ref().map_or(0, |p| p.catch_ups);
            live.underruns = player.as_ref().map_or(0, |p| p.underruns);
        }
    }
    report.player = player.map(AudioPlayer::stop);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_volume_leaves_the_sound_alone_and_zero_is_silence() {
        let original = [0.9_f32, -1.0, 0.25, -0.5];
        let mut pcm = original;
        apply_volume(&mut pcm, 100);
        assert_eq!(pcm, original);
        apply_volume(&mut pcm, 0);
        assert!(pcm.iter().all(|&s| s == 0.0), "{pcm:?}");
    }

    #[test]
    fn the_volume_only_ever_lowers_the_sound() {
        for percent in 0..=255_u8 {
            let g = gain(percent);
            assert!((0.0..=1.0).contains(&g), "{percent}: {g}");
            let mut pcm = [1.0_f32, -1.0];
            apply_volume(&mut pcm, percent);
            assert!(pcm.iter().all(|s| s.abs() <= 1.0), "{percent}: {pcm:?}");
        }
        // Rising with the slider, and about -12 dB at half.
        assert!((1..=100).all(|p| gain(p) > gain(p - 1)));
        let half_db = 20.0 * gain(50).log10();
        assert!((half_db + 12.04).abs() < 0.01, "{half_db}");
    }
}
