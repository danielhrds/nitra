//! Watching screens inside the window: one thread per watched screen runs
//! the same viewer as `ziz-cli --role viewer` (network, decoder, presenter;
//! signaling through the room), with its video as a child window of the
//! app's. The render thread only moves those windows and reads their stats.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use gpu::D3d11Device;
use identity::PublicKey;
use rooms::Join;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    SWP_ASYNCWINDOWPOS, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SetWindowPos,
};

use super::share::ProfileParts;
use crate::profile::Profile;
use crate::room::RoomArgs;
use crate::viewer::{self, Embed, LiveStats, ViewerArgs, ViewerSignaling};

/// Whose screen, in which room, on which room server.
pub struct WatchTarget {
    pub server: String,
    pub room: String,
    pub host: PublicKey,
}

/// How a watching thread ended.
#[derive(Debug)]
pub struct WatchEnded {
    pub id: u64,
    /// What went wrong, as a sentence for the window (None: stopped).
    pub error: Option<String>,
}

/// A watched screen (or one stopping).
pub struct Watching {
    pub id: u64,
    video_window: Arc<AtomicIsize>,
    stats: Arc<Mutex<LiveStats>>,
    muted: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Where the video was last put (None: hidden), to move it only on a change.
    placed: Option<[i32; 4]>,
    /// The video window was made click-through (disabled: its clicks go to
    /// the app's window, which keeps the keyboard).
    disabled: bool,
}

impl Watching {
    /// Starts watching `target` inside `parent`; `ended` gets the outcome,
    /// then `wake` runs.
    pub fn start(
        id: u64,
        profile: ProfileParts,
        WatchTarget { server, room, host }: WatchTarget,
        device: D3d11Device,
        parent: isize,
        ended: Sender<WatchEnded>,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Self> {
        let (video_window, stats, muted) = (Arc::default(), Arc::default(), Arc::default());
        let args = ViewerArgs {
            signaling: ViewerSignaling::Room {
                room: RoomArgs {
                    server,
                    room,
                    join: Join::Member,
                    invite: false,
                },
                host_key: host,
            },
            position: None,
            stamp_every: 0,
            seconds: 0,
            summary_json: None,
            headless: false,
            decode: true,
            impairment: test_impairment(),
            mute: false,
            network: transport::gather::NetworkConfig::internet(),
            embed: Some(Embed {
                parent,
                video_window: Arc::clone(&video_window),
                stats: Arc::clone(&stats),
                muted: Arc::clone(&muted),
                device: Some(device),
            }),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("watch".into())
                .spawn(move || {
                    let profile = Profile {
                        name: profile.name,
                        dir: profile.dir,
                        identity: profile.identity,
                    };
                    let error = run_viewer(&profile, &args, &stop).err();
                    if let Some(error) = &error {
                        tracing::warn!(%error, "watching ended");
                    }
                    let _ = ended.send(WatchEnded { id, error });
                    wake();
                })?
        };
        Ok(Self {
            id,
            video_window,
            stats,
            muted,
            stop,
            thread: Some(thread),
            placed: None,
            disabled: false,
        })
    }

    pub fn stats(&self) -> LiveStats {
        *self.stats.lock().unwrap()
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Puts the video at `rect` (x, y, width, height in the parent's client
    /// pixels), or hides it. Does nothing until the video window exists or
    /// when nothing changed; never waits for the viewer thread.
    pub fn place(&mut self, rect: Option<[i32; 4]>) {
        let raw = self.video_window.load(Ordering::Relaxed);
        if raw == 0 {
            return;
        }
        let window = HWND(raw as *mut _);
        if !self.disabled {
            self.disabled = true;
            // SAFETY: a valid child window (a closed one makes the call fail).
            let _ = unsafe { EnableWindow(window, false) };
        }
        if rect == self.placed {
            return;
        }
        self.placed = rect;
        let flags = SWP_ASYNCWINDOWPOS | SWP_NOZORDER | SWP_NOACTIVATE;
        // SAFETY: as above; asynchronous, so the viewer thread applies it.
        let _ = unsafe {
            match rect {
                Some([x, y, w, h]) => {
                    SetWindowPos(window, None, x, y, w, h, flags | SWP_SHOWWINDOW)
                }
                None => SetWindowPos(
                    window,
                    None,
                    0,
                    0,
                    0,
                    0,
                    flags | SWP_NOMOVE | SWP_NOSIZE | SWP_HIDEWINDOW,
                ),
            }
        };
    }

    /// Asks the viewer to stop; the thread ends on its own (`WatchEnded`).
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

/// Testing: `ZIZ_TEST_LOSS=0.05` drops that share of what this app receives
/// (checks the loss shown in the window). Unset in normal use.
fn test_impairment() -> Option<transport::impair::ImpairmentConfig> {
    let loss: f64 = std::env::var("ZIZ_TEST_LOSS").ok()?.parse().ok()?;
    Some(transport::impair::ImpairmentConfig {
        loss,
        ..Default::default()
    })
}

/// The viewer on this thread, with COM and Media Foundation set up for the
/// decoder (without `MFStartup` it decodes a few frames, then waits forever).
fn run_viewer(profile: &Profile, args: &ViewerArgs, stop: &AtomicBool) -> Result<(), String> {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    // SAFETY: balanced below on this thread.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| format!("Parou de assistir: {e}"))?;
    let result = mf::MediaFoundation::start()
        .map_err(|e| format!("Parou de assistir: {e}"))
        .and_then(|_media_foundation| viewer::run(profile, args, stop).map_err(describe));
    // SAFETY: balances the call above, after the viewer dropped its COM objects.
    unsafe { CoUninitialize() };
    result
}

/// The viewer's error in the window's words: a connection that never opened
/// gets its probable reason (there is no relay to fall back on); a GPU that
/// cannot decode H.264 says so.
fn describe(error: Box<dyn std::error::Error>) -> String {
    if let Some(viewer::NoDecoder(detail)) = error.downcast_ref() {
        tracing::warn!(%detail, "no H.264 decoder started");
        return "Não deu para assistir: a placa de vídeo não conseguiu iniciar o ".to_owned()
            + "decodificador H.264 do Windows. Atualizar o driver da placa costuma resolver.";
    }
    match error.downcast_ref::<viewer::NoDirectPath>() {
        Some(path) if path.had_public_address => {
            "Sem conexão direta com quem transmite. O mais provável: NAT restritivo ".to_owned()
                + "nos dois lados ou um firewall bloqueando UDP (o ziz não usa servidor "
                + "intermediário)."
        }
        Some(_) => {
            "Sem conexão direta: este PC não conseguiu um endereço público. A rede ".to_owned()
                + "ou um firewall deve estar bloqueando UDP para a internet."
        }
        None => format!("Parou de assistir: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_errors_read_in_the_window_words() {
        let text = describe(Box::new(viewer::NoDecoder("unsupported: x".to_owned())));
        assert!(text.contains("decodificador H.264"), "{text}");
        assert!(!text.contains("unsupported"), "{text}");
        let text = describe(Box::new(viewer::NoDirectPath {
            had_public_address: false,
        }));
        assert!(text.contains("endereço público"), "{text}");
        assert!(describe("boom".into()).starts_with("Parou de assistir: boom"));
    }
}
