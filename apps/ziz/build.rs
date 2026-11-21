//! Embeds the app icon (`assets/ziz.ico`, made by `scripts/make-icon.ps1`)
//! as resource 1 of `ziz.exe`, for Explorer, the taskbar and Alt+Tab. Uses
//! the Windows SDK's resource compiler (`rc.exe`, or the one `RC` names);
//! without it the build still works, with the generic icon and a warning.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/ziz.ico");
    println!("cargo:rerun-if-env-changed=RC");
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe (Windows SDK) not found: ziz.exe gets no icon");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let icon = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("ziz.ico");
    let script = out.join("ziz.rc");
    std::fs::write(
        &script,
        format!(
            "1 ICON \"{}\"\n",
            icon.display().to_string().replace('\\', "\\\\")
        ),
    )
    .expect("writing ziz.rc");
    let res = out.join("ziz.res");
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&script)
        .status()
        .expect("running rc.exe");
    assert!(status.success(), "rc.exe failed on {}", script.display());
    println!("cargo:rustc-link-arg-bins={}", res.display());
}

/// `RC` if set, else the newest x64 `rc.exe` of the Windows 10/11 SDK.
fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = std::env::var_os("RC") {
        return Some(PathBuf::from(rc));
    }
    let bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(bin)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("x64").join("rc.exe").is_file())
        .collect();
    versions.sort();
    versions.pop().map(|v| v.join("x64").join("rc.exe"))
}
