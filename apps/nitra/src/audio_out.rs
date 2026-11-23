//! Viewer audio: Opus packets from the network thread -> decoder -> player,
//! on a thread of its own (audio never waits for video, nor video for
//! audio). Short losses are concealed by the decoder; a longer gap starts
//! over (the player rebuilds its queue). Records when each packet is heard,
//! against its media time, for the A/V offset.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<AudioReport>>,
    dropped: u64,
}

impl AudioOut {
    /// `play`: open the default output (else decode only, e.g. headless or muted).
    pub fn start(play: bool, target: Duration) -> Result<Self, String> {
        let player = if play {
            Some(AudioPlayer::start(target)?)
        } else {
            None
        };
        let decoder = OpusDecoder::new().map_err(|e| e.to_string())?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("audio-out".into())
                .spawn(move || run(&rx, decoder, player, &stop))
                .map_err(|e| e.to_string())?
        };
        Ok(Self {
            packets: tx,
            stop,
            thread: Some(thread),
            dropped: 0,
        })
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

/// Hands a packet to the audio thread without ever waiting; false if dropped.
pub fn offer(packets: &SyncSender<AudioPacket>, packet: AudioPacket) -> bool {
    !matches!(packets.try_send(packet), Err(TrySendError::Full(_)))
}

fn run(
    packets: &Receiver<AudioPacket>,
    mut decoder: OpusDecoder,
    player: Option<AudioPlayer>,
    stop: &AtomicBool,
) -> AudioReport {
    let mut report = AudioReport::default();
    let mut pcm = vec![0_f32; FRAME_SAMPLES * CHANNELS];
    let mut expected: Option<u64> = None;
    let out = |pcm: &[f32], rtp: u64, report: &mut AudioReport| {
        let heard = match &player {
            Some(player) => {
                let delay = player.push(pcm);
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
                        report.concealed += 1;
                        out(&pcm, next + i * PACKET_TICKS, &mut report);
                    }
                }
            }
        }
        match decoder.decode(Some(&packet.data), &mut pcm) {
            Ok(_) => out(&pcm, packet.rtp_time, &mut report),
            Err(error) => {
                report.decode_errors += 1;
                tracing::debug!(%error, "audio decode failed");
            }
        }
        expected = Some(packet.rtp_time + PACKET_TICKS);
    }
    report.player = player.map(AudioPlayer::stop);
    report
}
