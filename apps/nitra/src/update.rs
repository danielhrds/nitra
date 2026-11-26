//! Updates from the GitHub releases. The app asks for the latest release
//! (at start, then once a day); when it is newer, it asks the person; on
//! their go it downloads the release's `nitra.exe`, `SHA256SUMS.txt` and
//! `SHA256SUMS.txt.sig`, installs it only if the signature checks out with
//! the release key built in here and the executable matches the signed
//! checksum, swaps its own executable and starts the new one.
//!
//! Nothing but the request itself reaches GitHub (no name, key or room):
//! GitHub sees the address it comes from, as with any website.

use std::cmp::Ordering;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use identity::PublicKey;
use identity::release::{sha256, unhex, verify};
use rooms::http::{self, HttpUrl};

/// The public half of the release key (`nitra-cli --release-keygen`): only a
/// release signed by its private half is installed.
const RELEASE_PUBLIC_KEY: &str = "27dk-jzrv-xztr-zzru-3cqn-izck-fzaz-6gck-l3dr-xjer-2adt-2eii-tgtq";
/// Where the latest release is asked for.
const LATEST_RELEASE: &str = "https://api.github.com/repos/danielhrds/nitra/releases/latest";
const EXECUTABLE: &str = "nitra.exe";
const CHECKSUMS: &str = "SHA256SUMS.txt";
const SIGNATURE: &str = "SHA256SUMS.txt.sig";
/// How often a running app asks again.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Larger answers are refused (the executable is about 14 MB).
const MAX_EXECUTABLE: usize = 100 * 1024 * 1024;
const MAX_SMALL: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// The key releases are checked against. A development build can take a
/// test key from `NITRA_TEST_RELEASE_KEY` (scripted tests); a beta build
/// trusts only the built-in one.
pub fn release_key() -> Option<PublicKey> {
    if !crate::BETA
        && let Ok(code) = std::env::var("NITRA_TEST_RELEASE_KEY")
    {
        return code.parse().ok();
    }
    RELEASE_PUBLIC_KEY.parse().ok()
}

/// Where the latest release is asked for; a development build can point
/// it at a local test server (`NITRA_TEST_UPDATE_URL`).
fn latest_release_url() -> String {
    if !crate::BETA
        && let Ok(url) = std::env::var("NITRA_TEST_UPDATE_URL")
    {
        return url;
    }
    LATEST_RELEASE.to_owned()
}

/// A version like `0.1.0-beta.4` (a release's tag may start with `v`).
/// Ordered as semantic versions: a pre-release comes before its release,
/// and `beta.10` after `beta.9`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    numbers: [u64; 3],
    /// The pre-release parts (`beta`, `4`); empty for a release.
    pre: Vec<String>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().strip_prefix('v').unwrap_or(text.trim());
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut numbers = [0; 3];
        let mut parts = core.split('.');
        for number in &mut numbers {
            *number = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            Some("") => return None,
            Some(pre) => pre.split('.').map(str::to_owned).collect(),
            None => Vec::new(),
        };
        Some(Self { numbers, pre })
    }

    /// This build's version.
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("the workspace version is valid")
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers.cmp(&other.numbers).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(a), Ok(b)) => a.cmp(&b),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.numbers;
        write!(f, "{major}.{minor}.{patch}")?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

/// A release that can be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Its page (release notes), opened by "see what's new".
    pub page: String,
    executable: String,
    checksums: String,
    signature: String,
}

/// The release in GitHub's answer, if it has the three files; None for a
/// draft, a pre-release or one missing a file.
fn parse_release(json: &[u8]) -> Option<Release> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    if value["draft"].as_bool() == Some(true) || value["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let version = Version::parse(value["tag_name"].as_str()?)?;
    let asset = |name: &str| {
        value["assets"].as_array()?.iter().find_map(|a| {
            (a["name"].as_str() == Some(name))
                .then(|| a["browser_download_url"].as_str().map(str::to_owned))
                .flatten()
        })
    };
    Some(Release {
        version,
        page: value["html_url"].as_str().unwrap_or_default().to_owned(),
        executable: asset(EXECUTABLE)?,
        checksums: asset(CHECKSUMS)?,
        signature: asset(SIGNATURE)?,
    })
}

/// The latest release, when it is newer than this build.
pub fn check() -> io::Result<Option<Release>> {
    let url = HttpUrl::parse(&latest_release_url())
        .ok_or_else(|| io::Error::other("bad update address"))?;
    let body = http::get(
        &url,
        "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28",
        TIMEOUT,
        MAX_SMALL,
        |_, _| {},
    )?;
    Ok(parse_release(&body).filter(|r| r.version > Version::current()))
}

/// Why an update was not installed.
#[derive(Debug)]
pub enum UpdateError {
    Download(io::Error),
    /// The signature does not check out with the built-in key, or the
    /// executable is not the one it signed.
    NotTrusted(&'static str),
    Install(io::Error),
}

impl UpdateError {
    /// What the update dialog says, in the app's language (the details of
    /// a download or file error stay as Windows gives them).
    pub fn message(&self) -> String {
        use crate::i18n::tr;
        match self {
            Self::Download(e) => format!("{}: {e}", tr("O download falhou", "The download failed")),
            Self::NotTrusted(why) => {
                let pt = match *why {
                    NOT_SIGNED => "a release não está assinada com a chave do nitra",
                    NOT_THE_SIGNED_EXE => "o executável não é o que a release assinou",
                    NO_CHECKSUM => "a lista assinada não traz o nitra.exe",
                    DAMAGED_SIGNATURE => "o arquivo de assinatura está danificado",
                    _ => "este nitra não tem a chave de release",
                };
                format!("{}: {}", tr("Não instalada", "Not installed"), tr(pt, why))
            }
            Self::Install(e) => format!(
                "{}: {e}",
                tr(
                    "Não deu para trocar o executável",
                    "Could not replace the executable"
                )
            ),
        }
    }
}

const NOT_SIGNED: &str = "the release is not signed with nitra's release key";
const NOT_THE_SIGNED_EXE: &str = "the executable is not the one the release signed";
const NO_CHECKSUM: &str = "the signed checksums do not list nitra.exe";
const DAMAGED_SIGNATURE: &str = "the signature file is damaged";

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Download(e) => write!(f, "download failed: {e}"),
            Self::NotTrusted(why) => write!(f, "not installed: {why}"),
            Self::Install(e) => write!(f, "could not replace the executable: {e}"),
        }
    }
}

/// Downloads `release` and returns its executable once it checks out.
/// `progress` gets (bytes so far, total) while the executable downloads.
pub fn download(
    release: &Release,
    progress: impl FnMut(usize, Option<usize>),
) -> Result<Vec<u8>, UpdateError> {
    let key = release_key().ok_or(UpdateError::NotTrusted("this build has no release key"))?;
    let fetch = |url: &str, max: usize, progress: &mut dyn FnMut(usize, Option<usize>)| {
        let url = HttpUrl::parse(url).ok_or_else(|| io::Error::other("bad download address"))?;
        http::get(&url, "", TIMEOUT, max, progress)
    };
    let checksums =
        fetch(&release.checksums, MAX_SMALL, &mut |_, _| {}).map_err(UpdateError::Download)?;
    let signature =
        fetch(&release.signature, MAX_SMALL, &mut |_, _| {}).map_err(UpdateError::Download)?;
    let mut progress = progress;
    let executable =
        fetch(&release.executable, MAX_EXECUTABLE, &mut progress).map_err(UpdateError::Download)?;
    trusted(&key, &checksums, &signature, &executable)?;
    Ok(executable)
}

/// Checks the signature of the checksums, then the executable against them.
fn trusted(
    key: &PublicKey,
    checksums: &[u8],
    signature: &[u8],
    executable: &[u8],
) -> Result<(), UpdateError> {
    let signature: [u8; identity::SIGNATURE_LENGTH] = std::str::from_utf8(signature)
        .ok()
        .and_then(unhex)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(UpdateError::NotTrusted(DAMAGED_SIGNATURE))?;
    if !verify(key, checksums, &signature) {
        return Err(UpdateError::NotTrusted(NOT_SIGNED));
    }
    let listed =
        listed_checksum(checksums, EXECUTABLE).ok_or(UpdateError::NotTrusted(NO_CHECKSUM))?;
    let actual = sha256(executable).map_err(UpdateError::Install)?;
    if actual != listed {
        return Err(UpdateError::NotTrusted(NOT_THE_SIGNED_EXE));
    }
    Ok(())
}

/// `name`'s SHA-256 in a `SHA256SUMS.txt` ("<hex>  <name>" lines).
fn listed_checksum(checksums: &[u8], name: &str) -> Option<[u8; 32]> {
    std::str::from_utf8(checksums)
        .ok()?
        .lines()
        .find_map(|line| {
            let (hash, file) = line.trim().split_once(char::is_whitespace)?;
            (file.trim().trim_start_matches('*') == name)
                .then(|| unhex(hash)?.try_into().ok())
                .flatten()
        })
}

/// `path` with `.new` / `.old` added.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// Puts `executable` in place of the file at `path` (the running one): the
/// new file is written next to it, the running one renamed out of the way
/// (Windows lets a running executable be renamed, not overwritten), then
/// the new one renamed in. Any failure puts the old one back.
pub fn install(path: &Path, executable: &[u8]) -> io::Result<()> {
    let new = sibling(path, ".new");
    let old = sibling(path, ".old");
    std::fs::write(&new, executable)?;
    let _ = std::fs::remove_file(&old);
    if let Err(error) = std::fs::rename(path, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&new, path) {
        let _ = std::fs::rename(&old, path);
        let _ = std::fs::remove_file(&new);
        return Err(error);
    }
    Ok(())
}

/// Deletes what the last update left (the old executable, once it is no
/// longer running). Called at start.
pub fn clean_up() {
    if let Ok(path) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&path, ".old"));
        let _ = std::fs::remove_file(sibling(&path, ".new"));
    }
}

/// Starts the installed executable with this run's arguments; it waits for
/// this process to end before it opens its window.
pub fn relaunch(path: &Path) -> io::Result<()> {
    let mut arguments: Vec<String> = Vec::new();
    let mut skip = false;
    for argument in std::env::args().skip(1) {
        if skip {
            skip = false;
            continue;
        }
        if argument == "--after-pid" {
            skip = true;
            continue;
        }
        arguments.push(argument);
    }
    arguments.push("--after-pid".into());
    arguments.push(std::process::id().to_string());
    std::process::Command::new(path)
        .args(arguments)
        .spawn()
        .map(drop)
}

/// What the update thread tells the window.
#[derive(Debug)]
pub enum UpdateEvent {
    Available(Release),
    Progress {
        done: usize,
        total: Option<usize>,
    },
    /// Installed: the window starts the new one and closes.
    Installed(PathBuf),
    Failed(String),
}

/// Asks for the latest release now and then once a day while `enabled`,
/// telling `events` (and waking the window with `wake`) when it is newer.
/// A failed check is only logged: the next one tries again.
pub fn start_checks(
    events: Sender<UpdateEvent>,
    enabled: Arc<AtomicBool>,
    wake: impl Fn() + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            loop {
                if enabled.load(AtomicOrdering::Relaxed) {
                    match check() {
                        Ok(Some(release)) => {
                            tracing::info!(version = %release.version, "update available");
                            if events.send(UpdateEvent::Available(release)).is_err() {
                                return;
                            }
                            wake();
                        }
                        Ok(None) => tracing::info!("no newer release"),
                        Err(error) => tracing::info!(%error, "update check failed"),
                    }
                }
                std::thread::sleep(CHECK_EVERY);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "update check thread");
    }
}

/// Downloads, checks and installs `release` off the window's thread.
pub fn start_install(
    release: Release,
    events: Sender<UpdateEvent>,
    wake: impl Fn() + Send + Sync + 'static,
) {
    let wake = Arc::new(wake);
    let spawned = std::thread::Builder::new()
        .name("update-install".into())
        .spawn(move || {
            let progress_events = events.clone();
            let progress_wake = Arc::clone(&wake);
            let mut last = 0;
            let result = download(&release, move |done, total| {
                // A few updates a second are plenty for a progress line.
                if done - last >= 256 * 1024 || Some(done) == total {
                    last = done;
                    let _ = progress_events.send(UpdateEvent::Progress { done, total });
                    progress_wake();
                }
            })
            .and_then(|executable| {
                let path = std::env::current_exe().map_err(UpdateError::Install)?;
                install(&path, &executable).map_err(UpdateError::Install)?;
                Ok(path)
            });
            let event = match result {
                Ok(path) => {
                    tracing::info!(version = %release.version, "update installed");
                    UpdateEvent::Installed(path)
                }
                Err(error) => {
                    tracing::warn!(%error, "update not installed");
                    UpdateEvent::Failed(error.message())
                }
            };
            let _ = events.send(event);
            wake();
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "update install thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity::release::{ReleaseKey, hex};

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_order_like_semantic_versions() {
        assert!(v("0.1.0-beta.5") > v("0.1.0-beta.4"));
        assert!(v("0.1.0-beta.10") > v("0.1.0-beta.9"));
        assert!(v("0.1.0") > v("0.1.0-beta.99"));
        assert!(v("0.2.0-beta.1") > v("0.1.9"));
        assert!(v("v0.1.0-beta.4") == v("0.1.0-beta.4"));
        assert!(v("0.1.0-rc.1") > v("0.1.0-beta.9"));
        assert_eq!(v("v1.2.3-beta.4").to_string(), "1.2.3-beta.4");
        for bad in ["", "1.2", "1.2.3.4", "1.2.x", "1.2.3-"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    fn release_json(tag: &str, draft: bool, prerelease: bool, files: &[&str]) -> Vec<u8> {
        let assets: Vec<serde_json::Value> = files
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "browser_download_url": format!("https://example.test/{name}"),
                })
            })
            .collect();
        serde_json::json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "html_url": "https://example.test/release",
            "assets": assets,
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn a_release_needs_its_three_files_and_to_be_published() {
        let all = [EXECUTABLE, CHECKSUMS, SIGNATURE];
        let release = parse_release(&release_json("v0.1.0-beta.5", false, false, &all)).unwrap();
        assert_eq!(release.version, v("0.1.0-beta.5"));
        assert_eq!(release.executable, "https://example.test/nitra.exe");
        assert_eq!(release.page, "https://example.test/release");
        assert!(parse_release(&release_json("v0.1.0-beta.5", true, false, &all)).is_none());
        assert!(parse_release(&release_json("v0.1.0-beta.5", false, true, &all)).is_none());
        let unsigned = [EXECUTABLE, CHECKSUMS];
        assert!(parse_release(&release_json("v0.1.0-beta.5", false, false, &unsigned)).is_none());
        assert!(parse_release(b"not json").is_none());
    }

    /// A signed release: (checksums, signature, executable).
    fn signed(key: &ReleaseKey, executable: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let sums = format!("{}  nitra.exe\n", hex(&sha256(executable).unwrap())).into_bytes();
        let signature = format!("{}\n", hex(&key.sign(&sums))).into_bytes();
        (sums, signature)
    }

    #[test]
    fn only_a_signed_and_matching_executable_is_trusted() {
        let key = ReleaseKey::generate().unwrap();
        let other = ReleaseKey::generate().unwrap();
        let executable = b"MZ the new nitra".to_vec();
        let (sums, signature) = signed(&key, &executable);
        assert!(trusted(key.public_key(), &sums, &signature, &executable).is_ok());
        // Another key's signature.
        let (_, foreign) = signed(&other, &executable);
        assert!(trusted(key.public_key(), &sums, &foreign, &executable).is_err());
        // An executable swapped after signing.
        assert!(trusted(key.public_key(), &sums, &signature, b"MZ something else").is_err());
        // Checksums changed to match a swapped executable, signature kept.
        let (swapped_sums, _) = signed(&key, b"MZ something else");
        assert!(
            trusted(
                key.public_key(),
                &swapped_sums,
                &signature,
                b"MZ something else"
            )
            .is_err()
        );
        assert!(trusted(key.public_key(), &sums, b"zz", &executable).is_err());
    }

    #[test]
    fn the_checksum_is_found_by_name() {
        let hash = hex(&[7; 32]);
        let sums = format!("{}  other.exe\n{hash} *nitra.exe\n", hex(&[1; 32]));
        assert_eq!(listed_checksum(sums.as_bytes(), "nitra.exe"), Some([7; 32]));
        assert_eq!(listed_checksum(b"", "nitra.exe"), None);
    }

    #[test]
    fn install_swaps_the_file_and_keeps_the_old_one_aside() {
        let dir = std::env::temp_dir().join(format!("nitra-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nitra.exe");
        std::fs::write(&path, b"old").unwrap();
        install(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(sibling(&path, ".old")).unwrap(), b"old");
        assert!(!sibling(&path, ".new").exists());
        // A missing executable fails and leaves nothing behind.
        let missing = dir.join("missing.exe");
        assert!(install(&missing, b"new").is_err());
        assert!(!sibling(&missing, ".new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
