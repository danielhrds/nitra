//! The video player on a thread of its own: feed (encode -> decode) into a
//! `VideoPresenter`, either embedded as a child window of the GPUI window
//! (placed over the layout's video area) or in a window of its own
//! (pop-out). The GPUI thread never touches video: it only sends the video
//! area's bounds and stop requests.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use feed::{Feed, FeedConfig, MediaThread};
use present::{FrameMeta, Presented, PresenterOptions, VideoPresenter};
use windows::Win32::System::Threading::WaitForSingleObject;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Embedded,
    PopOut,
}

enum Command {
    /// Physical pixels, in the parent's client area.
    Bounds(i32, i32, i32, i32),
    Stop,
}

pub struct Player {
    pub mode: Mode,
    commands: Sender<Command>,
    /// Set when the player ended on its own (pop-out window closed, error).
    ended: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<String, String>>>,
}

impl Player {
    /// `parent`: the GPUI window's HWND (used in embedded mode).
    pub fn start(mode: Mode, parent: isize) -> Self {
        let (commands, receiver) = channel();
        let ended = Arc::new(AtomicBool::new(false));
        let thread_ended = Arc::clone(&ended);
        let thread = std::thread::Builder::new()
            .name("player".to_owned())
            .spawn(move || {
                let result = run(mode, parent, &receiver);
                thread_ended.store(true, Ordering::Relaxed);
                result
            })
            .expect("player thread");
        Self {
            mode,
            commands,
            ended,
            thread: Some(thread),
        }
    }

    /// A sender of the video area (physical pixels, parent client area).
    pub fn bounds_sender(&self) -> impl Fn(i32, i32, i32, i32) + 'static {
        let commands = self.commands.clone();
        move |x, y, w, h| {
            let _ = commands.send(Command::Bounds(x, y, w, h));
        }
    }

    pub fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    /// Stops the player; returns its summary line.
    pub fn stop(mut self) -> Result<String, String> {
        self.join()
    }

    fn join(&mut self) -> Result<String, String> {
        let Some(thread) = self.thread.take() else {
            return Ok(String::new());
        };
        let _ = self.commands.send(Command::Stop);
        thread
            .join()
            .unwrap_or_else(|_| Err("player panicked".to_owned()))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

fn run(mode: Mode, parent: isize, commands: &Receiver<Command>) -> Result<String, String> {
    let _media = MediaThread::start()?;
    let adapters = gpu::enumerate_adapters().map_err(|e| e.to_string())?;
    let adapter = adapters
        .iter()
        .find(|adapter| !adapter.info.is_software)
        .ok_or("no hardware adapter")?;
    let device = gpu::create_device(adapter, gpu::DebugLayer::Off).map_err(|e| e.to_string())?;
    let mut presenter = VideoPresenter::open(
        &device,
        PresenterOptions {
            title: "ziz - stream".to_owned(),
            width: 1280,
            height: 720,
            borderless: false,
            overlay: true,
            parent: (mode == Mode::Embedded).then_some(parent),
        },
    )
    .map_err(|e| e.to_string())?;
    let mut feed = Feed::start(&device, &adapter.info, FeedConfig::default())?;
    'run: loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Bounds(x, y, w, h)) => presenter.set_bounds(x, y, w, h),
                Ok(Command::Stop) | Err(TryRecvError::Disconnected) => break 'run,
                Err(TryRecvError::Empty) => break,
            }
        }
        if presenter.is_closed() {
            break;
        }
        // SAFETY: valid event owned by the feed; bounded wait so commands
        // are handled promptly.
        let _ = unsafe { WaitForSingleObject(feed.ready_event(), 10) };
        if let Some(frame) = feed.poll()? {
            let shown = presenter
                .present(
                    &frame.texture,
                    frame.slice,
                    frame.width,
                    frame.height,
                    FrameMeta {
                        frame_id: frame.frame_id,
                        captured_at: frame.captured_at,
                        decoded_at: frame.decoded_at,
                    },
                )
                .map_err(|e| e.to_string())?;
            if shown == Presented::Closed {
                break;
            }
        }
    }
    let counters = feed.counters();
    feed.stop()?;
    let report = presenter.close();
    let summary = format!("feed {counters:?}; presenter {:?}", report.counters);
    match report.error {
        Some(error) => Err(error),
        None => Ok(summary),
    }
}
