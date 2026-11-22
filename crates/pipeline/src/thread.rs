//! The encode thread: takes the newest captured frame, converts it to NV12,
//! submits it to the encoder and follows the encoder's output closely.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

use capture::{Capture, CaptureEnd, CaptureReport, Taken};
use encode::{EncodedChunk, EncoderSelection, Submit, VideoEncoder, create_encoder};
use gpu::convert::{ConvertConfig, Converter, ShaderConverter, read_nv12_image};
use gpu::{AdapterInfo, D3d11Device, FrameFormat, GpuFrame, GpuTimer};
use telemetry::{QpcDuration, Stage, StageRecorder, qpc_frequency, qpc_now};
use windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

use crate::{Control, PipelineConfig, PipelineReport, Until};

/// While the encoder has work to follow, the loop checks for frames without
/// waiting and sleeps this long between polls of its output (std's sleep
/// uses a high-resolution timer on Windows; a condition variable timeout is
/// rounded to the system timer, which made the output late by milliseconds).
const FOLLOW_SLEEP: Duration = Duration::from_micros(200);
/// With nothing to follow it waits for the next frame (woken at once by it).
const IDLE_WAIT: Duration = Duration::from_millis(10);
/// NV12 textures: frames in the encoder plus one held and one converting.
const NV12_POOL: usize = 6;
const TIMER_SLOTS: usize = 32;
/// Bounded sample storage for the signed capture-to-output offsets.
const OFFSET_CAPACITY: usize = telemetry::DEFAULT_SAMPLE_CAPACITY;

pub(crate) struct Input {
    pub device: D3d11Device,
    pub adapter: AdapterInfo,
    pub capture: Capture,
    pub config: PipelineConfig,
    pub control: Arc<Control>,
    pub output: SyncSender<EncodedChunk>,
}

pub(crate) struct Finished {
    pub report: PipelineReport,
    /// Access units from the final drain that did not fit in the queue.
    pub leftover: Vec<EncodedChunk>,
}

impl Finished {
    pub fn panicked() -> Self {
        Self {
            report: PipelineReport {
                selection: None,
                capture_end: None,
                capture: CaptureReport {
                    backend: "unknown",
                    end: CaptureEnd::Failed("encode thread panicked".to_owned()),
                    counters: Default::default(),
                    cost: Vec::new(),
                    frame_time_offset: Vec::new(),
                    intervals: Vec::new(),
                    border_hidden: false,
                    min_update_interval_set: false,
                },
                counters: Default::default(),
                stages: StageRecorder::new(),
                capture_to_output: Vec::new(),
                timer_skipped: 0,
                error: Some("encode thread panicked".to_owned()),
            },
            leftover: Vec::new(),
        }
    }
}

/// Balances `CoInitializeEx` on this thread.
struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful CoInitializeEx in `run`.
        unsafe { CoUninitialize() };
    }
}

/// A frame waiting for the encoder: captured (not converted yet) or already
/// converted and refused once because the encoder was busy.
enum Held {
    Captured(GpuFrame),
    Converted(GpuFrame),
}

struct Worker {
    device: D3d11Device,
    context: ID3D11DeviceContext,
    config: PipelineConfig,
    control: Arc<Control>,
    output: SyncSender<EncodedChunk>,
    encoder: Option<Box<dyn VideoEncoder>>,
    converter: Option<ShaderConverter>,
    timer: GpuTimer,
    stages: StageRecorder,
    capture_to_output: Vec<i64>,
    held: Option<Held>,
    in_flight: usize,
    /// After a full output queue: drop access units until a keyframe.
    waiting_for_keyframe: bool,
    consumer_gone: bool,
    leftover: Vec<EncodedChunk>,
    /// Final drain: keep what does not fit instead of dropping it.
    draining: bool,
}

pub(crate) fn run(
    input: Input,
    ready: SyncSender<Result<Option<EncoderSelection>, String>>,
) -> Finished {
    let Input {
        device,
        adapter,
        capture,
        config,
        control,
        output,
    } = input;
    let fail_start = |message: String, capture: Capture| {
        let _ = ready.send(Err(message.clone()));
        let mut finished = Finished::panicked();
        finished.report.capture = capture.stop();
        finished.report.error = Some(message);
        finished
    };

    // SAFETY: joins the MTA for this thread; balanced by `Apartment`.
    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        return fail_start(format!("COM initialization: {error}"), capture);
    }
    let _apartment = Apartment;
    if config.low_priority {
        // SAFETY: pseudo-handle of the current thread; only its scheduling
        // priority changes (documented, per-thread, user mode).
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
    let _media_foundation = match config.until {
        Until::Encode => match mf::MediaFoundation::start() {
            Ok(started) => Some(started),
            Err(error) => return fail_start(format!("Media Foundation: {error}"), capture),
        },
        Until::Convert => None,
    };
    let context = match device.immediate_context() {
        Ok(context) => context,
        Err(error) => return fail_start(format!("device context: {error}"), capture),
    };
    let timer = match GpuTimer::new(&device, TIMER_SLOTS) {
        Ok(timer) => timer,
        Err(error) => return fail_start(format!("GPU timer: {error}"), capture),
    };
    let (encoder, selection) = match config.until {
        Until::Encode => {
            match create_encoder(&device, &adapter, config.encoder, config.encoder_preference) {
                Ok((encoder, selection)) => (Some(encoder), Some(selection)),
                Err(error) => return fail_start(error.to_string(), capture),
            }
        }
        Until::Convert => (None, None),
    };
    let _ = ready.send(Ok(selection.clone()));

    let mut worker = Worker {
        device,
        context,
        config,
        control,
        output,
        encoder,
        converter: None,
        timer,
        stages: StageRecorder::new(),
        capture_to_output: Vec::with_capacity(OFFSET_CAPACITY),
        held: None,
        in_flight: 0,
        waiting_for_keyframe: false,
        consumer_gone: false,
        leftover: Vec::new(),
        draining: false,
    };
    let (capture_end, error) = match worker.run_loop(&capture) {
        Ok(end) => (end, None),
        Err(error) => (None, Some(error)),
    };
    let error = error.or_else(|| worker.drain().err());
    worker.held = None;
    let capture = capture.stop();
    let gpu_times = worker.timer.finish(&worker.context);
    worker.record_gpu_times(&gpu_times);

    Finished {
        report: PipelineReport {
            selection,
            capture_end,
            capture,
            counters: worker.control.counters.snapshot(),
            timer_skipped: worker.timer.skipped(),
            stages: worker.stages,
            capture_to_output: worker.capture_to_output,
            error,
        },
        leftover: worker.leftover,
    }
}

impl Worker {
    /// Runs until stopped, the capture ends or the consumer goes away.
    fn run_loop(&mut self, capture: &Capture) -> Result<Option<CaptureEnd>, String> {
        let counters = Arc::clone(&self.control);
        let counters = &counters.counters;
        loop {
            if self.control.stop.load(Ordering::Relaxed) || self.consumer_gone {
                return Ok(None);
            }
            let following = self.held.is_some() || self.in_flight > 0;
            let wait = if following { Duration::ZERO } else { IDLE_WAIT };
            let mut new_frame = false;
            match capture.next(wait) {
                Taken::Value(frame) => {
                    new_frame = true;
                    counters.frames_taken.fetch_add(1, Ordering::Relaxed);
                    if self.held.replace(Held::Captured(frame)).is_some() {
                        counters.held_replaced.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Taken::Ended(end) => return Ok(Some(end)),
                Taken::Timeout => {}
            }
            self.apply_controls()?;
            if let Some(held) = self.held.take() {
                self.advance(held)?;
            }
            self.take_output()?;
            let gpu_times = self.timer.collect(&self.context);
            self.record_gpu_times(&gpu_times);
            if following && !new_frame {
                std::thread::sleep(FOLLOW_SLEEP);
            }
        }
    }

    fn apply_controls(&mut self) -> Result<(), String> {
        let Some(encoder) = self.encoder.as_mut() else {
            return Ok(());
        };
        if self.control.keyframe.swap(false, Ordering::Relaxed) {
            encoder.request_keyframe();
        }
        let bitrate = self.control.bitrate_bps.swap(0, Ordering::Relaxed);
        if bitrate > 0 && bitrate != encoder.config().bitrate_bps {
            encoder
                .set_bitrate(bitrate)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Converts (if needed) and submits a held frame; a refused frame is
    /// held again until a newer capture replaces it.
    fn advance(&mut self, held: Held) -> Result<(), String> {
        let control = Arc::clone(&self.control);
        let counters = &control.counters;
        let frame = match held {
            Held::Captured(captured) => {
                match self.take_snapshot(&captured).and(self.convert(&captured))? {
                    Some(frame) => frame,
                    None => {
                        counters.convert_dropped.fetch_add(1, Ordering::Relaxed);
                        return Ok(());
                    }
                }
            }
            Held::Converted(frame) => frame,
        };
        let Some(encoder) = self.encoder.as_mut() else {
            // Until::Convert: the converted frame is released here.
            return Ok(());
        };
        let started = qpc_now();
        let result = encoder.submit(frame).map_err(|error| error.to_string())?;
        self.stages.record(Stage::Submit, started, qpc_now());
        match result {
            Submit::Accepted => {
                counters.submitted.fetch_add(1, Ordering::Relaxed);
                self.in_flight += 1;
            }
            Submit::Busy(frame) => {
                counters.busy.fetch_add(1, Ordering::Relaxed);
                self.held = Some(Held::Converted(frame));
            }
        }
        Ok(())
    }

    /// If a still picture was asked for, scales this captured frame to the
    /// asked size and reads it back (a preview: small, rare). A failure only
    /// costs that picture.
    fn take_snapshot(&mut self, captured: &GpuFrame) -> Result<(), String> {
        let Some((width, height)) = self
            .control
            .snapshot
            .lock()
            .expect("snapshot lock")
            .wanted
            .take()
        else {
            return Ok(());
        };
        let config = ConvertConfig {
            input_width: captured.width,
            input_height: captured.height,
            output_width: width,
            output_height: height,
            pool_size: 1,
        };
        let picture = ShaderConverter::new(&self.device, config)
            .map_err(|e| e.to_string())
            .and_then(|mut converter| {
                let texture = converter
                    .convert(&captured.texture)
                    .map_err(|e| e.to_string())?
                    .ok_or("no texture free")?;
                read_nv12_image(&self.device, &texture).map_err(|e| e.to_string())
            });
        self.control.snapshot.lock().expect("snapshot lock").ready = Some(picture);
        Ok(())
    }

    /// Converts a captured frame to NV12 at the encoded size; `None` if
    /// every NV12 texture is in use. The captured texture is released when
    /// `captured` drops (the copy is already queued on the GPU).
    fn convert(&mut self, captured: &GpuFrame) -> Result<Option<GpuFrame>, String> {
        let wanted = (captured.width, captured.height);
        let current = self.converter.as_ref().map(|converter| {
            (
                converter.config().input_width,
                converter.config().input_height,
            )
        });
        if current != Some(wanted) {
            if current.is_some() {
                self.control
                    .counters
                    .converter_resizes
                    .fetch_add(1, Ordering::Relaxed);
            }
            let encoded = self.config.encoder;
            self.converter = Some(
                ShaderConverter::new(
                    &self.device,
                    ConvertConfig {
                        input_width: captured.width,
                        input_height: captured.height,
                        output_width: encoded.width,
                        output_height: encoded.height,
                        pool_size: NV12_POOL,
                    },
                )
                .map_err(|error| format!("converter: {error}"))?,
            );
        }
        let converter = self.converter.as_mut().expect("created above");
        let token = self.timer.begin(&self.context);
        let converted = converter
            .convert(&captured.texture)
            .map_err(|error| format!("conversion: {error}"))?;
        if let Some(token) = token {
            self.timer.end(&self.context, token);
        }
        // SAFETY: valid immediate context; submits the queued commands so
        // the GPU starts on them now rather than at the next implicit flush.
        unsafe { self.context.Flush() };
        let Some(texture) = converted else {
            return Ok(None);
        };
        self.control
            .counters
            .converted
            .fetch_add(1, Ordering::Relaxed);
        let encoded = self.config.encoder;
        Ok(Some(GpuFrame {
            texture,
            format: FrameFormat::Nv12,
            width: encoded.width,
            height: encoded.height,
            frame_id: captured.frame_id,
            captured_at: captured.captured_at,
        }))
    }

    fn take_output(&mut self) -> Result<(), String> {
        loop {
            let Some(encoder) = self.encoder.as_mut() else {
                return Ok(());
            };
            let Some(chunk) = encoder.poll().map_err(|error| error.to_string())? else {
                return Ok(());
            };
            self.in_flight = self.in_flight.saturating_sub(1);
            self.deliver(chunk);
        }
    }

    fn deliver(&mut self, chunk: EncodedChunk) {
        let counters = &self.control.counters;
        counters.access_units.fetch_add(1, Ordering::Relaxed);
        self.stages
            .record(Stage::EncodeOutput, chunk.submitted_at, chunk.output_at);
        if self.capture_to_output.len() < OFFSET_CAPACITY {
            self.capture_to_output
                .push(chunk.output_at.ticks() - chunk.captured_at.ticks());
        }
        if self.waiting_for_keyframe && !chunk.keyframe {
            counters
                .dropped_until_keyframe
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.waiting_for_keyframe = false;
        match self.output.try_send(chunk) {
            Ok(()) => {
                counters.delivered.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(chunk)) if self.draining => self.leftover.push(chunk),
            Err(TrySendError::Full(_)) => {
                counters.output_full_dropped.fetch_add(1, Ordering::Relaxed);
                self.waiting_for_keyframe = true;
                if let Some(encoder) = self.encoder.as_mut() {
                    encoder.request_keyframe();
                }
            }
            Err(TrySendError::Disconnected(_)) => self.consumer_gone = true,
        }
    }

    /// Finishes the frames in the encoder.
    fn drain(&mut self) -> Result<(), String> {
        self.draining = true;
        let Some(encoder) = self.encoder.as_mut() else {
            return Ok(());
        };
        let chunks = encoder.drain().map_err(|error| error.to_string())?;
        self.in_flight = 0;
        for chunk in chunks {
            self.deliver(chunk);
        }
        Ok(())
    }

    fn record_gpu_times(&mut self, micros: &[f64]) {
        let frequency = qpc_frequency();
        for &value in micros {
            self.stages
                .record_duration(Stage::Convert, QpcDuration::from_micros(value, frequency));
        }
    }
}
