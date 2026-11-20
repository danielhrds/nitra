//! Links libopus statically. The library comes from vcpkg
//! (`vcpkg install opus:x64-windows-static-md`); the app ships it inside
//! the executable, users install nothing.
//!
//! Where to look, in order: `OPUS_LIB_DIR` (a folder with `opus.lib`), then
//! `VCPKG_ROOT` with the `x64-windows-static-md` triplet.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=OPUS_LIB_DIR");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
    let dir = std::env::var_os("OPUS_LIB_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("VCPKG_ROOT").map(|root| {
                PathBuf::from(root)
                    .join("installed")
                    .join("x64-windows-static-md")
                    .join("lib")
            })
        })
        .filter(|dir| dir.join("opus.lib").is_file())
        .unwrap_or_else(|| {
            panic!(
                "opus.lib not found: install it with `vcpkg install opus:x64-windows-static-md` \
                 and set VCPKG_ROOT (or OPUS_LIB_DIR to the folder with opus.lib)"
            )
        });
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=opus");
}
