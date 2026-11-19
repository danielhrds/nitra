//! The video both UI spikes show, so they are compared on the same work:
//!
//! ```text
//! encode thread: synthetic frames (paced) -> NV12 (shader) -> hardware encoder
//!   --bounded queue of access units (event signaled on each)-->
//! consumer thread: hardware decoder -> newest decoded frame
//! ```
//!
//! Same building blocks as the real host and viewer (C1/C2), without capture
//! or network. The consumer thread must have joined the COM multithreaded
//! apartment and started Media Foundation (see [`MediaThread`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use capture::synthetic::{Pacer, PatternMode, SyntheticConfig, SyntheticSource};
use decode::media_foundation::MediaFoundationDecoder;
use decode::{DecodeSubmit, DecodedFrame, VideoDecoder};
use encode::{EncodedChunk, EncoderConfig, EncoderPreference, EncoderSelection, Submit};
use gpu::convert::{ConvertConfig, Converter, ShaderConverter};
use gpu::{AdapterInfo, D3d11Device, FrameFormat, GpuFrame};
use telemetry::qpc_now;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

/// Access units waiting for the consumer before the encode thread drops.
const QUEUE: usize = 8;

/// Milliseconds since this process was created (for start-up times).
pub fn ms_since_process_start() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: pseudo-handle of the current process; live out-pointers.
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .is_err()
    {
        return f64::NAN;
    }
    // SAFETY: plain query.
    let now = unsafe { GetSystemTimePreciseAsFileTime() };
    // FILETIME counts 100 ns units.
    ticks(now).saturating_sub(ticks(created)) as f64 / 10_000.0
}

/// COM (multithreaded apartment) and Media Foundation for the calling
/// thread. Must be dropped on that thread, after every COM object; fields
/// drop in order, so Media Foundation shuts down before COM is left.
pub struct MediaThread {
    _media_foundation: mf::MediaFoundation,
    _com: ComApartment,
}

impl MediaThread {
    pub fn start() -> Result<Self, String> {
        let com = ComApartment::multithreaded()?;
        let media_foundation =
            mf::MediaFoundation::start().map_err(|e| format!("Media Foundation: {e}"))?;
        Ok(Self {
            _media_foundation: media_foundation,
            _com: com,
        })
    }
}

/// The calling thread's membership of the COM multithreaded apartment.
pub struct ComApartment(());

impl ComApartment {
    pub fn multithreaded() -> Result<Self, String> {
        // SAFETY: no reserved pointer; balanced by CoUninitialize in Drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| format!("COM: {e}"))?;
        Ok(Self(()))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FeedConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 10_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeedCounters {
    pub encoded: u64,
    /// Access units dropped because the consumer fell behind (until the
    /// next keyframe).
    pub dropped: u64,
    pub decoded: u64,
    /// Decoded frames released unseen because a newer one was ready.
    pub superseded: u64,
}

struct Shared {
    stop: AtomicBool,
    encoded: AtomicU64,
    dropped: AtomicU64,
    /// Auto-reset event set for every queued access unit.
    ready: isize,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: created in `Feed::start`, closed once when the last user
        // (consumer or encode thread) is gone.
        let _ = unsafe { CloseHandle(HANDLE(self.ready as *mut _)) };
    }
}

pub struct Feed {
    shared: Arc<Shared>,
    units: Receiver<EncodedChunk>,
    decoder: MediaFoundationDecoder,
    selection: EncoderSelection,
    decoded: u64,
    superseded: u64,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl Feed {
    /// Starts the encode thread and the decoder (on this thread, which must
    /// be a [`MediaThread`]).
    pub fn start(
        device: &D3d11Device,
        adapter: &AdapterInfo,
        config: FeedConfig,
    ) -> Result<Self, String> {
        // SAFETY: unnamed auto-reset event, closed in `Shared::drop`.
        let ready = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            encoded: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            ready: ready.0 as isize,
        });
        let decoder = MediaFoundationDecoder::new(device).map_err(|e| format!("decoder: {e}"))?;
        let (sender, units) = sync_channel(QUEUE);
        let (started_sender, started) = mpsc::sync_channel(1);
        let thread = {
            let (adapter, shared) = (adapter.clone(), Arc::clone(&shared));
            std::thread::Builder::new()
                .name("feed-encode".to_owned())
                .spawn(move || encode_thread(adapter, config, shared, sender, started_sender))
                .map_err(|e| e.to_string())?
        };
        match started.recv() {
            Ok(Ok(selection)) => Ok(Self {
                shared,
                units,
                decoder,
                selection,
                decoded: 0,
                superseded: 0,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(message)
            }
            Err(_) => {
                let _ = thread.join();
                Err("encode thread exited during start".to_owned())
            }
        }
    }

    pub fn encoder(&self) -> &EncoderSelection {
        &self.selection
    }

    /// Signaled (auto-reset) whenever an access unit is queued: wait on it
    /// to sleep until there is something to decode.
    pub fn ready_event(&self) -> HANDLE {
        HANDLE(self.shared.ready as *mut _)
    }

    /// Decodes every queued access unit and returns the newest picture, if
    /// any (older ones are released at once). Never blocks.
    pub fn poll(&mut self) -> Result<Option<DecodedFrame>, String> {
        let mut newest = None;
        let mut keep = |frame: DecodedFrame, newest: &mut Option<DecodedFrame>| {
            self.decoded += 1;
            if newest.replace(frame).is_some() {
                self.superseded += 1;
            }
        };
        while let Ok(unit) = self.units.try_recv() {
            while self
                .decoder
                .submit(&unit.data, unit.frame_id, unit.captured_at)
                .map_err(|e| format!("decode: {e}"))?
                == DecodeSubmit::Busy
            {
                while let Some(frame) = self.decoder.poll().map_err(|e| e.to_string())? {
                    keep(frame, &mut newest);
                }
            }
            while let Some(frame) = self.decoder.poll().map_err(|e| e.to_string())? {
                keep(frame, &mut newest);
            }
        }
        Ok(newest)
    }

    pub fn counters(&self) -> FeedCounters {
        FeedCounters {
            encoded: self.shared.encoded.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            decoded: self.decoded,
            superseded: self.superseded,
        }
    }

    /// Stops the encode thread; returns its error, if it failed.
    pub fn stop(mut self) -> Result<(), String> {
        self.join()
    }

    fn join(&mut self) -> Result<(), String> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        self.shared.stop.store(true, Ordering::Relaxed);
        thread
            .join()
            .unwrap_or_else(|_| Err("encode thread panicked".to_owned()))
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

fn encode_thread(
    adapter: AdapterInfo,
    config: FeedConfig,
    shared: Arc<Shared>,
    units: SyncSender<EncodedChunk>,
    started: mpsc::SyncSender<Result<EncoderSelection, String>>,
) -> Result<(), String> {
    let _media = match MediaThread::start() {
        Ok(media) => media,
        Err(message) => {
            let _ = started.send(Err(message.clone()));
            return Err(message);
        }
    };
    let setup = (|| -> Result<_, String> {
        // A device of its own on the same GPU (the host side): it renders
        // and converts with the pipeline state of its immediate context,
        // which must not interleave with the consumer's drawing on another
        // device (multithread protection serializes calls, not sequences).
        let adapters = gpu::enumerate_adapters().map_err(|e| e.to_string())?;
        let gpu_adapter = adapters
            .iter()
            .find(|candidate| candidate.info.luid == adapter.luid)
            .ok_or("feed adapter not found")?;
        let device = gpu::create_device(gpu_adapter, gpu::DebugLayer::Off)
            .map_err(|e| format!("feed device: {e}"))?;
        let source = SyntheticSource::new(
            &device,
            SyntheticConfig {
                width: config.width,
                height: config.height,
                mode: PatternMode::Motion,
            },
        )
        .map_err(|e| format!("synthetic source: {e}"))?;
        let converter = ShaderConverter::new(
            &device,
            ConvertConfig {
                input_width: config.width,
                input_height: config.height,
                output_width: config.width,
                output_height: config.height,
                pool_size: 6,
            },
        )
        .map_err(|e| format!("converter: {e}"))?;
        let (encoder, selection) = encode::create_encoder(
            &device,
            &adapter,
            EncoderConfig {
                width: config.width,
                height: config.height,
                fps: config.fps,
                bitrate_bps: config.bitrate_bps,
                keyframe_interval: config.fps * 10,
                max_in_flight: 4,
            },
            EncoderPreference::Auto,
        )
        .map_err(|e| format!("encoder: {e}"))?;
        Ok((source, converter, encoder, selection))
    })();
    let (mut source, mut converter, mut encoder, selection) = match setup {
        Ok(parts) => parts,
        Err(message) => {
            let _ = started.send(Err(message.clone()));
            return Err(message);
        }
    };
    let _ = started.send(Ok(selection));

    let ready = HANDLE(shared.ready as *mut _);
    let mut skipping_to_keyframe = false;
    let mut deliver = |chunk: EncodedChunk, encoder: &mut Box<dyn encode::VideoEncoder>| {
        shared.encoded.fetch_add(1, Ordering::Relaxed);
        if skipping_to_keyframe && !chunk.keyframe {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match units.try_send(chunk) {
            Ok(()) => {
                skipping_to_keyframe = false;
                // SAFETY: valid event owned by `shared`.
                let _ = unsafe { SetEvent(ready) };
            }
            Err(TrySendError::Full(_)) => {
                // The consumer fell behind: a predictive stream cannot skip
                // frames, so drop until a fresh keyframe.
                shared.dropped.fetch_add(1, Ordering::Relaxed);
                skipping_to_keyframe = true;
                encoder.request_keyframe();
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    };
    let mut pacer = Pacer::start_now(config.fps);
    while !shared.stop.load(Ordering::Relaxed) {
        let slot = pacer.next_slot(qpc_now());
        pacer.wait(&slot);
        let synthetic = source.render_next().map_err(|e| e.to_string())?;
        let Some(texture) = converter
            .convert(&synthetic.texture)
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        let frame = GpuFrame {
            texture,
            format: FrameFormat::Nv12,
            width: config.width,
            height: config.height,
            frame_id: u64::from(synthetic.stamp.frame_id),
            captured_at: synthetic.stamp.timestamp,
        };
        // A busy encoder hands the frame back; it is simply skipped.
        if let Submit::Busy(_) = encoder.submit(frame).map_err(|e| e.to_string())? {}
        // Follow the output closely until the next frame is due.
        let next = pacer.deadline(slot.index + 1);
        loop {
            while let Some(chunk) = encoder.poll().map_err(|e| e.to_string())? {
                deliver(chunk, &mut encoder);
            }
            if qpc_now() >= next || shared.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_micros(250));
        }
    }
    Ok(())
}

/// What both prototypes show: the same fictitious members and settings.
pub mod sample {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Presence {
        Live,
        Playing,
        Online,
        Away,
        Offline,
    }

    impl Presence {
        pub fn label(self) -> &'static str {
            match self {
                Presence::Live => "Live",
                Presence::Playing => "Playing",
                Presence::Online => "Online",
                Presence::Away => "Away",
                Presence::Offline => "Offline",
            }
        }

        /// RGB, 0..1.
        pub fn color(self) -> [f32; 3] {
            match self {
                Presence::Live => [0.93, 0.26, 0.27],
                Presence::Playing => [0.35, 0.40, 0.95],
                Presence::Online => [0.23, 0.75, 0.40],
                Presence::Away => [0.95, 0.70, 0.20],
                Presence::Offline => [0.50, 0.50, 0.55],
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    pub struct Member {
        pub name: &'static str,
        pub presence: Presence,
        pub activity: &'static str,
    }

    const fn member(name: &'static str, presence: Presence, activity: &'static str) -> Member {
        Member {
            name,
            presence,
            activity,
        }
    }

    /// 20 fictitious members.
    pub const MEMBERS: [Member; 20] = [
        member("Aurora", Presence::Live, "Streaming: ranked match"),
        member("Bento", Presence::Playing, "In a match"),
        member("Caio", Presence::Online, ""),
        member("Dalila", Presence::Live, "Streaming: practice range"),
        member("Enzo", Presence::Away, "Idle 12 min"),
        member("Flora", Presence::Online, ""),
        member("Gael", Presence::Playing, "In the lobby"),
        member("Helena", Presence::Offline, ""),
        member("Igor", Presence::Online, ""),
        member("Jade", Presence::Playing, "In a match"),
        member("Kauã", Presence::Offline, ""),
        member("Lara", Presence::Away, "Idle 3 min"),
        member("Miguel", Presence::Live, "Streaming: custom game"),
        member("Nina", Presence::Online, ""),
        member("Otto", Presence::Offline, ""),
        member("Pietra", Presence::Playing, "In a match"),
        member("Rafa", Presence::Online, ""),
        member("Sofia", Presence::Offline, ""),
        member("Theo", Presence::Away, "Idle 40 min"),
        member("Valentina", Presence::Online, ""),
    ];

    pub const RESOLUTIONS: [&str; 3] = ["1280x720", "1600x900", "1920x1080"];
    pub const FRAME_RATES: [&str; 2] = ["30 fps", "60 fps"];
    pub const ENCODERS: [&str; 3] = ["Automatic", "AMF", "Media Foundation"];

    /// The settings screen's state (not applied: the prototypes only show it).
    #[derive(Clone, Copy, Debug)]
    pub struct Settings {
        pub resolution: usize,
        pub frame_rate: usize,
        pub encoder: usize,
        pub bitrate_kbps: i32,
        pub show_cursor: bool,
        pub start_with_windows: bool,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                resolution: 2,
                frame_rate: 1,
                encoder: 0,
                bitrate_kbps: 10_000,
                show_cursor: true,
                start_with_windows: false,
            }
        }
    }
}

/// Checks, from a WGC capture of a window showing the feed letterboxed,
/// whether each picture on screen keeps the video's aspect ratio: a point in
/// the letterbox bar (away from the top-left overlay) must be black. Used to
/// count wrongly scaled frames while the window is being resized. Reads one
/// pixel per capture (test-only readback).
pub mod resize_probe {
    use std::time::Duration;

    use capture::wgc::CaptureTarget;
    use capture::{Capture, CaptureOptions, Taken};
    use gpu::D3d11Device;
    use windows::Win32::Foundation::HWND;

    #[derive(Clone, Copy, Debug, Default)]
    pub struct ProbeCounts {
        /// Captures with a bar where one was expected.
        pub good: u64,
        /// Captures with picture where the bar should be (wrongly scaled).
        pub bad: u64,
        /// Captures with no bar to check (window close to 16:9).
        pub skipped: u64,
    }

    pub struct ResizeProbe {
        device: D3d11Device,
        capture: Capture,
        aspect: f64,
        pub counts: ProbeCounts,
    }

    impl ResizeProbe {
        pub fn start(
            device: &D3d11Device,
            window: HWND,
            video: (u32, u32),
        ) -> Result<Self, String> {
            let capture = capture::wgc::start(
                device,
                CaptureTarget::Window(window),
                CaptureOptions {
                    fps: 240,
                    cursor: false,
                    hide_border: true,
                    ..CaptureOptions::default()
                },
            )
            .map_err(|e| e.to_string())?;
            Ok(Self {
                device: device.clone(),
                capture,
                aspect: f64::from(video.0) / f64::from(video.1),
                counts: ProbeCounts::default(),
            })
        }

        /// Prints a coarse map of a capture: # picture, . black.
        fn dump(
            &self,
            texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
            w: u32,
            h: u32,
        ) {
            let (cols, rows) = (32, 16);
            println!("bad capture {w}x{h}:");
            for row in 0..rows {
                let line: String = (0..cols)
                    .map(|col| {
                        let x = (col * 2 + 1) * w / (cols * 2);
                        let y = (row * 2 + 1) * h / (rows * 2);
                        match gpu::read_pixel(&self.device, texture, x, y) {
                            Ok(p) if p[..3].iter().all(|&c| c < 24) => '.',
                            Ok(_) => '#',
                            Err(_) => '?',
                        }
                    })
                    .collect();
                println!("  {line}");
            }
        }

        /// Checks every capture that arrived.
        pub fn check(&mut self) {
            while let Taken::Value(frame) = self.capture.next(Duration::ZERO) {
                let (w, h) = (f64::from(frame.width), f64::from(frame.height));
                // A point in the middle of a bar at least 24 px thick, on
                // the right or bottom side (the overlay sits top-left).
                let point = if w / h > self.aspect * 1.08 {
                    let bar = (w - h * self.aspect) / 2.0;
                    (bar >= 24.0).then(|| (w - bar / 2.0, h / 2.0))
                } else if w / h < self.aspect / 1.08 {
                    let bar = (h - w / self.aspect) / 2.0;
                    (bar >= 24.0).then(|| (w / 2.0, h - bar / 2.0))
                } else {
                    None
                };
                let Some((x, y)) = point else {
                    self.counts.skipped += 1;
                    continue;
                };
                match gpu::read_pixel(&self.device, &frame.texture, x as u32, y as u32) {
                    Ok(pixel) if pixel[..3].iter().all(|&c| c < 24) => self.counts.good += 1,
                    Ok(_) => {
                        self.counts.bad += 1;
                        if std::env::var_os("ZIZ_PROBE_DUMP").is_some() {
                            self.dump(&frame.texture, frame.width, frame.height);
                        }
                    }
                    Err(_) => self.counts.skipped += 1,
                }
            }
        }
    }
}
