//! The host media pipeline.
//!
//! ```text
//! capture thread (WGC/DXGI) --newest frame wins--> encode thread
//!   encode thread: BGRA -> NV12 (shader) -> hardware encoder, output followed closely
//!   --bounded queue of access units--> consumer (file today, network later)
//! ```
//!
//! Every hand-off is bounded: the capture keeps one pending frame (a newer
//! one replaces it), the encode thread holds at most one frame the encoder
//! refused (a newer capture replaces it, so the picture stays fresh), the
//! encoder takes at most `max_in_flight` frames, and the output queue holds
//! `output_capacity` access units. When the consumer falls behind and the
//! queue is full, access units are dropped until the next keyframe (one is
//! requested at once), because a predictive stream cannot be decoded across
//! a gap.

mod thread;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use capture::{Capture, CaptureEnd, CaptureReport};
use encode::{EncodedChunk, EncoderConfig, EncoderPreference, EncoderSelection};
use gpu::convert::Nv12Image;
use gpu::{AdapterInfo, D3d11Device};
use serde::Serialize;
use telemetry::StageRecorder;

/// How far frames go (shorter runs isolate the cost of each stage).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Until {
    /// Capture and NV12 conversion only; converted frames are released.
    Convert,
    /// The whole pipeline.
    Encode,
}

#[derive(Clone, Copy, Debug)]
pub struct PipelineConfig {
    /// Encoded size, frame rate, bitrate, keyframe interval, frames in flight.
    pub encoder: EncoderConfig,
    pub encoder_preference: EncoderPreference,
    pub until: Until,
    /// Access units the output queue holds before dropping.
    pub output_capacity: usize,
    /// Run the encode thread below normal priority, so a CPU-bound game's
    /// threads win when they compete.
    pub low_priority: bool,
}

/// Counters updated by the encode thread.
#[derive(Debug, Default)]
struct Counters {
    frames_taken: AtomicU64,
    /// Refused frames replaced by a newer capture before they were encoded.
    held_replaced: AtomicU64,
    convert_dropped: AtomicU64,
    converted: AtomicU64,
    submitted: AtomicU64,
    busy: AtomicU64,
    access_units: AtomicU64,
    delivered: AtomicU64,
    output_full_dropped: AtomicU64,
    dropped_until_keyframe: AtomicU64,
    converter_resizes: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PipelineCounters {
    pub frames_taken: u64,
    pub held_replaced: u64,
    pub convert_dropped: u64,
    pub converted: u64,
    pub submitted: u64,
    pub busy: u64,
    pub access_units: u64,
    pub delivered: u64,
    pub output_full_dropped: u64,
    pub dropped_until_keyframe: u64,
    pub converter_resizes: u64,
}

impl Counters {
    fn snapshot(&self) -> PipelineCounters {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        PipelineCounters {
            frames_taken: read(&self.frames_taken),
            held_replaced: read(&self.held_replaced),
            convert_dropped: read(&self.convert_dropped),
            converted: read(&self.converted),
            submitted: read(&self.submitted),
            busy: read(&self.busy),
            access_units: read(&self.access_units),
            delivered: read(&self.delivered),
            output_full_dropped: read(&self.output_full_dropped),
            dropped_until_keyframe: read(&self.dropped_until_keyframe),
            converter_resizes: read(&self.converter_resizes),
        }
    }
}

/// Requests from the consumer to the encode thread.
#[derive(Debug)]
struct Control {
    stop: AtomicBool,
    keyframe: AtomicBool,
    /// 0 = no change requested.
    bitrate_bps: AtomicU32,
    counters: Counters,
    snapshot: Mutex<Snapshot>,
}

/// A still picture of the next captured frame, at a small size.
#[derive(Debug, Default)]
struct Snapshot {
    /// Requested size (width, height), until the encode thread takes it.
    wanted: Option<(u32, u32)>,
    /// The picture, or why it could not be taken.
    ready: Option<Result<Nv12Image, String>>,
}

/// Asks the encode thread for small still pictures of the capture (screen
/// previews); usable from any thread while the pipeline runs.
#[derive(Clone, Debug)]
pub struct Snapshots(Arc<Control>);

impl Snapshots {
    /// Asks for the next captured frame scaled to `width` x `height` (even,
    /// at most [`gpu::convert::MAX_IMAGE_PIXELS`]). A newer request replaces
    /// a pending one.
    pub fn request(&self, width: u32, height: u32) {
        self.0.snapshot.lock().expect("snapshot lock").wanted = Some((width, height));
    }

    /// The picture (or why it failed), once taken; then cleared.
    pub fn take(&self) -> Option<Result<Nv12Image, String>> {
        self.0.snapshot.lock().expect("snapshot lock").ready.take()
    }
}

/// What the pipeline measured. Stage samples: `Convert` = GPU time of the
/// conversion, `Submit` = CPU time of `submit`, `EncodeOutput` = submit to
/// access unit out.
#[derive(Debug)]
pub struct PipelineReport {
    pub selection: Option<EncoderSelection>,
    /// Why the capture ended, if it ended before `stop`.
    pub capture_end: Option<CaptureEnd>,
    pub capture: CaptureReport,
    pub counters: PipelineCounters,
    pub stages: StageRecorder,
    /// Access unit out minus the capture timestamp, signed QPC ticks (WGC
    /// timestamps run a few ms ahead, so this is not an exact latency).
    pub capture_to_output: Vec<i64>,
    /// Conversion GPU measurements skipped (timer slots busy).
    pub timer_skipped: u64,
    /// The error that stopped the encode thread, if any.
    pub error: Option<String>,
}

/// What `HostPipeline::recv` returned.
#[derive(Debug)]
pub enum Received {
    Chunk(EncodedChunk),
    Timeout,
    /// The encode thread ended (stopped, capture ended or failed).
    Finished,
}

pub struct HostPipeline {
    control: Arc<Control>,
    output: Receiver<EncodedChunk>,
    selection: Option<EncoderSelection>,
    thread: Option<JoinHandle<thread::Finished>>,
}

impl HostPipeline {
    /// Starts the encode thread on `capture` (already running on `device`,
    /// created on `adapter`). Returns once the encoder is ready.
    pub fn start(
        device: &D3d11Device,
        adapter: &AdapterInfo,
        capture: Capture,
        config: PipelineConfig,
    ) -> Result<Self, String> {
        if config.until == Until::Encode {
            config
                .encoder
                .validate()
                .map_err(|error| error.to_string())?;
        }
        if config.output_capacity == 0 {
            return Err("output capacity must be at least 1".to_owned());
        }
        let control = Arc::new(Control {
            stop: AtomicBool::new(false),
            keyframe: AtomicBool::new(false),
            bitrate_bps: AtomicU32::new(0),
            counters: Counters::default(),
            snapshot: Mutex::default(),
        });
        let (sender, output) = sync_channel(config.output_capacity);
        let (ready_sender, ready) = mpsc::sync_channel(1);
        let input = thread::Input {
            device: device.clone(),
            adapter: adapter.clone(),
            capture,
            config,
            control: Arc::clone(&control),
            output: sender,
        };
        let handle = std::thread::Builder::new()
            .name("encode".to_owned())
            .spawn(move || thread::run(input, ready_sender))
            .map_err(|error| format!("encode thread: {error}"))?;
        match ready.recv() {
            Ok(Ok(selection)) => Ok(Self {
                control,
                output,
                selection,
                thread: Some(handle),
            }),
            Ok(Err(message)) => {
                let _ = handle.join();
                Err(message)
            }
            Err(_) => {
                let _ = handle.join();
                Err("encode thread exited during start".to_owned())
            }
        }
    }

    /// The encoder in use (`None` with `Until::Convert`).
    pub fn selection(&self) -> Option<&EncoderSelection> {
        self.selection.as_ref()
    }

    /// Waits up to `timeout` for the next access unit.
    pub fn recv(&self, timeout: Duration) -> Received {
        match self.output.recv_timeout(timeout) {
            Ok(chunk) => Received::Chunk(chunk),
            Err(RecvTimeoutError::Timeout) => Received::Timeout,
            Err(RecvTimeoutError::Disconnected) => Received::Finished,
        }
    }

    /// Makes the next encoded frame a keyframe.
    pub fn request_keyframe(&self) {
        self.control.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn set_bitrate(&self, bitrate_bps: u32) {
        self.control
            .bitrate_bps
            .store(bitrate_bps.max(1), Ordering::Relaxed);
    }

    pub fn counters(&self) -> PipelineCounters {
        self.control.counters.snapshot()
    }

    /// Still pictures of the capture, for screen previews.
    pub fn snapshots(&self) -> Snapshots {
        Snapshots(Arc::clone(&self.control))
    }

    /// Stops the pipeline (finishing the frames in the encoder) and returns
    /// its report plus every access unit not yet received, in order.
    pub fn stop(mut self) -> (PipelineReport, Vec<EncodedChunk>) {
        let finished = self.join().expect("joined once");
        let mut remaining: Vec<EncodedChunk> = self.output.try_iter().collect();

        remaining.extend(finished.leftover);
        (finished.report, remaining)
    }

    fn join(&mut self) -> Option<thread::Finished> {
        let handle = self.thread.take()?;
        self.control.stop.store(true, Ordering::Relaxed);
        Some(
            handle
                .join()
                .unwrap_or_else(|_| thread::Finished::panicked()),
        )
    }
}

impl Drop for HostPipeline {
    fn drop(&mut self) {
        let _ = self.join();
    }
}
