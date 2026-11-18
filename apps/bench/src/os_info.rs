//! Windows edition and build, read from the registry.
//!
//! `GetVersionEx` is subject to compatibility shims, so the registry is used
//! instead. `ProductName` is not read because it still says "Windows 10" on
//! Windows 11.

use serde::Serialize;
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::core::{HSTRING, PCWSTR, w};

const CURRENT_VERSION_KEY: PCWSTR = w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OsInfo {
    /// e.g. `Professional`.
    pub edition: Option<String>,
    /// e.g. `25H2`.
    pub display_version: Option<String>,
    /// e.g. `26200.7171`.
    pub build: Option<String>,
}

impl OsInfo {
    pub fn read() -> Self {
        let build = read_string("CurrentBuild").map(|build| match read_dword("UBR") {
            Some(revision) => format!("{build}.{revision}"),
            None => build,
        });
        Self {
            edition: read_string("EditionID"),
            display_version: read_string("DisplayVersion"),
            build,
        }
    }
}

fn read_string(value: &str) -> Option<String> {
    let value = HSTRING::from(value);
    let mut size_bytes = 0_u32;
    // SAFETY: static key path and a live value name; a null data pointer asks for the size only.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            CURRENT_VERSION_KEY,
            &value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size_bytes),
        )
    }
    .ok()
    .ok()?;

    let mut buffer = vec![0_u16; (size_bytes as usize).div_ceil(2)];
    // SAFETY: `buffer` holds `size_bytes` bytes, as reported by the previous call.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            CURRENT_VERSION_KEY,
            &value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size_bytes),
        )
    }
    .ok()
    .ok()?;
    Some(gpu::wide_to_string(&buffer))
}

fn read_dword(value: &str) -> Option<u32> {
    let value = HSTRING::from(value);
    let mut data = 0_u32;
    let mut size_bytes = size_of::<u32>() as u32;
    // SAFETY: `data` is a live u32 and `size_bytes` matches its size.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            CURRENT_VERSION_KEY,
            &value,
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size_bytes),
        )
    }
    .ok()
    .ok()?;
    Some(data)
}
