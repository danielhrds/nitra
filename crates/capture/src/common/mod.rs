//! Shared by every capture backend: options, counters, the capture handle
//! and its thread, delivery into the texture pool, rate limiting and the
//! newest-wins hand-off to the consumer.
//!
//! A backend only waits for frames from its API and calls [`Delivery`] for
//! each one; everything else (thread, COM apartment, start-up handshake,
//! stopping, reports) lives here.

mod delivery;
mod limiter;
mod slot;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use gpu::{D3d11Device, GpuFrame};
use serde::Serialize;
use telemetry::QpcDuration;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoIncrementMTAUsage, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

pub(crate) use delivery::{Delivery, Samples};
pub use limiter::RateLimiter;
pub use slot::{LatestSlot, Taken};

#[derive(Clone, Copy, Debug)]
pub struct CaptureOptions {
    /// Maximum delivered frame rate.
    pub fps: u32,
    /// Draw the mouse cursor into frames (WGC only; DXGI never does).
    pub cursor: bool,
    /// Ask WGC not to draw its yellow capture border (WGC only).
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
    pub(crate) arrived: AtomicU64,
    pub(crate) superseded: AtomicU64,
    pub(crate) rate_dropped: AtomicU64,
    pub(crate) pool_dropped: AtomicU64,
    pub(crate) replaced: AtomicU64,
    pub(crate) delivered: AtomicU64,
    pub(crate) resizes: AtomicU64,
    pub(crate) access_lost: AtomicU64,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, amount: u64) {
        counter.fetch_add(amount, Ordering::Relaxed);
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
            access_lost: load(&self.access_lost),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CounterSnapshot {
    /// Frames the capture API handed to us.
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
    /// DXGI only: duplication lost (mode change, secure desktop) and recreated.
    pub access_lost: u64,
}

/// Optional API settings and whether the system accepted them.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AcceptedSettings {
    pub border_hidden: bool,
    pub min_update_interval_set: bool,
}

/// Everything measured during a capture, returned when it stops.
#[derive(Debug)]
pub struct CaptureReport {
    pub backend: &'static str,
    pub end: CaptureEnd,
    pub counters: CounterSnapshot,
    /// Capture thread CPU time per delivered frame (take, copy, release, hand off).
    pub cost: Vec<QpcDuration>,
    /// Hand-off time minus the API's frame timestamp, in signed QPC ticks.
    /// WGC's `SystemRelativeTime` measured about 3-5 ms in the future (it
    /// looks like DWM's target display time), so negative values are normal
    /// there; DXGI's `LastPresentTime` is a past present time. Neither is an
    /// end-to-end latency: that comes from frame stamps.
    pub frame_time_offset: Vec<i64>,
    /// Between consecutive deliveries.
    pub intervals: Vec<QpcDuration>,
    pub border_hidden: bool,
    pub min_update_interval_set: bool,
}

/// What a backend does on the capture thread.
pub(crate) trait Backend {
    /// Waits for and delivers frames until stopped, closed or failed.
    fn run(&mut self, context: &ThreadContext) -> CaptureEnd;
    /// Releases API objects (called once, after `run`).
    fn close(&mut self);
    fn into_results(self) -> (Samples, AcceptedSettings);
}

/// Shared state handed to a backend on the capture thread.
pub(crate) struct ThreadContext {
    pub device: D3d11Device,
    pub options: CaptureOptions,
    pub slot: Arc<LatestSlot<GpuFrame, CaptureEnd>>,
    pub counters: Arc<Counters>,
    /// Owned by `Capture`, which joins this thread before closing it.
    stop_event: isize,
}

impl ThreadContext {
    pub fn stop_handle(&self) -> HANDLE {
        HANDLE(self.stop_event as *mut _)
    }
}

/// A running capture. Dropping it stops the capture and waits for its thread.
pub struct Capture {
    backend: &'static str,
    slot: Arc<LatestSlot<GpuFrame, CaptureEnd>>,
    counters: Arc<Counters>,
    stop_event: OwnedEvent,
    thread: Option<JoinHandle<CaptureReport>>,
}

impl Capture {
    /// Waits up to `timeout` for the newest frame or the end of the capture.
    pub fn next(&self, timeout: Duration) -> Taken<GpuFrame, CaptureEnd> {
        self.slot.take(timeout)
    }

    pub fn counters(&self) -> CounterSnapshot {
        self.counters.snapshot()
    }

    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// Stops the capture and returns its measurements.
    pub fn stop(mut self) -> CaptureReport {
        self.stop_thread()
            .expect("capture thread runs until stopped")
    }

    fn stop_thread(&mut self) -> Option<CaptureReport> {
        let thread = self.thread.take()?;
        self.stop_event.set();
        Some(thread.join().unwrap_or_else(|_| CaptureReport {
            backend: self.backend,
            end: CaptureEnd::Failed("capture thread panicked".to_owned()),
            counters: self.counters.snapshot(),
            cost: Vec::new(),
            frame_time_offset: Vec::new(),
            intervals: Vec::new(),
            border_hidden: false,
            min_update_interval_set: false,
        }))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop_thread();
    }
}

/// Starts a capture thread for `backend`. `setup` runs on that thread (in
/// the MTA) and creates the backend; `spawn` returns once it succeeded or
/// failed. Enables the device's multithread protection, since the capture
/// API and the capture thread use it concurrently with other threads.
pub(crate) fn spawn<B, F>(
    backend: &'static str,
    device: &D3d11Device,
    options: CaptureOptions,
    setup: F,
) -> Result<Capture, String>
where
    B: Backend,
    F: FnOnce(&ThreadContext) -> windows::core::Result<B> + Send + 'static,
{
    keep_mta_alive().map_err(|error| format!("COM MTA usage: {error}"))?;
    device
        .enable_multithread_protection()
        .map_err(|error| format!("multithread protection: {error}"))?;
    let stop_event = OwnedEvent::new(true).map_err(|error| format!("stop event: {error}"))?;
    let slot = Arc::new(LatestSlot::new());
    let counters = Arc::new(Counters::default());
    let context = ThreadContext {
        device: device.clone(),
        options,
        slot: Arc::clone(&slot),
        counters: Arc::clone(&counters),
        stop_event: stop_event.raw(),
    };

    let (ready_sender, ready) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name(format!("{backend}-capture"))
        .spawn(move || run_thread(backend, context, setup, ready_sender))
        .map_err(|error| format!("capture thread: {error}"))?;

    match ready.recv() {
        Ok(Ok(())) => Ok(Capture {
            backend,
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

fn run_thread<B, F>(
    backend_name: &'static str,
    context: ThreadContext,
    setup: F,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> CaptureReport
where
    B: Backend,
    F: FnOnce(&ThreadContext) -> windows::core::Result<B>,
{
    let failed = |message: String| CaptureReport {
        backend: backend_name,
        end: CaptureEnd::Failed(message),
        counters: context.counters.snapshot(),
        cost: Vec::new(),
        frame_time_offset: Vec::new(),
        intervals: Vec::new(),
        border_hidden: false,
        min_update_interval_set: false,
    };

    // SAFETY: joins the MTA for this thread; balanced by `Apartment::drop`.
    let _apartment = match unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        Ok(()) => Apartment,
        Err(error) => {
            let message = format!("COM initialization: {error}");
            let _ = ready.send(Err(message.clone()));
            return failed(message);
        }
    };

    let mut backend = match setup(&context) {
        Ok(backend) => backend,
        Err(error) => {
            let message = format!("{backend_name} start: {error}");
            let _ = ready.send(Err(message.clone()));
            context.slot.end(CaptureEnd::Failed(message.clone()));
            return failed(message);
        }
    };
    let _ = ready.send(Ok(()));

    let end = backend.run(&context);
    backend.close();
    context.slot.end(end.clone());
    let (samples, settings) = backend.into_results();
    CaptureReport {
        backend: backend_name,
        end,
        counters: context.counters.snapshot(),
        cost: samples.cost,
        frame_time_offset: samples.frame_time_offset,
        intervals: samples.intervals,
        border_hidden: settings.border_hidden,
        min_update_interval_set: settings.min_update_interval_set,
    }
}

/// Keeps the process's multithreaded apartment alive until the process
/// exits, once per process.
///
/// Without it, when a capture thread was the last MTA member, its
/// `CoUninitialize` tore the MTA down and unloaded GraphicsCapture.dll while
/// a WGC worker thread was still running code from it (e.g. dispatching the
/// window-closed event): the process crashed with an access violation in
/// "GraphicsCapture.dll_unloaded" (about 1 in 10 window-close tests with
/// another capture running, Windows 11 25H2). `CoIncrementMTAUsage` is the
/// documented way for a library to keep the MTA alive; the usage is never
/// released on purpose (the cookie is dropped without `CoDecrementMTAUsage`).
fn keep_mta_alive() -> windows::core::Result<()> {
    static KEPT: AtomicBool = AtomicBool::new(false);
    if KEPT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // SAFETY: no pointers; the usage it adds lasts until process exit.
        if let Err(error) = unsafe { CoIncrementMTAUsage() } {
            KEPT.store(false, Ordering::Release);
            return Err(error);
        }
    }
    Ok(())
}

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
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
