//! `bench present <file.h264>`: decodes an Annex B file with the hardware
//! decoder and shows it in the video presenter's window at the source rate,
//! optionally looping, until the time is up or the window is closed. For
//! the visual check (smooth playback, resize, fullscreen) and the
//! presenter's statistics.

use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use capture::synthetic::Pacer;
use decode::media_foundation::MediaFoundationDecoder;
use decode::{DecodeSubmit, DecodedFrame, VideoDecoder};
use gpu::DebugLayer;
use present::{FrameMeta, Presented, PresenterOptions, VideoPresenter};
use telemetry::{QpcTimestamp, Stage, Summary, qpc_frequency, qpc_now};

use crate::com::ComApartment;

pub struct PresentCommand {
    pub file: PathBuf,
    pub fps: u32,
    /// Stop after this long (0 = when the file ends, or never with `repeat`).
    pub seconds: u32,
    pub repeat: bool,
    pub width: u32,
    pub height: u32,
    pub borderless: bool,
    pub overlay: bool,
    /// Per-frame present and display QPC times.
    pub display_csv: Option<PathBuf>,
}

pub fn run(command: PresentCommand) -> Result<(), Box<dyn Error>> {
    let _com = ComApartment::multithreaded()?;
    let _media_foundation = mf::MediaFoundation::start()?;
    let stream = std::fs::read(&command.file)?;
    let units = encode::annexb::access_units(&stream);
    if units.is_empty() {
        return Err(format!("no access units in {}", command.file.display()).into());
    }
    let adapters = gpu::enumerate_adapters()?;
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .ok_or("no hardware adapter found")?;
    let debug_layer = if cfg!(debug_assertions) {
        DebugLayer::IfAvailable
    } else {
        DebugLayer::Off
    };
    let device = gpu::create_device(adapter, debug_layer)?;
    let mut decoder = MediaFoundationDecoder::new(&device)?;
    let mut presenter = VideoPresenter::open(
        &device,
        PresenterOptions {
            title: format!(
                "ziz bench present - {}",
                command.file.file_name().map_or_else(
                    || command.file.display().to_string(),
                    |name| name.to_string_lossy().into_owned()
                )
            ),
            width: command.width,
            height: command.height,
            borderless: command.borderless,
            overlay: command.overlay,
            parent: None,
        },
    )?;
    println!(
        "present: {} ({} access units) at {} fps{}; F11 / Alt+Enter fullscreen, O overlay, close the window to stop",
        command.file.display(),
        units.len(),
        command.fps,
        if command.repeat { ", repeating" } else { "" }
    );

    let started = Instant::now();
    let limit = (command.seconds > 0).then(|| Duration::from_secs(u64::from(command.seconds)));
    let mut pacer = Pacer::start_now(command.fps.max(1));
    let mut index = 0_u64;
    let mut decoded = 0_u64;
    let mut show =
        |frame: DecodedFrame, presenter: &mut VideoPresenter| -> Result<bool, Box<dyn Error>> {
            decoded += 1;
            let result = presenter.present(
                &frame.texture,
                frame.slice,
                frame.width,
                frame.height,
                FrameMeta {
                    frame_id: frame.frame_id,
                    captured_at: frame.captured_at,
                    decoded_at: frame.decoded_at,
                },
            )?;
            Ok(result != Presented::Closed)
        };
    'play: loop {
        for unit in &units {
            if presenter.is_closed() || limit.is_some_and(|limit| started.elapsed() >= limit) {
                break 'play;
            }
            let slot = pacer.next_slot(qpc_now());
            pacer.wait(&slot);
            while decoder.submit(unit, index, QpcTimestamp::from_ticks(0))? == DecodeSubmit::Busy {
                while let Some(frame) = decoder.poll()? {
                    if !show(frame, &mut presenter)? {
                        break 'play;
                    }
                }
            }
            index += 1;
            while let Some(frame) = decoder.poll()? {
                if !show(frame, &mut presenter)? {
                    break 'play;
                }
            }
        }
        if !command.repeat {
            break;
        }
    }
    if !presenter.is_closed() {
        for frame in decoder.drain()? {
            show(frame, &mut presenter)?;
        }
        // Let the last frame reach the screen before closing.
        std::thread::sleep(Duration::from_millis(100));
    }
    let elapsed = started.elapsed().as_secs_f64();
    let report = presenter.close();
    let counters = report.counters;
    println!(
        "  {} access units submitted, {} frames decoded in {:.1} s",
        index, decoded, elapsed
    );
    println!(
        "  presenter: {} submitted, {} presented ({:.1}/s), {} replaced, {} dropped (pool), {} redraws",
        counters.submitted,
        counters.presented,
        counters.presented as f64 / elapsed,
        counters.replaced,
        counters.pool_dropped,
        counters.redraws
    );
    if let Some(s) = Summary::from_durations(report.stages.samples(Stage::Present), qpc_frequency())
    {
        println!(
            "  decode->present (ms): mean {:.2}, p50 {:.2}, p95 {:.2}, p99 {:.2}, max {:.2}",
            s.mean_us / 1000.0,
            s.p50_us / 1000.0,
            s.p95_us / 1000.0,
            s.p99_us / 1000.0,
            s.max_us / 1000.0
        );
    }
    let display = &report.display;
    let to_displayed: Vec<_> = display
        .frames
        .iter()
        .map(|frame| frame.displayed_at.duration_since(frame.presented_at))
        .collect();
    if let Some(s) = Summary::from_durations(&to_displayed, qpc_frequency()) {
        println!(
            "  present->displayed (ms): mean {:.2}, p50 {:.2}, p95 {:.2}, p99 {:.2}, max {:.2}",
            s.mean_us / 1000.0,
            s.p50_us / 1000.0,
            s.p95_us / 1000.0,
            s.p99_us / 1000.0,
            s.max_us / 1000.0
        );
    }
    println!(
        "  display: {:?}, refresh period {:.0} us",
        display.counters,
        display.refresh_period_us.unwrap_or(0.0)
    );
    if let Some(path) = &command.display_csv {
        let mut csv = String::from("frame_id,presented_qpc,displayed_qpc\n");
        for frame in &display.frames {
            csv.push_str(&format!(
                "{},{},{}\n",
                frame.frame_id,
                frame.presented_at.ticks(),
                frame.displayed_at.ticks()
            ));
        }
        std::fs::write(path, csv)?;
        println!("  display CSV: {}", path.display());
    }
    let messages = device.take_debug_messages();
    if device.debug_layer_enabled() {
        let problems: Vec<_> = messages
            .iter()
            .filter(|message| message.severity.is_problem())
            .collect();
        println!(
            "  debug layer: {} message(s), {} warning/error",
            messages.len(),
            problems.len()
        );
        for message in &problems {
            println!("    {:?}: {}", message.severity, message.text);
        }
    }
    match report.error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}
