//! The window's settings, saved in the profile (`settings.json`): the name
//! others see in the rooms, how shares are sent, how often previews go out,
//! apps left out of a whole-screen share's sound, and what the close button
//! does. Missing fields take their defaults; a damaged file is set aside
//! (renamed, never overwritten) and the defaults are used.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::i18n::{LanguageChoice, thousands, tr};

const FILE: &str = "settings.json";
/// The room server's limit for names (characters).
pub const NAME_MAX_CHARS: usize = 32;

/// The largest picture a share is sent at, keeping its aspect ratio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// The source's own size (up to 4K, the hardware encoders' limit).
    Source,
    P1080,
    #[default]
    P720,
}

impl Resolution {
    pub const ALL: [Self; 3] = [Self::Source, Self::P1080, Self::P720];

    pub fn max_size(self) -> (u32, u32) {
        match self {
            Self::Source => (3840, 2160),
            Self::P1080 => (1920, 1080),
            Self::P720 => (1280, 720),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Source => tr("Fonte", "Source"),
            Self::P1080 => "1080p",
            Self::P720 => "720p",
        }
    }
}

/// The frame rates offered.
pub const FPS_CHOICES: [u32; 3] = [15, 30, 60];
/// The bitrates the slider allows (kbps), and its step.
pub const BITRATE_RANGE_KBPS: (u32, u32) = (500, 20_000);
pub const BITRATE_STEP_KBPS: u32 = 100;

/// Ready-made choices of resolution, frame rate and bitrate; any other mix
/// is "manual".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// Light on the upload: slow connections or several viewers (VMAF 84
    /// against 94 for the default, the same cost to the game).
    Light,
    /// The default.
    Normal,
    High,
}

impl Preset {
    pub const ALL: [Self; 3] = [Self::Light, Self::Normal, Self::High];

    /// (resolution, frames per second, kbps).
    pub fn values(self) -> (Resolution, u32, u32) {
        match self {
            Self::Light => (Resolution::P720, 30, 2_000),
            Self::Normal => (Resolution::P720, 60, 6_000),
            Self::High => (Resolution::P1080, 60, 10_000),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => tr("Leve", "Light"),
            Self::Normal => "Normal",
            Self::High => tr("Alta", "High"),
        }
    }
}
/// How watched screens trade delay for smoothness: the viewer's own choice
/// (each person watching picks theirs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fluidity {
    /// Holds frames only while they come unevenly (up to ~33 ms at 60 fps).
    #[default]
    Auto,
    /// Always holds two frames (~33 ms at 60 fps).
    Smooth,
    /// Shows each frame as soon as it arrives.
    Minimum,
}

impl Fluidity {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Smooth, Self::Minimum];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => tr(
                "Automática (suave só quando precisa)",
                "Automatic (smooth only when needed)",
            ),
            Self::Smooth => tr("Suave (+ ~33 ms)", "Smooth (+ ~33 ms)"),
            Self::Minimum => tr("Atraso mínimo", "Lowest delay"),
        }
    }

    pub fn smoothing(self) -> present::Smoothing {
        match self {
            Self::Auto => present::Smoothing::Auto,
            Self::Smooth => present::Smoothing::Smooth,
            Self::Minimum => present::Smoothing::Minimum,
        }
    }
}

/// The preview intervals offered (minutes; 0 = no previews).
pub const PREVIEW_CHOICES: [u32; 5] = [1, 3, 5, 10, 0];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The name others see; None until chosen (asked on the first start).
    pub name: Option<String>,
    /// How a share is sent: the largest picture, frames per second and
    /// bitrate (kbps), chosen in the room's panel (presets fill them in).
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Minutes between previews; 0 = none.
    pub preview_minutes: u32,
    /// Apps a whole-screen share leaves out of its sound, besides Discord
    /// and nitra (executable names, e.g. `TeamSpeak.exe`).
    pub audio_exclude: Vec<String>,
    /// How watched screens trade delay for smoothness.
    pub fluidity: Fluidity,
    /// The close button hides the window to the tray (quit from its menu).
    pub close_to_tray: bool,
    /// Sharing on a GPU nitra was not tested on was accepted once: do not
    /// ask again.
    pub untested_gpu_ok: bool,
    /// Each person's volume when watched (their key code), only those not
    /// at 100%; the most recently set last, at most [`MAX_VOLUMES`].
    pub volumes: Vec<PersonVolume>,
    /// The share panel beside the screens in a room is hidden (more room
    /// for the screens).
    pub share_panel_hidden: bool,
    /// Show a live picture of your own share on its card (off: nothing is
    /// spent on it).
    pub own_preview: bool,
    /// The language the app is shown in.
    pub language: LanguageChoice,
}

/// How loud someone's screens play.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonVolume {
    pub key: String,
    pub percent: u8,
}

/// People whose volume is kept (the oldest set goes first).
pub const MAX_VOLUMES: usize = 200;

impl Default for Settings {
    fn default() -> Self {
        Self {
            name: None,
            resolution: Preset::Normal.values().0,
            fps: Preset::Normal.values().1,
            bitrate_kbps: Preset::Normal.values().2,
            preview_minutes: 3,
            audio_exclude: Vec::new(),
            fluidity: Fluidity::Auto,
            // Like most apps: the close button closes; the tray icon can
            // still hide the window, and this setting keeps it there.
            close_to_tray: false,
            untested_gpu_ok: false,
            volumes: Vec::new(),
            share_panel_hidden: false,
            own_preview: false,
            language: LanguageChoice::Auto,
        }
    }
}

impl Settings {
    /// The volume `key`'s screens play at (100 if never set).
    pub fn volume_of(&self, key: &str) -> u8 {
        self.volumes
            .iter()
            .find(|v| v.key == key)
            .map_or(100, |v| v.percent.min(100))
    }

    /// Keeps `key`'s volume (100: forgets it), as the most recent.
    pub fn set_volume(&mut self, key: &str, percent: u8) {
        self.volumes.retain(|v| v.key != key);
        if percent < 100 {
            self.volumes.push(PersonVolume {
                key: key.to_owned(),
                percent,
            });
        }
        let excess = self.volumes.len().saturating_sub(MAX_VOLUMES);
        self.volumes.drain(..excess);
    }

    /// The bitrate to share at, in bits per second (in the slider's range).
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_kbps
            .clamp(BITRATE_RANGE_KBPS.0, BITRATE_RANGE_KBPS.1)
            * 1000
    }

    /// The frame rate to share at: one of the choices (the nearest below,
    /// for a file edited by hand).
    pub fn share_fps(&self) -> u32 {
        FPS_CHOICES
            .iter()
            .copied()
            .rev()
            .find(|&f| f <= self.fps)
            .unwrap_or(FPS_CHOICES[0])
    }

    /// The preset these values match, if any (else "manual").
    pub fn preset(&self) -> Option<Preset> {
        Preset::ALL
            .into_iter()
            .find(|p| p.values() == (self.resolution, self.share_fps(), self.bitrate_kbps))
    }

    pub fn apply(&mut self, preset: Preset) {
        (self.resolution, self.fps, self.bitrate_kbps) = preset.values();
    }

    /// The share as one line ("720p · 60 fps · 6.000 kbps").
    pub fn share_label(&self) -> String {
        format!(
            "{} · {} fps · {} kbps",
            self.resolution.label(),
            self.share_fps(),
            thousands(self.bitrate_bps() / 1000)
        )
    }

    /// How often previews go out (None: never).
    pub fn preview_interval(&self) -> Option<Duration> {
        (self.preview_minutes > 0)
            .then(|| Duration::from_secs(u64::from(self.preview_minutes) * 60))
    }
}

/// A name as the room keeps it: no control characters, single spaces, at
/// most [`NAME_MAX_CHARS`] characters; None if nothing is left.
pub fn clean_name(text: &str) -> Option<String> {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cut: String = cleaned.chars().take(NAME_MAX_CHARS).collect();
    let cut = cut.trim().to_owned();
    (!cut.is_empty()).then_some(cut)
}

/// Executable names from a comma-separated list ("TeamSpeak.exe, Mumble").
pub fn parse_app_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.to_ascii_lowercase().ends_with(".exe") {
                s.to_owned()
            } else {
                format!("{s}.exe")
            }
        })
        .collect()
}

pub struct SettingsStore {
    path: PathBuf,
    settings: Settings,
}

impl SettingsStore {
    /// The saved settings, and a warning when the file had to be set aside.
    pub fn load(dir: &Path) -> io::Result<(Self, Option<String>)> {
        let path = dir.join(FILE);
        // Bytes, not text: a file that is not even UTF-8 (a disk error, a
        // cut-off save) is damaged like any other, not a failure to start.
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok((
                    Self {
                        path,
                        settings: Settings::default(),
                    },
                    None,
                ));
            }
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<Settings>(super::store::without_bom(&bytes)) {
            Ok(settings) => Ok((Self { path, settings }, None)),
            Err(error) => {
                let aside = dir.join(format!("{FILE}.damaged"));
                std::fs::rename(&path, &aside)?;
                let warning = format!(
                    "{} ({error}); {} {}",
                    tr(
                        "as configurações estavam danificadas",
                        "the settings were damaged"
                    ),
                    tr("guardadas em", "kept in"),
                    aside.display()
                );
                Ok((
                    Self {
                        path,
                        settings: Settings::default(),
                    },
                    Some(warning),
                ))
            }
        }
    }

    pub fn get(&self) -> &Settings {
        &self.settings
    }

    /// Keeps and saves `settings` (temporary file, then renamed).
    pub fn set(&mut self, settings: Settings) -> io::Result<()> {
        if settings == self.settings {
            return Ok(());
        }
        let json = serde_json::to_string_pretty(&settings).map_err(io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, json + "\n")?;
        std::fs::rename(&temporary, &self.path)?;
        self.settings = settings;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nitra-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_then_saved_and_read_back() {
        let dir = temp_dir("roundtrip");
        let (mut store, warning) = SettingsStore::load(&dir).unwrap();
        assert!(warning.is_none());
        assert_eq!(*store.get(), Settings::default());
        assert_eq!(
            store.get().preview_interval(),
            Some(Duration::from_secs(180))
        );
        assert_eq!(store.get().bitrate_bps(), 6_000_000);
        let changed = Settings {
            name: Some("Daniel".into()),
            resolution: Resolution::P1080,
            fps: 30,
            bitrate_kbps: 12_000,
            preview_minutes: 0,
            audio_exclude: vec!["TeamSpeak.exe".into()],
            fluidity: Fluidity::Smooth,
            close_to_tray: false,
            untested_gpu_ok: true,
            volumes: vec![PersonVolume {
                key: "abcd".into(),
                percent: 40,
            }],
            share_panel_hidden: true,
            own_preview: true,
            language: LanguageChoice::English,
        };
        store.set(changed.clone()).unwrap();
        let (again, _) = SettingsStore::load(&dir).unwrap();
        assert_eq!(*again.get(), changed);
        assert_eq!(again.get().preview_interval(), None);
        assert_eq!(again.get().bitrate_bps(), 12_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn volumes_are_kept_per_person_up_to_a_limit() {
        let mut s = Settings::default();
        assert_eq!(s.volume_of("ana"), 100);
        s.set_volume("ana", 40);
        s.set_volume("bia", 0);
        assert_eq!((s.volume_of("ana"), s.volume_of("bia")), (40, 0));
        // Back to 100: forgotten (the file keeps only the changed ones).
        s.set_volume("ana", 100);
        assert_eq!(s.volume_of("ana"), 100);
        assert_eq!(s.volumes.len(), 1);
        // Past the limit the oldest set goes; setting again makes it recent.
        for i in 0..MAX_VOLUMES {
            s.set_volume(&format!("p{i}"), 50);
        }
        assert_eq!(s.volumes.len(), MAX_VOLUMES);
        assert_eq!(s.volume_of("bia"), 100, "the oldest was dropped");
        s.set_volume("p0", 30);
        s.set_volume("new", 20);
        assert_eq!((s.volume_of("p0"), s.volume_of("p1")), (30, 100));
        // A hand-edited file cannot go above 100.
        s.volumes.push(PersonVolume {
            key: "x".into(),
            percent: 250,
        });
        assert_eq!(s.volume_of("x"), 100);
    }

    #[test]
    fn missing_fields_take_defaults_and_a_damaged_file_is_set_aside() {
        let dir = temp_dir("damaged");
        std::fs::write(dir.join(FILE), r#"{ "name": "Ana", "future_field": 1 }"#).unwrap();
        let (store, warning) = SettingsStore::load(&dir).unwrap();
        assert!(warning.is_none());
        assert_eq!(store.get().name.as_deref(), Some("Ana"));
        assert_eq!(store.get().preset(), Some(Preset::Normal));
        std::fs::write(dir.join(FILE), "{ not json").unwrap();
        let (store, warning) = SettingsStore::load(&dir).unwrap();
        assert!(warning.is_some());
        assert_eq!(*store.get(), Settings::default());
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.json.damaged")).unwrap(),
            "{ not json"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mutated_files_load_without_panicking() {
        let dir = temp_dir("mutated");
        let good = serde_json::to_vec(&Settings {
            name: Some("Ana".into()),
            bitrate_kbps: 8_000,
            audio_exclude: vec!["discord.exe".into()],
            ..Settings::default()
        })
        .unwrap();
        let mut mutator = crate::testing::Mutator(0x5EED_1234_ABCD_0009);
        for _ in 0..1_000 {
            std::fs::write(dir.join(FILE), mutator.mutate(&good)).unwrap();
            // A damaged file is set aside (and a default used); whatever
            // loads gives sane values.
            let (store, _) = SettingsStore::load(&dir).unwrap();
            let s = store.get();
            assert!((500_000..=20_000_000).contains(&s.bitrate_bps()));
            assert!(FPS_CHOICES.contains(&s.share_fps()));
            let _ = s.preview_interval();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_byte_order_mark_is_not_damage() {
        let dir = temp_dir("bom");
        std::fs::write(dir.join(FILE), b"\xEF\xBB\xBF{ \"name\": \"Ana\" }").unwrap();
        let (store, warning) = SettingsStore::load(&dir).unwrap();
        assert!(warning.is_none());
        assert_eq!(store.get().name.as_deref(), Some("Ana"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn presets_fill_the_fields_and_any_change_is_manual() {
        crate::i18n::set(crate::i18n::Lang::Pt);
        let mut s = Settings::default();
        assert_eq!(s.preset(), Some(Preset::Normal));
        assert_eq!(s.share_label(), "720p · 60 fps · 6.000 kbps");
        s.apply(Preset::Light);
        assert_eq!(
            (s.resolution.max_size(), s.share_fps(), s.bitrate_bps()),
            ((1280, 720), 30, 2_000_000)
        );
        assert_eq!(s.preset(), Some(Preset::Light));
        s.bitrate_kbps = 2_100;
        assert_eq!(s.preset(), None);
        s.apply(Preset::High);
        s.resolution = Resolution::Source;
        assert_eq!(s.preset(), None);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"source\""), "{json}");
    }

    #[test]
    fn frame_rates_snap_to_the_choices() {
        let mut s = Settings::default();
        for (fps, snapped) in [(0, 15), (29, 15), (30, 30), (59, 30), (60, 60), (144, 60)] {
            s.fps = fps;
            assert_eq!(s.share_fps(), snapped, "{fps}");
        }
    }

    #[test]
    fn names_are_cleaned_like_the_room_does() {
        assert_eq!(clean_name("  Daniel   R. "), Some("Daniel R.".into()));
        assert_eq!(clean_name("a\u{0}b\tc"), Some("a b c".into()));
        assert_eq!(clean_name("   "), None);
        assert_eq!(
            clean_name(&"x".repeat(40)).unwrap().chars().count(),
            NAME_MAX_CHARS
        );
    }

    #[test]
    fn bitrates_stay_in_range_and_app_lists_are_parsed() {
        let mut s = Settings {
            bitrate_kbps: 1,
            ..Settings::default()
        };
        assert_eq!(s.bitrate_bps(), BITRATE_RANGE_KBPS.0 * 1000);
        s.bitrate_kbps = 999_999;
        assert_eq!(s.bitrate_bps(), BITRATE_RANGE_KBPS.1 * 1000);
        assert_eq!(
            parse_app_list(" TeamSpeak.exe, mumble ,,Ventrilo.EXE "),
            vec!["TeamSpeak.exe", "mumble.exe", "Ventrilo.EXE"]
        );
    }
}
