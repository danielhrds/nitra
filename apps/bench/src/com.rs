//! COM apartment lifetime for the calling thread.

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::Result;

/// Joins the multithreaded apartment; leaves it on drop.
/// Must be dropped on the thread that created it, after every COM object.
pub struct ComApartment(());

impl ComApartment {
    pub fn multithreaded() -> Result<Self> {
        // SAFETY: no reserved pointer is passed; balanced by CoUninitialize in Drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self(()))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}
