//! Which apps' sound a capture takes, with real audio: copies of the
//! `tone-player` helper stand in for Discord (1 kHz) and another app
//! (440 Hz). The tones play quietly on the speakers for a few seconds.
//! Keep other sound paused while running (the capture sees each app's
//! sound before the Windows volume): `cargo test -p audio --test app_audio --
//! --ignored --test-threads=1`.

use std::collections::HashSet;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use audio::capture::{AudioCapture, CaptureSource};
use audio::opus::{CHANNELS, SAMPLE_RATE};
use audio::process::{DEFAULT_EXCLUDED, processes, window_process};
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
use windows::core::HSTRING;

const DISCORD_HZ: f32 = 1000.0;
const OTHER_HZ: f32 = 440.0;
/// The tones play at amplitude 0.05; a tone measured above this is there.
const PRESENT: f32 = 0.01;

/// Copies of the helper under other names, started through `cmd /c start`
/// so they are not children of this test (every capture leaves out its own
/// process tree), and killed when dropped.
struct Players {
    dir: PathBuf,
    pids: Vec<u32>,
}

impl Players {
    fn start(players: &[(&str, f32)], seconds: f32) -> Self {
        let dir = std::env::temp_dir().join(format!("nitra-app-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let helper = Path::new(env!("CARGO_BIN_EXE_tone-player"));
        let mut pids = Vec::new();
        for (name, hz) in players {
            let exe = dir.join(name);
            std::fs::copy(helper, &exe).unwrap();
            let before = pids_named(name);
            // Raw: `start` takes its first quoted argument as a window title,
            // and Rust's own quoting would escape the empty one.
            let status = Command::new("cmd")
                .raw_arg(format!(
                    "/c start \"\" /min \"{}\" {hz} {seconds}",
                    exe.display()
                ))
                .status()
                .unwrap();
            assert!(status.success());
            let deadline = Instant::now() + Duration::from_secs(3);
            let pid = loop {
                if let Some(pid) = pids_named(name).difference(&before).next() {
                    break *pid;
                }
                assert!(Instant::now() < deadline, "{name} did not start");
                std::thread::sleep(Duration::from_millis(20));
            };
            pids.push(pid);
        }
        // Let the players open their audio sessions and windows.
        std::thread::sleep(Duration::from_millis(800));
        Self { dir, pids }
    }

    fn pid(&self, index: usize) -> u32 {
        self.pids[index]
    }
}

impl Drop for Players {
    fn drop(&mut self) {
        for pid in &self.pids {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .output();
        }
        std::thread::sleep(Duration::from_millis(200));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pids_named(name: &str) -> HashSet<u32> {
    processes()
        .unwrap()
        .into_iter()
        .filter(|(_, p)| p.name.eq_ignore_ascii_case(name))
        .map(|(pid, _)| pid)
        .collect()
}

/// `seconds` of mono samples from a capture.
fn record(source: CaptureSource, seconds: f32) -> Vec<f32> {
    let capture = AudioCapture::start(source).unwrap();
    let mut mono = Vec::new();
    let started = Instant::now();
    while started.elapsed().as_secs_f32() < seconds {
        if let Some(frame) = capture.recv_timeout(Duration::from_millis(100)) {
            mono.extend(
                frame
                    .samples
                    .chunks(CHANNELS)
                    .map(|c| c.iter().sum::<f32>() / CHANNELS as f32),
            );
        }
    }
    let report = capture.stop();
    assert!(report.error.is_none(), "{report:?}");
    println!("captured {} samples, {report:?}", mono.len());
    mono
}

/// Amplitude of the `hz` component, about the tone's amplitude when it is
/// there: Goertzel over 100 ms blocks (a ~10 Hz wide band, so a tone a
/// little off its frequency, as the helper's drift-corrected playback
/// makes it, still counts in full), averaged over the blocks.
fn level(samples: &[f32], hz: f32) -> f32 {
    const BLOCK: usize = SAMPLE_RATE as usize / 10;
    let k = 2.0 * (2.0 * std::f32::consts::PI * hz / SAMPLE_RATE as f32).cos();
    let blocks: Vec<f32> = samples
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .map(|block| {
            let (mut s1, mut s2) = (0.0_f32, 0.0_f32);
            for &x in block {
                let s = x + k * s1 - s2;
                s2 = s1;
                s1 = s;
            }
            let power = s1 * s1 + s2 * s2 - k * s1 * s2;
            2.0 * power.max(0.0).sqrt() / BLOCK as f32
        })
        .collect();
    blocks.iter().sum::<f32>() / blocks.len().max(1) as f32
}

fn default_excluded() -> Vec<String> {
    DEFAULT_EXCLUDED.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
#[ignore = "plays tones on the speakers and needs an audio output"]
fn the_desktop_capture_leaves_discord_out() {
    let players = Players::start(
        &[
            ("Discord.exe", DISCORD_HZ),
            ("nitra-other-app.exe", OTHER_HZ),
        ],
        5.0,
    );
    let mono = record(CaptureSource::SystemExcept(default_excluded()), 2.5);
    let (other, discord) = (level(&mono, OTHER_HZ), level(&mono, DISCORD_HZ));
    println!("other app {other:.5}, Discord {discord:.5}");
    assert!(other > PRESENT, "the other app is missing: {other}");
    assert!(
        discord < other / 100.0,
        "Discord leaked: {discord} vs {other}"
    );
    drop(players);
}

#[test]
#[ignore = "plays tones on the speakers and needs an audio output"]
fn the_plain_system_capture_takes_discord_too() {
    // The control: without the exclusion list the same tone is there, so
    // the test above really tells the two apart.
    let players = Players::start(&[("Discord.exe", DISCORD_HZ)], 4.0);
    let mono = record(CaptureSource::System, 2.0);
    let discord = level(&mono, DISCORD_HZ);
    println!("Discord {discord:.5}");
    assert!(discord > PRESENT, "{discord}");
    drop(players);
}

#[test]
#[ignore = "plays tones on the speakers and needs an audio output"]
fn a_window_capture_takes_only_that_app() {
    let players = Players::start(
        &[
            ("nitra-app-a.exe", DISCORD_HZ),
            ("nitra-app-b.exe", OTHER_HZ),
        ],
        5.0,
    );
    let title = HSTRING::from(format!("nitra tone {DISCORD_HZ}"));
    // SAFETY: plain lookup by exact window title.
    let window = unsafe { FindWindowW(None, &title) }.expect("the helper's window");
    let table = processes().unwrap();
    let pid = window_process(window.0 as isize, &table).unwrap();
    assert_eq!(pid, players.pid(0));
    let mono = record(CaptureSource::App(pid), 2.0);
    let (a, b) = (level(&mono, DISCORD_HZ), level(&mono, OTHER_HZ));
    println!("window app {a:.5}, other app {b:.5}");
    assert!(a > PRESENT, "{a}");
    assert!(b < a / 100.0, "{b} vs {a}");
    drop(players);
}

#[test]
#[ignore = "plays tones on the speakers and needs an audio output"]
fn an_app_that_starts_later_joins_the_mix() {
    let mono = {
        let capture = AudioCapture::start(CaptureSource::SystemExcept(default_excluded())).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let players = Players::start(&[("nitra-late-app.exe", OTHER_HZ)], 3.0);
        let mut mono = Vec::new();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if let Some(frame) = capture.recv_timeout(Duration::from_millis(100)) {
                mono.extend(
                    frame
                        .samples
                        .chunks(CHANNELS)
                        .map(|c| c.iter().sum::<f32>() / CHANNELS as f32),
                );
            }
        }
        let report = capture.stop();
        println!("{report:?}");
        assert!(report.apps >= 1);
        drop(players);
        mono
    };
    assert!(level(&mono, OTHER_HZ) > PRESENT);
}
