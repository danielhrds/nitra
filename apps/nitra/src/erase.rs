//! "Apagar meus dados deste PC": the window starts this program again with
//! `--erase-profile <dir> --after-pid <its id>` and closes; this run waits
//! for it to end (its log file stays open until then), deletes the profile
//! folder, and the folders above it when nothing else is left in them.
//! It refuses anything that does not look like a nitra profile.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long to wait for the window to close, and to retry a file it still
/// holds.
const WAIT: Duration = Duration::from_secs(30);

pub fn run(dir: &Path, after_pid: Option<u32>) -> Result<(), Box<dyn Error>> {
    check_profile(dir)?;
    if let Some(pid) = after_pid {
        wait_for_exit(pid);
    }
    let started = Instant::now();
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) if started.elapsed() > WAIT => return Err(e.into()),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
    // `profiles`, then the app's folder (`nitra`, `nitra-dev`), if empty now.
    for parent in dir.ancestors().skip(1).take(2) {
        if std::fs::remove_dir(parent).is_err() {
            break;
        }
    }
    Ok(())
}

/// A nitra profile: a folder with an identity key, inside a `profiles` folder.
fn check_profile(dir: &Path) -> Result<(), Box<dyn Error>> {
    let in_profiles = dir
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case("profiles"));
    if !in_profiles || !dir.join("identity.key").is_file() {
        return Err(format!("not a nitra profile folder: {}", dir.display()).into());
    }
    Ok(())
}

/// Waits (bounded) for the process `pid` to end.
pub(crate) fn wait_for_exit(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: a handle opened, waited on and closed here; a process that
    // already ended fails to open, which is fine.
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(handle, WAIT.as_millis() as u32);
            let _ = CloseHandle(handle);
        }
    }
}

/// The arguments that erase `dir` once this process has ended.
pub fn helper_args(dir: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "--erase-profile".into(),
        PathBuf::from(dir).into_os_string(),
        "--after-pid".into(),
        std::process::id().to_string().into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nitra-erase-{name}-{}", std::process::id()))
    }

    #[test]
    fn a_profile_and_its_empty_parents_are_erased() {
        let app = root("ok");
        let profile = app.join("profiles").join("default");
        std::fs::create_dir_all(profile.join("logs")).unwrap();
        std::fs::write(profile.join("identity.key"), "k").unwrap();
        std::fs::write(profile.join("logs").join("app-1.log"), "x").unwrap();
        run(&profile, None).unwrap();
        assert!(!app.exists(), "{}", app.display());
    }

    #[test]
    fn other_profiles_keep_their_folders() {
        let app = root("others");
        let profiles = app.join("profiles");
        for name in ["default", "other"] {
            std::fs::create_dir_all(profiles.join(name)).unwrap();
            std::fs::write(profiles.join(name).join("identity.key"), "k").unwrap();
        }
        run(&profiles.join("default"), None).unwrap();
        assert!(!profiles.join("default").exists());
        assert!(profiles.join("other").join("identity.key").is_file());
        let _ = std::fs::remove_dir_all(&app);
    }

    #[test]
    fn anything_else_is_refused() {
        let app = root("refused");
        // No identity key; not under `profiles`.
        let plain = app.join("profiles").join("nokey");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(run(&plain, None).is_err());
        let elsewhere = app.join("documents");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("identity.key"), "k").unwrap();
        assert!(run(&elsewhere, None).is_err());
        assert!(plain.exists() && elsewhere.exists());
        let _ = std::fs::remove_dir_all(&app);
    }
}
