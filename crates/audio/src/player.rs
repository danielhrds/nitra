//! Playback on the default output device (WASAPI shared mode, event-driven,
//! 48 kHz stereo float converted by Windows). Frames are pushed from any
//! thread into a bounded queue; the render thread plays them through the
//! drift controller so the queue (and the delay) holds its target.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
    IAudioClock, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::drift::{DriftController, Resampler};
use crate::opus::{CHANNELS, SAMPLE_RATE};

#[derive(Clone, Debug, Default)]
pub struct PlayerStats {
    pub frames_pushed: u64,
    /// Device periods with too little audio queued (silence filled in).
    pub underruns: u64,
    /// Frames dropped because the queue passed its bound.
    pub trimmed_frames: u64,
    pub ratio_min: f64,
    pub ratio_max: f64,
    /// Queue + device buffer, last seen and its range after start-up.
    pub level_ms: f64,
    pub level_min_ms: f64,
    pub level_max_ms: f64,
    /// Output device clock against QPC, in ppm (positive: device fast).
    pub device_drift_ppm: Option<f64>,
    pub error: Option<String>,
}

struct Shared {
    queue: Mutex<Resampler>,
    stop: AtomicBool,
    stats: Mutex<PlayerStats>,
    pushed: std::sync::atomic::AtomicU64,
}

pub struct AudioPlayer {
    shared: Arc<Shared>,
    max_queued: usize,
    thread: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Opens the default output and holds `target` of audio queued
    /// (device buffer included).
    pub fn start(target: Duration) -> Result<Self, String> {
        let target_frames = (target.as_secs_f64() * f64::from(SAMPLE_RATE)) as usize;
        let shared = Arc::new(Shared {
            queue: Mutex::new(Resampler::default()),
            stop: AtomicBool::new(false),
            stats: Mutex::new(PlayerStats {
                ratio_min: 1.0,
                ratio_max: 1.0,
                level_min_ms: f64::MAX,
                ..PlayerStats::default()
            }),
            pushed: Default::default(),
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("audio-render".into())
                .spawn(move || render(&shared, target_frames, &ready_tx))
                .map_err(|e| e.to_string())?
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                shared,
                // A burst can queue up to 4x the target; more is dropped.
                max_queued: target_frames * 4,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                shared.stop.store(true, Ordering::Relaxed);
                Err("audio playback did not start".into())
            }
        }
    }

    /// Queues interleaved stereo samples for playback.
    pub fn push(&self, interleaved: &[f32]) {
        let trimmed = {
            let mut queue = self.shared.queue.lock().unwrap();
            queue.push(interleaved);
            queue.trim_to(self.max_queued)
        };
        self.shared
            .pushed
            .fetch_add((interleaved.len() / CHANNELS) as u64, Ordering::Relaxed);
        if trimmed > 0 {
            self.shared.stats.lock().unwrap().trimmed_frames += trimmed as u64;
        }
    }

    pub fn stats(&self) -> PlayerStats {
        let mut stats = self.shared.stats.lock().unwrap().clone();
        stats.frames_pushed = self.shared.pushed.load(Ordering::Relaxed);
        stats
    }

    pub fn stop(mut self) -> PlayerStats {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.stats()
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn render(
    shared: &Shared,
    target_frames: usize,
    ready: &std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    // SAFETY: joins this thread to the MTA; balanced at the end.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let result = render_loop(shared, target_frames, ready);
    if let Err(error) = result {
        tracing::warn!(%error, "audio playback failed");
        let _ = ready.send(Err(error.clone()));
        shared.stats.lock().unwrap().error = Some(error);
    }
    if com {
        // SAFETY: balances CoInitializeEx; every COM object is gone.
        unsafe { CoUninitialize() };
    }
}

fn render_loop(
    shared: &Shared,
    target_frames: usize,
    ready: &std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let format = crate::capture::float_format();
    // SAFETY: COM calls on this MTA thread with valid arguments; the event
    // and the clients live until the end of this function.
    let (client, renderer, clock, event, buffer_frames) = unsafe {
        let devices: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = devices
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("no output device: {e}"))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| e.to_string())?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                200_000, // 20 ms
                0,
                &format,
                None,
            )
            .map_err(|e| format!("initialize: {e}"))?;
        let event = Event(CreateEventW(None, false, false, None).map_err(|e| e.to_string())?);
        client.SetEventHandle(event.0).map_err(|e| e.to_string())?;
        let buffer_frames = client.GetBufferSize().map_err(|e| e.to_string())?;
        let renderer: IAudioRenderClient = client.GetService().map_err(|e| e.to_string())?;
        let clock: IAudioClock = client.GetService().map_err(|e| e.to_string())?;
        client.Start().map_err(|e| e.to_string())?;
        (client, renderer, clock, event, buffer_frames)
    };
    let _ = ready.send(Ok(()));

    // SAFETY: valid clock.
    let device_frequency = unsafe { clock.GetFrequency() }.map_err(|e| e.to_string())?;
    let mut first_clock: Option<(u64, u64)> = None;
    let mut controller = DriftController::new(target_frames);
    let mut primed = false;
    let mut periods: u64 = 0;
    let ms_per_frame = 1000.0 / f64::from(SAMPLE_RATE);
    while !shared.stop.load(Ordering::Relaxed) {
        // SAFETY: valid event.
        unsafe { WaitForSingleObject(event.0, 100) };
        // SAFETY: valid client.
        let padding = unsafe { client.GetCurrentPadding() }.map_err(|e| e.to_string())?;
        let available = buffer_frames.saturating_sub(padding) as usize;
        if available == 0 {
            continue;
        }
        // SAFETY: asks for `available` frames, which fit in the buffer.
        let data = unsafe { renderer.GetBuffer(available as u32) }.map_err(|e| e.to_string())?;
        // SAFETY: WASAPI hands `available` frames of the format we set
        // (2 x f32) at `data`, writable until ReleaseBuffer.
        let out =
            unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), available * CHANNELS) };
        out.fill(0.0);
        let (level, ratio, written) = {
            let mut queue = shared.queue.lock().unwrap();
            let level = queue.queued() + padding as usize;
            if !primed && level >= target_frames {
                primed = true;
            }
            if primed {
                let ratio = controller.ratio(level);
                let written = queue.pull(out, ratio);
                (level, ratio, written)
            } else {
                (level, 1.0, available)
            }
        };
        // SAFETY: releases exactly the frames obtained above.
        unsafe { renderer.ReleaseBuffer(available as u32, 0) }.map_err(|e| e.to_string())?;
        periods += 1;
        let mut stats = shared.stats.lock().unwrap();
        stats.level_ms = level as f64 * ms_per_frame;
        if primed && written < available {
            // Ran dry: rebuild the queue before playing again.
            stats.underruns += 1;
            primed = false;
        }
        if primed && periods > 200 {
            stats.ratio_min = stats.ratio_min.min(ratio);
            stats.ratio_max = stats.ratio_max.max(ratio);
            stats.level_min_ms = stats.level_min_ms.min(stats.level_ms);
            stats.level_max_ms = stats.level_max_ms.max(stats.level_ms);
        }
        if periods.is_multiple_of(100) {
            let (mut position, mut at) = (0_u64, 0_u64);
            // SAFETY: valid clock; out-pointers are live locals.
            if unsafe { clock.GetPosition(&mut position, Some(&mut at)) }.is_ok() {
                // `at` is QPC in 100 ns units.
                match first_clock {
                    None => first_clock = Some((position, at)),
                    Some((p0, t0)) if at > t0 + 10_000_000 => {
                        let device_s = (position - p0) as f64 / device_frequency as f64;
                        let qpc_s = (at - t0) as f64 / 1e7;
                        stats.device_drift_ppm = Some((device_s / qpc_s - 1.0) * 1e6);
                    }
                    Some(_) => {}
                }
            }
        }
    }
    // SAFETY: valid client.
    let _ = unsafe { client.Stop() };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an audio output device (plays a quiet tone for 2 s); run with `--ignored`.
    #[test]
    #[ignore]
    fn a_tone_plays_without_underruns() {
        let player = AudioPlayer::start(Duration::from_millis(60)).unwrap();
        let started = std::time::Instant::now();
        for index in 0..100_u64 {
            let due = started + Duration::from_millis(20 * index);
            if let Some(wait) = due.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(wait);
            }
            player.push(&crate::capture::synthetic_frame(index + 1));
        }
        std::thread::sleep(Duration::from_millis(200));
        let stats = player.stop();
        println!("{stats:?}");
        assert!(stats.error.is_none());
        assert_eq!(stats.trimmed_frames, 0);
    }
}
