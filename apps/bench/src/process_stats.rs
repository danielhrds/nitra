//! Handle count, private memory and CPU time of the current process, for
//! leak checks and cost comparisons.

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessHandleCount, GetProcessTimes,
};
use windows::core::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStats {
    pub handles: u32,
    pub private_bytes: usize,
    /// User plus kernel CPU time of all threads so far, in 100 ns units.
    pub cpu_time_100ns: u64,
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

        let (mut created, mut exited, mut kernel, mut user) = Default::default();
        // SAFETY: valid pseudo-handle; out-pointers reference live locals.
        unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }?;

        Ok(Self {
            handles,
            private_bytes: counters.PrivateUsage,
            cpu_time_100ns: filetime_ticks(kernel) + filetime_ticks(user),
        })
    }
}

fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// CPU used between two samples, as a percentage of one logical core.
pub fn cpu_percent_of_one_core(start: &ProcessStats, end: &ProcessStats, elapsed_s: f64) -> f64 {
    if elapsed_s <= 0.0 {
        return 0.0;
    }
    let cpu_s = end.cpu_time_100ns.saturating_sub(start.cpu_time_100ns) as f64 / 10_000_000.0;
    100.0 * cpu_s / elapsed_s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_combines_both_halves() {
        let time = FILETIME {
            dwLowDateTime: 5,
            dwHighDateTime: 1,
        };
        assert_eq!(filetime_ticks(time), (1 << 32) + 5);
    }

    #[test]
    fn cpu_percent_is_relative_to_one_core() {
        let stats = |cpu_time_100ns| ProcessStats {
            handles: 0,
            private_bytes: 0,
            cpu_time_100ns,
        };
        // 0.5 s of CPU over 10 s = 5% of one core.
        assert!((cpu_percent_of_one_core(&stats(0), &stats(5_000_000), 10.0) - 5.0).abs() < 1e-9);
        assert_eq!(cpu_percent_of_one_core(&stats(0), &stats(1), 0.0), 0.0);
    }
}
