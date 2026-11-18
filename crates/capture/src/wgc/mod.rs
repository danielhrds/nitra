//! Windows.Graphics.Capture (WGC) for monitors and windows.
//!
//! A dedicated thread owns the WGC session. When WGC signals a new frame, the
//! thread takes the newest one, copies it into a texture from a small fixed
//! pool (GPU to GPU) and releases the WGC buffer at once, so WGC never waits
//! for consumers. Frames are delivered immediately, thinned to the target
//! rate by a [`RateLimiter`], through a [`LatestSlot`]: a slow consumer only
//! ever sees the newest frame. Every discarded frame is counted.

mod interop;
mod limiter;
mod session;
mod slot;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use gpu::{D3d11Device, GpuFrame};
use serde::Serialize;
use telemetry::QpcDuration;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

pub use interop::{find_window_by_title, timespan_to_qpc};
pub use limiter::RateLimiter;
pub use slot::{LatestSlot, Taken};

/// What to capture.
#[derive(Clone, Copy, Debug)]
pub enum CaptureTarget {
    Monitor(HMONITOR),
    Window(HWND),
}

#[derive(Clone, Copy, Debug)]
pub struct CaptureOptions {
    /// Maximum delivered frame rate.
    pub fps: u32,
    pub cursor: bool,
    /// Ask WGC not to draw its yellow capture border (Windows 11).
    pub hide_border: bool,
    /// Textures in the pool; frames are dropped when all are in use.
    pub pool_size: usize,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            fps: 60,
            cursor: true,
            hide_border: true,
            pool_size: 4,
        }
    }
}

/// Why a capture ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureEnd {
    Stopped,
    /// The window was closed (or the monitor disappeared).
    SourceClosed,
    Failed(String),
}

/// Live counters, updated by the capture thread.
#[derive(Debug, Default)]
pub struct Counters {
    arrived: AtomicU64,
    superseded: AtomicU64,
    rate_dropped: AtomicU64,
    pool_dropped: AtomicU64,
    replaced: AtomicU64,
    delivered: AtomicU64,
    resizes: AtomicU64,
}

impl Counters {
    fn add(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> CounterSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        CounterSnapshot {
            arrived: load(&self.arrived),
            superseded: load(&self.superseded),
            rate_dropped: load(&self.rate_dropped),
            pool_dropped: load(&self.pool_dropped),
            replaced: load(&self.replaced),
            delivered: load(&self.delivered),
            resizes: load(&self.resizes),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CounterSnapshot {
    /// Frames WGC handed to us.
    pub arrived: u64,
    /// Skipped because a newer frame was already waiting.
    pub superseded: u64,
    /// Skipped because they came too early for the target rate.
    pub rate_dropped: u64,
    /// Skipped because every pool texture was still held by consumers.
    pub pool_dropped: u64,
    /// Delivered but replaced by a newer frame before the consumer took them.
    pub replaced: u64,
    /// Copied and handed to the consumer slot.
    pub delivered: u64,
    pub resizes: u64,
}

/// Everything measured during a capture, returned when it stops.
#[derive(Debug)]
pub struct CaptureReport {
    pub end: CaptureEnd,
    pub counters: CounterSnapshot,
    /// Capture thread CPU time per delivered frame (take, copy, release, hand off).
    pub cost: Vec<QpcDuration>,
    /// Hand-off time minus WGC's `SystemRelativeTime`, in signed QPC ticks,
    /// per delivered frame. Measured negative (about -3.5 to -5.5 ms) on
    /// Windows 11 25H2: the timestamp looks like DWM's target display time
    /// of the composed frame, not when its content was produced, so this is
    /// not a capture latency. End-to-end latency uses frame stamps instead.
    pub frame_time_offset: Vec<i64>,
    /// Between consecutive deliveries.
    pub intervals: Vec<QpcDuration>,
    /// Whether WGC accepted each optional setting.
    pub border_hidden: bool,
    pub min_update_interval_set: bool,
}

/// A running capture. Dropping it stops the capture and waits for its thread.
pub struct WgcCapture {
    slot: Arc<LatestSlot<GpuFrame, CaptureEnd>>,
    counters: Arc<Counters>,
    stop_event: OwnedEvent,
    thread: Option<JoinHandle<CaptureReport>>,
}

impl WgcCapture {
    /// Starts capturing `target` with frames copied on `device`. Returns once
    /// WGC is running or failed to start. Enables the device's multithread
    /// protection, since WGC and the capture thread use it concurrently.
    pub fn start(
        device: &D3d11Device,
        target: CaptureTarget,
        options: CaptureOptions,
    ) -> Result<Self, String> {
        device
            .enable_multithread_protection()
            .map_err(|error| format!("multithread protection: {error}"))?;
        let stop_event = OwnedEvent::new(true).map_err(|error| format!("stop event: {error}"))?;

        let slot = Arc::new(LatestSlot::new());
        let counters = Arc::new(Counters::default());
        let (ready_sender, ready) = mpsc::sync_channel(1);
        let context = session::SessionContext {
            device: device.clone(),
            target: session::RawTarget::from(target),
            options,
            slot: Arc::clone(&slot),
            counters: Arc::clone(&counters),
            stop_event: stop_event.raw(),
        };
        let thread = std::thread::Builder::new()
            .name("wgc-capture".to_owned())
            .spawn(move || session::run(context, ready_sender))
            .map_err(|error| format!("capture thread: {error}"))?;

        match ready.recv() {
            Ok(Ok(())) => Ok(Self {
                slot,
                counters,
                stop_event,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(message)
            }
            Err(_) => {
                let _ = thread.join();
                Err("capture thread exited during start".to_owned())
            }
        }
    }

    /// Waits up to `timeout` for the newest frame or the end of the capture.
    pub fn next(&self, timeout: Duration) -> Taken<GpuFrame, CaptureEnd> {
        self.slot.take(timeout)
    }

    pub fn counters(&self) -> CounterSnapshot {
        self.counters.snapshot()
    }

    /// Stops the capture and returns its measurements.
    pub fn stop(mut self) -> CaptureReport {
        self.stop_thread()
            .expect("capture thread runs until stopped")
    }

    fn stop_thread(&mut self) -> Option<CaptureReport> {
        let thread = self.thread.take()?;
        self.stop_event.set();
        match thread.join() {
            Ok(report) => Some(report),
            Err(_) => Some(CaptureReport {
                end: CaptureEnd::Failed("capture thread panicked".to_owned()),
                counters: self.counters.snapshot(),
                cost: Vec::new(),
                frame_time_offset: Vec::new(),
                intervals: Vec::new(),
                border_hidden: false,
                min_update_interval_set: false,
            }),
        }
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        let _ = self.stop_thread();
    }
}

/// A Win32 event, closed on drop.
#[derive(Debug)]
pub(crate) struct OwnedEvent(HANDLE);

impl OwnedEvent {
    pub(crate) fn new(manual_reset: bool) -> windows::core::Result<Self> {
        // SAFETY: default security, initially unsignaled, unnamed.
        let handle = unsafe { CreateEventW(None, manual_reset, false, None) }?;
        Ok(Self(handle))
    }

    /// The handle value, for code that does not outlive this event.
    pub(crate) fn raw(&self) -> isize {
        self.0.0 as isize
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }

    pub(crate) fn set(&self) {
        // SAFETY: valid event handle owned by `self`.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: Win32 event handles may be used from any thread.
unsafe impl Send for OwnedEvent {}
unsafe impl Sync for OwnedEvent {}
