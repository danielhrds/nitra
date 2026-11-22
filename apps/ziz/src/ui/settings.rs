//! The window's settings, saved in the profile (`settings.json`): the name
//! others see in the rooms, the sharing quality, how often previews go out,
//! apps left out of a whole-screen share's sound, and what the close button
//! does. Missing fields take their defaults; a damaged file is set aside
//! (renamed, never overwritten) and the defaults are used.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

const FILE: &str = "settings.json";
/// The room server's limit for names (characters).
pub const NAME_MAX_CHARS: usize = 32;

/// What a shared screen is sent as. The size is the largest the picture
/// fits in, keeping its aspect ratio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    #[default]
    P720At60,
    P1080At60,
    P1080At30,
    P1440At60,
}

impl Quality {
    pub const ALL: [Self; 4] = [
        Self::P720At60,
        Self::P1080At60,
        Self::P1080At30,
        Self::P1440At60,
    ];

    pub fn max_size(self) -> (u32, u32) {
        match self {
            Self::P720At60 => (1280, 720),
            Self::P1080At60 | Self::P1080At30 => (1920, 1080),
            Self::P1440At60 => (2560, 1440),
        }
    }

    pub fn fps(self) -> u32 {
        match self {
            Self::P1080At30 => 30,
            _ => 60,
        }
    }

    /// The preset's bitrate (the advanced setting can change it).
    pub fn bitrate_kbps(self) -> u32 {
        match self {
            Self::P720At60 | Self::P1080At30 => 6_000,
            Self::P1080At60 => 10_000,
            Self::P1440At60 => 16_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::P720At60 => "720p · 60 fps",
            Self::P1080At60 => "1080p · 60 fps",
            Self::P1080At30 => "1080p · 30 fps",
            Self::P1440At60 => "1440p · 60 fps",
        }
    }
}

/// The bitrates the advanced setting allows (kbps).
pub const BITRATE_RANGE_KBPS: (u32, u32) = (1_000, 50_000);
/// The preview intervals offered (minutes; 0 = no previews).
pub const PREVIEW_CHOICES: [u32; 5] = [1, 3, 5, 10, 0];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The name others see; None until chosen (asked on the first start).
    pub name: Option<String>,
    pub quality: Quality,
    /// Advanced: a bitrate instead of the preset's (kbps).
    pub bitrate_kbps: Option<u32>,
    /// Minutes between previews; 0 = none.
    pub preview_minutes: u32,
    /// Apps a whole-screen share leaves out of its sound, besides Discord
    /// and ziz (executable names, e.g. `TeamSpeak.exe`).
    pub audio_exclude: Vec<String>,
    /// The close button hides the window to the tray (quit from its menu).
    pub close_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            name: None,
            quality: Quality::default(),
            bitrate_kbps: None,
            preview_minutes: 3,
            audio_exclude: Vec::new(),
            close_to_tray: true,
        }
    }
}

impl Settings {
    /// The bitrate to share at, in bits per second.
    pub fn bitrate_bps(&self) -> u32 {
        let kbps = self
            .bitrate_kbps
            .unwrap_or_else(|| self.quality.bitrate_kbps());
        kbps.clamp(BITRATE_RANGE_KBPS.0, BITRATE_RANGE_KBPS.1) * 1000
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
        match serde_json::from_slice::<Settings>(&bytes) {
            Ok(settings) => Ok((Self { path, settings }, None)),
            Err(error) => {
                let aside = dir.join(format!("{FILE}.damaged"));
                std::fs::rename(&path, &aside)?;
                let warning = format!(
                    "as configurações estavam danificadas ({error}); guardadas em {}",
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
        let dir = std::env::temp_dir().join(format!("ziz-settings-{name}-{}", std::process::id()));
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
            quality: Quality::P1080At60,
            bitrate_kbps: Some(12_000),
            preview_minutes: 0,
            audio_exclude: vec!["TeamSpeak.exe".into()],
            close_to_tray: false,
        };
        store.set(changed.clone()).unwrap();
        let (again, _) = SettingsStore::load(&dir).unwrap();
        assert_eq!(*again.get(), changed);
        assert_eq!(again.get().preview_interval(), None);
        assert_eq!(again.get().bitrate_bps(), 12_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_fields_take_defaults_and_a_damaged_file_is_set_aside() {
        let dir = temp_dir("damaged");
        std::fs::write(dir.join(FILE), r#"{ "name": "Ana", "future_field": 1 }"#).unwrap();
        let (store, warning) = SettingsStore::load(&dir).unwrap();
        assert!(warning.is_none());
        assert_eq!(store.get().name.as_deref(), Some("Ana"));
        assert_eq!(store.get().quality, Quality::P720At60);
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
            bitrate_kbps: Some(8_000),
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
            assert!((1_000_000..=50_000_000).contains(&s.bitrate_bps()));
            let _ = s.preview_interval();
        }
        let _ = std::fs::remove_dir_all(&dir);
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
            bitrate_kbps: Some(1),
            ..Settings::default()
        };
        assert_eq!(s.bitrate_bps(), BITRATE_RANGE_KBPS.0 * 1000);
        s.bitrate_kbps = Some(999_999);
        assert_eq!(s.bitrate_bps(), BITRATE_RANGE_KBPS.1 * 1000);
        assert_eq!(
            parse_app_list(" TeamSpeak.exe, mumble ,,Ventrilo.EXE "),
            vec!["TeamSpeak.exe", "mumble.exe", "Ventrilo.EXE"]
        );
    }
}
