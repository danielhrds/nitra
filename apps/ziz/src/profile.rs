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

    pub fn log_path(&self, role: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.dir.join("logs").join(format!("{role}-{stamp}.log"))
    }
}

/// Default root: `%LOCALAPPDATA%\ziz-dev\profiles` (development builds).
pub fn default_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("ziz-dev")
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
    fn unsafe_names_are_refused() {
        let root = std::env::temp_dir();
        for name in ["", "..", "a/b", r"a\b", "x y", &"n".repeat(65)] {
            assert!(Profile::open(&root, name).is_err(), "{name:?}");
        }
    }
}
