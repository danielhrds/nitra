//! Audio capture into stamped 20 ms frames on a thread of its own, handed
//! over through a bounded queue (a full queue drops the newest frame: audio
//! capture never waits for the consumer).
//!
//! Sources:
//! - `System`: everything playing on the default output, except this
//!   process tree (so a host that also watches does not capture what it
//!   plays back: no echo).
//! - `App(pid)`: only that process tree (sharing one app's window).
//! - `Synthetic`: a 440 Hz tone with a 1 kHz click every second on a whole
//!   20 ms frame, for tests and A/V sync measurements.
//!
//! The first two use WASAPI process loopback (Windows 10 2004+), which
//! takes the requested format (48 kHz stereo float) and converts for us.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use telemetry::{QpcTimestamp, qpc_frequency, qpc_now};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, Ref};
use windows_core::implement;

use crate::frames::{AudioFrame, Framer};
use crate::opus::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

/// Frames waiting for the consumer (1 s).
const QUEUE_FRAMES: usize = 50;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    System,
    App(u32),
    Synthetic,
}

#[derive(Clone, Debug, Default)]
pub struct CaptureReport {
    pub frames: u64,
    /// Frames dropped because the consumer was behind.
    pub dropped: u64,
    /// Timeline restarts (gaps, discontinuities).
    pub restarts: u64,
    pub error: Option<String>,
}

pub struct AudioCapture {
    frames: Receiver<AudioFrame>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<CaptureReport>>,
}

impl AudioCapture {
    /// Starts capturing; fails if the source cannot be opened.
    pub fn start(source: CaptureSource) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel(QUEUE_FRAMES);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || match source {
                    CaptureSource::Synthetic => {
                        let _ = ready_tx.send(Ok(()));
                        synthetic(&tx, &stop)
                    }
                    CaptureSource::System => loopback(
                        std::process::id(),
                        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                        &tx,
                        &stop,
                        &ready_tx,
                    ),
                    CaptureSource::App(pid) => loopback(
                        pid,
                        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                        &tx,
                        &stop,
                        &ready_tx,
                    ),
                })
                .map_err(|e| e.to_string())?
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                frames: rx,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("audio capture did not start".into())
            }
        }
    }

    /// The next frame, or `None` after `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<AudioFrame> {
        self.frames.recv_timeout(timeout).ok()
    }

    pub fn stop(mut self) -> CaptureReport {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn hand_over(tx: &SyncSender<AudioFrame>, frame: AudioFrame, report: &mut CaptureReport) -> bool {
    match tx.try_send(frame) {
        Ok(()) => report.frames += 1,
        Err(TrySendError::Full(_)) => report.dropped += 1,
        Err(TrySendError::Disconnected(_)) => return false,
    }
    true
}

/// The synthetic source: one frame every 20 ms of the QPC clock.
fn synthetic(tx: &SyncSender<AudioFrame>, stop: &AtomicBool) -> CaptureReport {
    let mut report = CaptureReport::default();
    let frequency = qpc_frequency().ticks_per_second() as i64;
    let start = qpc_now();
    let started = Instant::now();
    let mut index: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        let due = started + Duration::from_millis(20 * index);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let at = QpcTimestamp::from_ticks(start.ticks() + index as i64 * frequency / 50);
        let frame = AudioFrame {
            samples: synthetic_frame(index),
            captured_at: at,
        };
        if !hand_over(tx, frame, &mut report) {
            break;
        }
        index += 1;
    }
    report
}

/// Frame `index` of the synthetic signal: a quiet 440 Hz tone, and a loud
/// 1 kHz click on every 50th frame (once a second).
pub fn synthetic_frame(index: u64) -> Vec<f32> {
    let click = index.is_multiple_of(50);
    (0..FRAME_SAMPLES)
        .flat_map(|i| {
            let n = index * FRAME_SAMPLES as u64 + i as u64;
            let t = n as f32 / SAMPLE_RATE as f32;
            let (freq, amplitude) = if click { (1000.0, 0.8) } else { (440.0, 0.1) };
            let s = amplitude * (2.0 * std::f32::consts::PI * freq * t).sin();
            [s; CHANNELS]
        })
        .collect()
}

/// Signals an event when the activation completes (any thread).
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // SAFETY: the event outlives the activation (waited on before closing).
        unsafe { SetEvent(self.0) }
    }
}

/// An owned event handle, closed on drop.
struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self, String> {
        // SAFETY: plain auto-reset event, owned by the result.
        unsafe { CreateEventW(None, false, false, None) }
            .map(Self)
            .map_err(|e| e.to_string())
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(crate) fn float_format() -> WAVEFORMATEX {
    let block = (CHANNELS * 4) as u16;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * u32::from(block),
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// Opens a process loopback client for `pid` in `mode`.
fn open_process_loopback(pid: u32, mode: PROCESS_LOOPBACK_MODE) -> Result<IAudioClient, String> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    };
    // A VT_BLOB pointing at `params`. Nothing in it is owned: it must never
    // be cleared (windows-rs drops a PROPVARIANT with PropVariantClear, which
    // would free `params`, a stack value), hence `ManuallyDrop`.
    let variant = std::mem::ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw const params).cast_mut().cast(),
                    },
                },
            }),
        },
    });
    let done = Event::new()?;
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done.0).into();
    // SAFETY: `params` and `variant` live until the activation completes
    // (waited on below); the handler is a COM object kept alive by the call.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&raw const *variant),
            &handler,
        )
    }
    .map_err(|e| format!("process loopback activation: {e}"))?;
    // SAFETY: valid event handle.
    if unsafe { WaitForSingleObject(done.0, 5000) } != WAIT_OBJECT_0 {
        return Err("process loopback activation timed out".into());
    }
    let mut result = windows::core::HRESULT(0);
    let mut client = None;
    // SAFETY: the operation completed; both out-pointers are live locals.
    unsafe { operation.GetActivateResult(&mut result, &mut client) }.map_err(|e| e.to_string())?;
    result.ok().map_err(|e| format!("process loopback: {e}"))?;
    client
        .ok_or("no audio client")?
        .cast::<IAudioClient>()
        .map_err(|e| e.to_string())
}

/// Process loopback capture until `stop`.
fn loopback(
    pid: u32,
    mode: PROCESS_LOOPBACK_MODE,
    tx: &SyncSender<AudioFrame>,
    stop: &AtomicBool,
    ready: &SyncSender<Result<(), String>>,
) -> CaptureReport {
    let mut report = CaptureReport::default();
    // SAFETY: joins this thread to the MTA; balanced at the end.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let result = (|| -> Result<(), String> {
        let client = open_process_loopback(pid, mode)?;
        let format = float_format();
        let event = Event::new()?;
        // SAFETY: valid client and format; 200 ms buffer, shared mode.
        let capture = unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    2_000_000,
                    0,
                    &format,
                    None,
                )
                .map_err(|e| format!("initialize: {e}"))?;
            client.SetEventHandle(event.0).map_err(|e| e.to_string())?;
            let capture: IAudioCaptureClient = client.GetService().map_err(|e| e.to_string())?;
            client.Start().map_err(|e| e.to_string())?;
            capture
        };
        let _ = ready.send(Ok(()));
        let frequency = qpc_frequency();
        let mut framer = Framer::new(frequency);
        while !stop.load(Ordering::Relaxed) {
            // SAFETY: valid event handle.
            unsafe { WaitForSingleObject(event.0, 100) };
            loop {
                // SAFETY: valid capture client.
                let next = unsafe { capture.GetNextPacketSize() }.map_err(|e| e.to_string())?;
                if next == 0 {
                    break;
                }
                let (mut data, mut count, mut flags, mut qpc) =
                    (std::ptr::null_mut(), 0_u32, 0_u32, 0_u64);
                // SAFETY: valid client; out-pointers are live locals.
                unsafe {
                    capture.GetBuffer(&mut data, &mut count, &mut flags, None, Some(&mut qpc))
                }
                .map_err(|e| e.to_string())?;
                let samples = count as usize * CHANNELS;
                let pcm: Vec<f32> =
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        vec![0.0; samples]
                    } else {
                        // SAFETY: WASAPI hands `count` frames of the requested
                        // format (2 x f32) at `data`, valid until ReleaseBuffer.
                        unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) }.to_vec()
                    };
                // SAFETY: releases exactly what GetBuffer handed out.
                unsafe { capture.ReleaseBuffer(count) }.map_err(|e| e.to_string())?;
                // The QPC position is in 100 ns units.
                let at = QpcTimestamp::from_ticks(
                    (u128::from(qpc) * u128::from(frequency.ticks_per_second()) / 10_000_000)
                        as i64,
                );
                let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                for frame in framer.push(&pcm, at, discontinuity) {
                    if !hand_over(tx, frame, &mut report) {
                        return Ok(());
                    }
                }
            }
        }
        report.restarts = framer.restarts;
        // SAFETY: valid client.
        let _ = unsafe { client.Stop() };
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "audio capture failed");
        let _ = ready.send(Err(error.clone()));
        report.error = Some(error);
    }
    if com {
        // SAFETY: balances the CoInitializeEx above; every COM object is gone.
        unsafe { CoUninitialize() };
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_synthetic_source_paces_and_stamps_frames() {
        let capture = AudioCapture::start(CaptureSource::Synthetic).unwrap();
        let mut frames = Vec::new();
        let started = Instant::now();
        while frames.len() < 25 {
            frames.extend(capture.recv_timeout(Duration::from_millis(100)));
            assert!(started.elapsed() < Duration::from_secs(3));
        }
        let report = capture.stop();
        // 25 frames take ~480 ms (the first is immediate).
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
        let frequency = qpc_frequency().ticks_per_second() as i64;
        for pair in frames.windows(2) {
            let step = pair[1].captured_at.ticks() - pair[0].captured_at.ticks();
            assert_eq!(step, frequency / 50);
        }
        assert_eq!(report.dropped, 0);
    }

    #[test]
    fn the_click_is_on_every_50th_frame() {
        let loud = |f: &[f32]| f.iter().map(|s| s.abs()).fold(0.0, f32::max) > 0.5;
        assert!(loud(&synthetic_frame(0)));
        assert!(!loud(&synthetic_frame(1)));
        assert!(loud(&synthetic_frame(50)));
    }

    /// Needs an audio output device; run with `--ignored`.
    #[test]
    #[ignore]
    fn system_loopback_captures_frames() {
        let capture = AudioCapture::start(CaptureSource::System).unwrap();
        let mut n = 0;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(1) {
            n += usize::from(capture.recv_timeout(Duration::from_millis(100)).is_some());
        }
        let report = capture.stop();
        println!("{n} frames in 1 s, {report:?}");
        assert!(report.error.is_none());
    }
}
