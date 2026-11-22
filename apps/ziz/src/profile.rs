//! Profiles: each running instance has its own data directory, logs and
//! identity, so several instances (host + viewers) can run side by side on
//! one PC. There is no single-instance lock.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use identity::Identity;

pub struct Profile {
    pub name: String,
    pub dir: PathBuf,
    /// This profile's key pair, created on first use (`identity.key`, DPAPI).
    pub identity: Arc<Identity>,
}

impl Profile {
    /// Opens (creating if needed) the profile `name` under `root`.
    pub fn open(root: &Path, name: &str) -> io::Result<Self> {
        let valid = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile names use letters, digits, '-' and '_' (at most 64)",
            ));
        }
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("logs"))?;
        let identity = Arc::new(Identity::load_or_create(&dir.join("identity.key"))?);
        Ok(Self {
            name: name.to_owned(),
            dir,
            identity,
        })
    }

    /// A new log file's path; the oldest logs go first, so that with the
    /// new one at most [`LOGS_KEPT`] remain (one per start: they would grow
    /// without end).
    pub fn log_path(&self, role: &str) -> PathBuf {
        let logs = self.dir.join("logs");
        prune_logs(&logs, LOGS_KEPT - 1);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        logs.join(format!("{role}-{stamp}.log"))
    }
}

/// How many logs a profile keeps, the current one included.
const LOGS_KEPT: usize = 10;

/// Deletes the oldest `.log` files in `dir` until at most `keep` are left.
/// A log still open (another running instance) cannot be deleted and stays.
fn prune_logs(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if logs.len() <= keep {
        return;
    }
    logs.sort();
    for (_, path) in &logs[..logs.len() - keep] {
        let _ = std::fs::remove_file(path);
    }
}

/// Default root: `%LOCALAPPDATA%\ziz\profiles` in a beta build,
/// `%LOCALAPPDATA%\ziz-dev\profiles` in development ones (their data never
/// mixes).
pub fn default_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join(if crate::BETA { "ziz" } else { "ziz-dev" })
        .join("profiles")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_separate_and_keep_their_identity() {
        let root = std::env::temp_dir().join(format!("ziz-profile-test-{}", std::process::id()));
        let host = Profile::open(&root, "test-host").unwrap();
        let viewer = Profile::open(&root, "test-viewer").unwrap();
        assert_ne!(host.dir, viewer.dir);
        assert_ne!(host.identity.public_key(), viewer.identity.public_key());
        assert!(host.dir.join("logs").is_dir());
        assert!(host.dir.join("identity.key").is_file());
        assert_eq!(
            Profile::open(&root, "test-host")
                .unwrap()
                .identity
                .public_key(),
            host.identity.public_key()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_the_newest_logs_are_kept() {
        let root = std::env::temp_dir().join(format!("ziz-logs-test-{}", std::process::id()));
        let profile = Profile::open(&root, "logs").unwrap();
        let logs = profile.dir.join("logs");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for i in 0..14 {
            let path = logs.join(format!("app-{i}.log"));
            std::fs::write(&path, "x").unwrap();
            // Older names, older times: app-0 is the oldest.
            let when = old + std::time::Duration::from_secs(i);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        std::fs::write(logs.join("notes.txt"), "kept").unwrap();
        let next = profile.log_path("app");
        std::fs::write(&next, "new").unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&logs)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".log"))
            .collect();
        left.sort();
        assert_eq!(left.len(), LOGS_KEPT, "{left:?}");
        assert!(!left.contains(&"app-4.log".to_owned()), "{left:?}");
        assert!(left.contains(&"app-5.log".to_owned()), "{left:?}");
        assert!(logs.join("notes.txt").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unsafe_names_are_refused() {
        let root = std::env::temp_dir();
        for name in ["", "..", "a/b", r"a\b", "x y", &"n".repeat(65)] {
            assert!(Profile::open(&root, name).is_err(), "{name:?}");
        }
    }
}
