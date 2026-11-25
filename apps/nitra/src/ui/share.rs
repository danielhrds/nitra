//! Sharing a screen from the window: what can be shared, and a sharing
//! thread that runs the same host as `nitra-cli --role host` (capture, encode,
//! WebRTC; signaling through the room). The media never touches the render
//! thread; stopping sets a flag the host checks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use identity::Identity;
use signaling::signed::Trust;

use crate::host::{self, AudioPlan, HostArgs, Source, Watcher, WindowTarget};
use crate::i18n::tr;
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
    /// Left out of a whole-screen share's sound, besides Discord and nitra.
    pub audio_exclude: Vec<String>,
}

impl ShareOptions {
    pub fn from_settings(settings: &super::settings::Settings) -> Self {
        Self {
            max_size: settings.resolution.max_size(),
            fps: settings.share_fps(),
            bitrate_bps: settings.bitrate_bps(),
            preview_interval: settings.preview_interval(),
            audio_exclude: settings.audio_exclude.clone(),
        }
    }

    /// A camera: 720p30 at most, a bitrate for a face rather than a game,
    /// no preview and no sound (the host leaves both out for a camera).
    pub fn camera() -> Self {
        Self {
            max_size: (1280, 720),
            fps: 30,
            bitrate_bps: CAMERA_BITRATE_BPS,
            preview_interval: None,
            audio_exclude: Vec::new(),
        }
    }
}

/// A camera's bitrate: 720p30 of a person moving a little.
const CAMERA_BITRATE_BPS: u32 = 2_500_000;

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
    /// A camera, by the name Windows lists it under (shared beside the
    /// screen, by a share of its own).
    Camera { name: String },
}

impl ShareSource {
    /// The label the picker and the notices show.
    pub fn label(&self) -> String {
        match self {
            Self::Monitor {
                index,
                width,
                height,
            } => format!("{} {} · {width}×{height}", tr("Tela", "Screen"), index + 1),
            Self::Window { title, app, .. } => format!("{} · {app}", shorten(title, 60)),
            Self::Camera { name } => format!("{} · {name}", tr("Câmera", "Camera")),
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Self::Monitor { width, height, .. } | Self::Window { width, height, .. } => {
                (*width, *height)
            }
            // The host fits the camera's own size in 720p.
            Self::Camera { .. } => (1280, 720),
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

/// How a switch to another source went (the share goes on either way).
#[derive(Debug)]
pub struct ShareSwitched {
    /// Which share (`Sharing::id`).
    pub id: u64,
    /// What is shared now, if it worked.
    pub label: String,
    pub error: Option<String>,
}

/// A running share (or one stopping).
pub struct Sharing {
    pub id: u64,
    audience: Arc<Mutex<Vec<Watcher>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Requests to share another source, read by the host.
    switches: Sender<host::Switch>,
    /// The picture's size limit, kept for the next source.
    max_size: (u32, u32),
    /// The sharer's own live picture (on when the window wants it).
    own_preview: Arc<host::OwnPreview>,
    /// Where switch outcomes go, and how to wake the window for them.
    switched: Sender<ShareSwitched>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

/// The host's source for `source`, scaled to `(width, height)`.
fn host_source(source: &ShareSource, (width, height): (u32, u32)) -> Source {
    match source {
        &ShareSource::Monitor { index, .. } => Source::Monitor {
            index,
            size: Some((width, height)),
        },
        &ShareSource::Window { hwnd, .. } => Source::Window {
            target: WindowTarget::Handle(hwnd),
            width,
            height,
        },
        ShareSource::Camera { name } => Source::Camera { name: name.clone() },
    }
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
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        id: u64,
        profile: ProfileParts,
        room: RoomArgs,
        source: &ShareSource,
        options: &ShareOptions,
        ended: Sender<ShareEnded>,
        switched: Sender<ShareSwitched>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let size = output_size(source.size(), options.max_size);
        let audience = Arc::new(Mutex::new(Vec::new()));
        let (switches, switch_requests) = mpsc::channel();
        let own_preview = Arc::new(host::OwnPreview::default());
        let args = HostArgs {
            // Any free port: the window's local signaling is not used, and
            // a fixed one would clash with another instance on this PC.
            port: 0,
            source: host_source(source, size),
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
            switches: Some(Arc::new(Mutex::new(switch_requests))),
            own_preview: Some(Arc::clone(&own_preview)),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            let wake = Arc::clone(&wake);
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
            switches,
            max_size: options.max_size,
            own_preview,
            switched,
            wake,
        })
    }

    /// Turns the sharer's own live picture on or off.
    pub fn set_preview(&self, wanted: bool) {
        self.own_preview.wanted.store(wanted, Ordering::Relaxed);
    }

    /// The texture with the live picture, once the host made it.
    pub fn preview_handle(&self) -> Option<gpu::convert::SharedHandle> {
        *self.own_preview.handle.lock().unwrap()
    }

    /// Shares `source` instead, without ending the share: viewers stay
    /// connected. The outcome comes back as a [`ShareSwitched`].
    pub fn switch(&self, source: &ShareSource) {
        let size = output_size(source.size(), self.max_size);
        let (done, outcome) = mpsc::channel();
        let request = host::Switch {
            source: host_source(source, size),
            done,
        };
        if self.switches.send(request).is_err() {
            return; // The host already ended (its `ShareEnded` says why).
        }
        let (id, label) = (self.id, source.label());
        let (switched, wake) = (self.switched.clone(), Arc::clone(&self.wake));
        // Waits for the host's answer off the render thread.
        let _ = std::thread::Builder::new()
            .name("share-switch".into())
            .spawn(move || {
                let error = match outcome.recv() {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(_) => return, // The host ended first.
                };
                let _ = switched.send(ShareSwitched { id, label, error });
                wake();
            });
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
            host::run(profile, args, stop).map_err(|error| describe(error.as_ref()))
        });
    // SAFETY: balances the call above, after the host dropped its COM objects.
    unsafe { CoUninitialize() };
    result
}

/// The host's error in the window's words ("A transmissão parou: ...").
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    if let Some(gone) = error.downcast_ref::<host::SourceGone>() {
        return match gone {
            host::SourceGone::Monitor => tr(
                "o monitor transmitido foi desconectado ou desligado",
                "the shared monitor was disconnected or turned off",
            ),
            host::SourceGone::Window => tr(
                "a janela transmitida foi fechada",
                "the shared window was closed",
            ),
            host::SourceGone::Camera => {
                tr("a câmera foi desconectada", "the camera was disconnected")
            }
        }
        .to_owned();
    }
    if let Some(pipeline::StartError::NoEncoder(detail)) = error.downcast_ref() {
        tracing::warn!(%detail, "no hardware encoder started");
        // No software fallback: encoding on the CPU would weigh on the game.
        return tr(
            "a placa de vídeo não conseguiu iniciar um codificador H.264 por hardware. \
             Atualizar o driver da placa costuma resolver; o nitra não transmite por \
             software, porque pesaria no jogo.",
            "the graphics card could not start a hardware H.264 encoder. Updating its \
             driver usually fixes it; nitra does not share with a software encoder, \
             because it would weigh on the game.",
        )
        .to_owned();
    }
    error.to_string()
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
    fn host_errors_read_in_the_window_words() {
        let gone: Box<dyn std::error::Error> = Box::new(host::SourceGone::Monitor);
        assert!(describe(gone.as_ref()).contains("monitor"));
        let camera: Box<dyn std::error::Error> = Box::new(host::SourceGone::Camera);
        assert!(describe(camera.as_ref()).contains("câmera"));
        let no_encoder: Box<dyn std::error::Error> = Box::new(pipeline::StartError::NoEncoder(
            "encoder not found: media-foundation: no MFT".to_owned(),
        ));
        let text = describe(no_encoder.as_ref());
        assert!(text.contains("codificador H.264 por hardware"), "{text}");
        assert!(!text.contains("MFT"), "{text}");
        // Anything else keeps its own words.
        let other: Box<dyn std::error::Error> =
            Box::new(pipeline::StartError::Other("device context: x".to_owned()));
        assert_eq!(describe(other.as_ref()), "device context: x");
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
