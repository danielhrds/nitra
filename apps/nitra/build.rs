//! Embeds the app icon (`assets/nitra.ico`, made by `scripts/make-icon.ps1`)
//! as resource 1 of `nitra.exe`, for Explorer, the taskbar and Alt+Tab, and
//! the version (the file's Properties > Details) from `Cargo.toml`. Uses
//! the Windows SDK's resource compiler (`rc.exe`, or the one `RC` names);
//! without it the build still works, with the generic icon and a warning.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/nitra.ico");
    println!("cargo:rerun-if-env-changed=RC");
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe (Windows SDK) not found: nitra.exe gets no icon");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    // Read when the script runs, not baked in when it is compiled: a build
    // script compiled before the folder moved would point at the old place.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let icon = Path::new(&manifest).join("assets").join("nitra.ico");
    let script = out.join("nitra.rc");
    std::fs::write(
        &script,
        format!(
            "1 ICON \"{}\"\n{}",
            icon.display().to_string().replace('\\', "\\\\"),
            version_info()
        ),
    )
    .expect("writing nitra.rc");
    let res = out.join("nitra.res");
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

/// The VERSIONINFO resource: numbers from the crate version (the
/// pre-release tag, e.g. `beta.1`, only shows in the text fields).
fn version_info() -> String {
    let part = |name: &str| std::env::var(name).unwrap_or_else(|_| "0".to_owned());
    let numbers = format!(
        "{},{},{},0",
        part("CARGO_PKG_VERSION_MAJOR"),
        part("CARGO_PKG_VERSION_MINOR"),
        part("CARGO_PKG_VERSION_PATCH")
    );
    let text = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    format!(
        "1 VERSIONINFO\nFILEVERSION {numbers}\nPRODUCTVERSION {numbers}\nFILEOS 0x40004\nFILETYPE 0x1\n\
         BEGIN\n BLOCK \"StringFileInfo\"\n BEGIN\n  BLOCK \"040904B0\"\n  BEGIN\n\
         VALUE \"CompanyName\", \"nitra\"\n\
         VALUE \"FileDescription\", \"nitra\"\n\
         VALUE \"FileVersion\", \"{text}\"\n\
         VALUE \"ProductName\", \"nitra\"\n\
         VALUE \"ProductVersion\", \"{text}\"\n\
         VALUE \"OriginalFilename\", \"nitra.exe\"\n\
         END\n END\n BLOCK \"VarFileInfo\"\n BEGIN\n  VALUE \"Translation\", 0x409, 1200\n END\nEND\n"
    )
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
