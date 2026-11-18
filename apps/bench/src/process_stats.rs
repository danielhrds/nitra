//! Handle count and private memory of the current process, for leak checks.

use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
use windows::core::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStats {
    pub handles: u32,
    pub private_bytes: usize,
}

impl ProcessStats {
    pub fn current() -> Result<Self> {
        // SAFETY: returns a pseudo-handle that needs no closing.
        let process = unsafe { GetCurrentProcess() };

        let mut handles = 0_u32;
        // SAFETY: valid pseudo-handle and a live out-pointer.
        unsafe { GetProcessHandleCount(process, &mut handles) }?;

        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        // SAFETY: the EX structure starts with PROCESS_MEMORY_COUNTERS and `cb`
        // tells the API how large the buffer really is.
        unsafe {
            GetProcessMemoryInfo(
                process,
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX)
                    .cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        }?;

        Ok(Self {
            handles,
            private_bytes: counters.PrivateUsage,
        })
    }
}
