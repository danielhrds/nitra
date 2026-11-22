//! Sharing a screen from the window: what can be shared, and a sharing
//! thread that runs the same host as `ziz-cli --role host` (capture, encode,
//! WebRTC; signaling through the room). The media never touches the render
//! thread; stopping sets a flag the host checks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use identity::Identity;
use signaling::signed::Trust;

use crate::host::{self, AudioPlan, HostArgs, Source, Watcher, WindowTarget};
use crate::profile::Profile;
use crate::room::RoomArgs;

const AUDIO_BITRATE_BPS: u32 = 128_000;

/// How to share, from the settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareOptions {
    /// The picture fits in this, keeping its aspect ratio.
    pub max_size: (u32, u32),
    pub fps: u32,
    pub bitrate_bps: u32,
    pub preview_interval: Option<std::time::Duration>,
    /// Left out of a whole-screen share's sound, besides Discord and ziz.
    pub audio_exclude: Vec<String>,
}

impl ShareOptions {
    pub fn from_settings(settings: &super::settings::Settings) -> Self {
        Self {
            max_size: settings.quality.max_size(),
            fps: settings.quality.fps(),
            bitrate_bps: settings.bitrate_bps(),
            preview_interval: settings.preview_interval(),
            audio_exclude: settings.audio_exclude.clone(),
        }
    }
}

/// Something to share, as the picker lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareSource {
    Monitor {
        /// Bench probe order (what `host::Source::Monitor` expects).
        index: usize,
        width: u32,
        height: u32,
    },
    Window {
        hwnd: isize,
        title: String,
        /// Executable name without `.exe`.
        app: String,
        width: u32,
        height: u32,
    },
}

impl ShareSource {
    /// The label the picker and the notices show.
    pub fn label(&self) -> String {
        match self {
            Self::Monitor {
                index,
                width,
                height,
            } => format!("Tela {} · {width}×{height}", index + 1),
            Self::Window { title, app, .. } => format!("{} · {app}", shorten(title, 60)),
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Self::Monitor { width, height, .. } | Self::Window { width, height, .. } => {
                (*width, *height)
            }
        }
    }
}

/// At most `max` characters, with an ellipsis when cut (browser tabs and
/// documents have long titles).
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Monitors, then the windows someone would share (not this app's own).
pub fn sources() -> Vec<ShareSource> {
    let mut list: Vec<ShareSource> = gpu::enumerate_adapters()
        .unwrap_or_default()
        .iter()
        .filter(|a| !a.info.is_software)
        .flat_map(|a| a.outputs.iter())
        .enumerate()
        .map(|(index, output)| {
            let desktop = output.info.desktop;
            ShareSource::Monitor {
                index,
                width: (desktop.right - desktop.left).max(0) as u32,
                height: (desktop.bottom - desktop.top).max(0) as u32,
            }
        })
        .collect();
    let processes = audio::process::processes().unwrap_or_default();
    list.extend(
        capture::wgc::shareable_windows(std::process::id())
            .into_iter()
            .map(|w| ShareSource::Window {
                hwnd: w.hwnd,
                app: processes
                    .get(&w.process_id)
                    .map_or_else(String::new, |p| p.name.trim_end_matches(".exe").to_owned()),
                title: w.title,
                width: w.width,
                height: w.height,
            }),
    );
    list
}

/// The largest size within `max` with the source's aspect ratio, in even
/// pixels (the encoder's 4:2:0 needs them): the GPU scaler stretches to
/// whatever it is given.
pub fn output_size(source: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (w, h) = (f64::from(source.0.max(2)), f64::from(source.1.max(2)));
    let scale = (f64::from(max.0) / w).min(f64::from(max.1) / h).min(1.0);
    let even = |v: f64| ((v * scale / 2.0).round() as u32 * 2).max(2);
    (even(w), even(h))
}

/// How a sharing thread ended.
#[derive(Debug)]
pub struct ShareEnded {
    /// Which share (`Sharing::id`).
    pub id: u64,
    /// None if it stopped because it was asked to.
    pub error: Option<String>,
}

/// A running share (or one stopping).
pub struct Sharing {
    pub id: u64,
    audience: Arc<Mutex<Vec<Watcher>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// What the sharing thread needs from the profile.
pub struct ProfileParts {
    pub name: String,
    pub dir: std::path::PathBuf,
    pub identity: Arc<Identity>,
}

impl Sharing {
    /// Starts sharing `source` in `room`; `ended` gets the outcome (then
    /// `wake` runs, to redraw the window).
    pub fn start(
        id: u64,
        profile: ProfileParts,
        room: RoomArgs,
        source: &ShareSource,
        options: &ShareOptions,
        ended: Sender<ShareEnded>,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (width, height) = output_size(source.size(), options.max_size);
        let audience = Arc::new(Mutex::new(Vec::new()));
        let host_source = match *source {
            ShareSource::Monitor { index, .. } => Source::Monitor {
                index,
                size: Some((width, height)),
            },
            ShareSource::Window { hwnd, .. } => Source::Window {
                target: WindowTarget::Handle(hwnd),
                width,
                height,
            },
        };
        let args = HostArgs {
            // Any free port: the window's local signaling is not used, and
            // a fixed one would clash with another instance on this PC.
            port: 0,
            source: host_source,
            fps: options.fps,
            bitrate_bps: options.bitrate_bps,
            seconds: 0,
            audio: AudioPlan::Auto {
                also_excluded: options.audio_exclude.clone(),
            },
            audio_bitrate_bps: AUDIO_BITRATE_BPS,
            network: transport::gather::NetworkConfig::internet(),
            // Nobody over local signaling; room viewers are checked by the room.
            trust: Trust::Keys(Vec::new()),
            invite: None,
            room: Some(room),
            audience: Some(Arc::clone(&audience)),
            preview_interval: options.preview_interval,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("share".into())
                .spawn(move || {
                    let profile = Profile {
                        name: profile.name,
                        dir: profile.dir,
                        identity: profile.identity,
                    };
                    let error = run_host(&profile, &args, &stop).err();
                    if let Some(error) = &error {
                        tracing::warn!(%error, "sharing ended");
                    }
                    let _ = ended.send(ShareEnded { id, error });
                    wake();
                })?
        };
        Ok(Self {
            id,
            audience,
            stop,
            thread: Some(thread),
        })
    }

    /// Who is watching now, and how the link to each is doing.
    pub fn audience(&self) -> Vec<Watcher> {
        self.audience.lock().unwrap().clone()
    }

    /// Asks the host to stop; the thread ends on its own (`ShareEnded`).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Stops and waits for the thread (closing the app).
    pub fn finish(mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The host on this thread, with COM and Media Foundation set up for
/// capture and encode (the MF encoder, used off AMD GPUs, needs `MFStartup`).
fn run_host(profile: &Profile, args: &HostArgs, stop: &AtomicBool) -> Result<(), String> {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    // SAFETY: balanced below on this thread.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| e.to_string())?;
    let result = mf::MediaFoundation::start()
        .map_err(|e| e.to_string())
        .and_then(|_media_foundation| {
            host::run(profile, args, stop).map_err(|error| {
                // In the window's words ("A transmissão parou: ...").
                match error.downcast_ref::<host::SourceGone>() {
                    Some(gone) if gone.monitor => {
                        "o monitor transmitido foi desconectado ou desligado".to_owned()
                    }
                    Some(_) => "a janela transmitida foi fechada".to_owned(),
                    None => error.to_string(),
                }
            })
        });
    // SAFETY: balances the call above, after the host dropped its COM objects.
    unsafe { CoUninitialize() };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_fits_720p_keeping_the_aspect_ratio() {
        assert_eq!(output_size((2560, 1440), (1280, 720)), (1280, 720));
        assert_eq!(output_size((1920, 1080), (1280, 720)), (1280, 720));
        // 4:3 and tall windows: height-bound, no stretching.
        assert_eq!(output_size((1600, 1200), (1280, 720)), (960, 720));
        assert_eq!(output_size((800, 1600), (1280, 720)), (360, 720));
        // Ultrawide: width-bound.
        assert_eq!(output_size((3440, 1440), (1280, 720)), (1280, 536));
        // Small windows are not enlarged; odd sizes become even.
        assert_eq!(output_size((801, 601), (1280, 720)), (802, 602));
        assert_eq!(output_size((0, 0), (1280, 720)), (2, 2));
    }

    #[test]
    fn long_window_titles_are_shortened() {
        assert_eq!(shorten("WhatsApp", 60), "WhatsApp");
        let long = "a".repeat(80);
        let short = shorten(&long, 60);
        assert_eq!(short.chars().count(), 60);
        assert!(short.ends_with('…'));
    }
}
