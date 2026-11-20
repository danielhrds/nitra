//! The screens shown in the prototype: H.264 files from the lab, each read
//! by a thread of its own at its frame rate (looped), decoded by the
//! hardware decoder on the render thread (as a viewer would decode what
//! the network hands it).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decode::media_foundation::MediaFoundationDecoder;
use decode::{DecodeSubmit, DecodedFrame, VideoDecoder};
use gpu::D3d11Device;
use telemetry::qpc_now;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

/// Access units waiting for the decoder, per stream.
const QUEUE: usize = 4;

pub struct StreamSpec {
    pub file: PathBuf,
    pub fps: u32,
}

struct Shared {
    stop: AtomicBool,
    /// Auto-reset event: an access unit was queued (any stream).
    ready: isize,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: created in `Streams::start`, closed once with the last user.
        let _ = unsafe { CloseHandle(HANDLE(self.ready as *mut _)) };
    }
}

struct Stream {
    units: Receiver<Vec<u8>>,
    decoder: MediaFoundationDecoder,
    thread: Option<JoinHandle<()>>,
    active: Arc<AtomicBool>,
    /// Pictures decoded since the last resume.
    pub since_resume: u64,
    submitted: u64,
}

pub struct Streams {
    shared: Arc<Shared>,
    streams: Vec<Stream>,
}

impl Streams {
    /// Starts one reader per file; decoders live on this (render) thread,
    /// which must have joined COM and started Media Foundation.
    pub fn start(device: &D3d11Device, specs: &[StreamSpec]) -> Result<Self, String> {
        // SAFETY: unnamed auto-reset event, closed in `Shared::drop`.
        let ready = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared { stop: AtomicBool::new(false), ready: ready.0 as isize });
        let mut streams = Vec::new();
        for spec in specs {
            let stream = std::fs::read(&spec.file).map_err(|e| format!("{}: {e}", spec.file.display()))?;
            let units: Vec<Vec<u8>> = encode::annexb::access_units(&stream).into_iter().map(<[u8]>::to_vec).collect();
            if units.is_empty() {
                return Err(format!("no access units in {}", spec.file.display()));
            }
            let (sender, receiver) = sync_channel(QUEUE);
            let (shared_reader, fps) = (Arc::clone(&shared), spec.fps.max(1));
            let active = Arc::new(AtomicBool::new(false));
            let active_reader = Arc::clone(&active);
            let thread = std::thread::Builder::new()
                .name("stream-reader".into())
                .spawn(move || read(&units, fps, &sender, &shared_reader, &active_reader))
                .map_err(|e| e.to_string())?;
            streams.push(Stream {
                units: receiver,
                decoder: MediaFoundationDecoder::new(device).map_err(|e| format!("decoder: {e}"))?,
                thread: Some(thread),
                active,
                since_resume: 0,
                submitted: 0,
            });
        }
        Ok(Self { shared, streams })
    }

    /// Runs or pauses stream `index`. Pausing drops what is queued, so a
    /// resume starts clean at a keyframe.
    pub fn set_active(&mut self, index: usize, active: bool) {
        let stream = &mut self.streams[index];
        if stream.active.swap(active, Ordering::Relaxed) == active {
            return;
        }
        if active {
            stream.since_resume = 0;
        } else {
            while stream.units.try_recv().is_ok() {}
        }
    }

    pub fn since_resume(&self, index: usize) -> u64 {
        self.streams[index].since_resume
    }

    pub fn is_active(&self, index: usize) -> bool {
        self.streams[index].active.load(Ordering::Relaxed)
    }

    pub fn ready_event(&self) -> HANDLE {
        HANDLE(self.shared.ready as *mut _)
    }

    pub fn len(&self) -> usize {
        self.streams.len()
    }

    /// Decodes everything queued for stream `index`; returns the newest picture.
    pub fn poll(&mut self, index: usize) -> Result<Option<DecodedFrame>, String> {
        let stream = &mut self.streams[index];
        let mut newest = None;
        while let Ok(unit) = stream.units.try_recv() {
            let id = stream.submitted;
            stream.submitted += 1;
            while stream.decoder.submit(&unit, id, qpc_now()).map_err(|e| e.to_string())? == DecodeSubmit::Busy {
                while let Some(frame) = stream.decoder.poll().map_err(|e| e.to_string())? {
                    stream.since_resume += 1;
                    newest = Some(frame);
                }
            }
            while let Some(frame) = stream.decoder.poll().map_err(|e| e.to_string())? {
                stream.since_resume += 1;
                newest = Some(frame);
            }
        }
        Ok(newest)
    }

    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for stream in &mut self.streams {
            if let Some(thread) = stream.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read(units: &[Vec<u8>], fps: u32, sender: &SyncSender<Vec<u8>>, shared: &Shared, active: &AtomicBool) {
    let period = Duration::from_secs_f64(1.0 / f64::from(fps));
    let mut next = Instant::now();
    let mut index = 0;
    while !shared.stop.load(Ordering::Relaxed) {
        if !active.load(Ordering::Relaxed) {
            // Paused (screen not shown): restart from the first access unit
            // (a keyframe) when resumed.
            std::thread::sleep(Duration::from_millis(50));
            index = 0;
            next = Instant::now();
            continue;
        }
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        next += period;
        // Wait for room rather than drop: a dropped access unit breaks the
        // decoder's references until the next keyframe (smeared picture).
        let mut unit = units[index].clone();
        index = (index + 1) % units.len();
        loop {
            match sender.try_send(unit) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) => {
                    if shared.stop.load(Ordering::Relaxed) || !active.load(Ordering::Relaxed) {
                        break;
                    }
                    unit = back;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        // SAFETY: valid event owned by `shared`.
        let _ = unsafe { SetEvent(HANDLE(shared.ready as *mut _)) };
    }
}
