//! Puts the short summary on the Windows clipboard, so a friend can paste
//! it into a chat. (The same code as the nitra app's clipboard module, which
//! this small program does not depend on.)

use std::io;
use std::time::Duration;

use windows::Win32::Foundation::{GlobalFree, HANDLE};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// Holds the clipboard open; closes it when dropped.
struct Open;

impl Open {
    /// Another program may hold the clipboard for a moment: retry briefly.
    fn new() -> io::Result<Self> {
        let mut last = None;
        for _ in 0..20 {
            // SAFETY: no owner window; balanced by `CloseClipboard` in drop.
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(io::Error::other(format!(
            "the clipboard is busy: {}",
            last.map_or_else(String::new, |e| e.to_string())
        )))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: the clipboard was opened by `Open::new`.
        let _ = unsafe { CloseClipboard() };
    }
}

pub fn set_text(text: &str) -> io::Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let bytes = wide.len() * 2;
    let _open = Open::new()?;
    // SAFETY: the memory block is allocated for `bytes`, filled while
    // locked, and handed to the clipboard, which then owns it; it is freed
    // here only if the clipboard did not take it.
    unsafe {
        EmptyClipboard().map_err(io::Error::other)?;
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(io::Error::other)?;
        let target = GlobalLock(memory).cast::<u16>();
        if target.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(io::Error::other("cannot lock clipboard memory"));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
        let _ = GlobalUnlock(memory);
        if let Err(e) = SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(memory.0))) {
            let _ = GlobalFree(Some(memory));
            return Err(io::Error::other(e));
        }
    }
    Ok(())
}
