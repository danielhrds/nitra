//! Profiles: each running instance has its own data directory, logs and id,
//! so several instances (host + viewers) can run side by side on one PC.
//! There is no single-instance lock. The persistent cryptographic identity
//! comes later; for now a profile has a random id.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};

pub struct Profile {
    pub name: String,
    pub dir: PathBuf,
    pub id: String,
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
        let id_file = dir.join("id");
        let id = match std::fs::read_to_string(&id_file) {
            Ok(id) if !id.trim().is_empty() => id.trim().to_owned(),
            _ => {
                let id = random_id();
                std::fs::write(&id_file, &id)?;
                id
            }
        };
        Ok(Self {
            name: name.to_owned(),
            dir,
            id,
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

/// 128 random bits from the standard library's randomly keyed hasher
/// (not for secrets; just a distinct name).
fn random_id() -> String {
    let half = || {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .elapsed()
                .map_or(0, |d| d.as_nanos()),
        );
        hasher.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_separate_and_keep_their_id() {
        let root = std::env::temp_dir().join(format!("ziz-profile-test-{}", std::process::id()));
        let host = Profile::open(&root, "test-host").unwrap();
        let viewer = Profile::open(&root, "test-viewer").unwrap();
        assert_ne!(host.dir, viewer.dir);
        assert_ne!(host.id, viewer.id);
        assert_eq!(host.id.len(), 32);
        assert!(host.dir.join("logs").is_dir());
        assert_eq!(Profile::open(&root, "test-host").unwrap().id, host.id);
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
