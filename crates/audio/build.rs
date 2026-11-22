//! Links libopus statically. The library comes from vcpkg, built for the
//! same C runtime as the executables: `x64-windows-static` when the runtime
//! is linked in (`crt-static`, the workspace default, so users need no
//! Visual C++ Redistributable), `x64-windows-static-md` otherwise. The app
//! ships it inside the executable; users install nothing.
//!
//! Where to look, in order: `OPUS_LIB_DIR` (a folder with `opus.lib`), then
//! `VCPKG_ROOT` with the triplet above.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=OPUS_LIB_DIR");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
    let static_crt = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .is_ok_and(|features| features.split(',').any(|f| f == "crt-static"));
    let triplet = if static_crt {
        "x64-windows-static"
    } else {
        "x64-windows-static-md"
    };
    let dir = std::env::var_os("OPUS_LIB_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("VCPKG_ROOT").map(|root| {
                PathBuf::from(root)
                    .join("installed")
                    .join(triplet)
                    .join("lib")
            })
        })
        .filter(|dir| dir.join("opus.lib").is_file())
        .unwrap_or_else(|| {
            panic!(
                "opus.lib not found: install it with `vcpkg install opus:{triplet}` \
                 and set VCPKG_ROOT (or OPUS_LIB_DIR to the folder with opus.lib)"
            )
        });
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=opus");
}
