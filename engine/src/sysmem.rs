//! Memory figures: the process's own (for measurements) and the memory
//! the system has available (to warn before a comparison that may not fit).
//!
//! Linux: /proc. Windows: GetProcessMemoryInfo and GlobalMemoryStatusEx.
//! Values are bytes.

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct MemoryStat {
    /// Current resident set / working set.
    pub current: u64,
    /// Peak resident set / working set since process start.
    pub peak: u64,
    /// Windows only: peak private commit (PeakPagefileUsage).
    pub peak_private: Option<u64>,
}

#[cfg(target_os = "linux")]
pub fn read() -> MemoryStat {
    let mut m = MemoryStat::default();
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            let kb = |l: &str| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
            if line.starts_with("VmRSS:") {
                m.current = kb(line);
            } else if line.starts_with("VmHWM:") {
                m.peak = kb(line);
            }
        }
    }
    m
}

/// Cheap current RSS for frequent sampling.
#[cfg(target_os = "linux")]
pub fn current() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        .map_or(0, |pages| pages * 4096)
}

#[cfg(windows)]
pub fn read() -> MemoryStat {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain FFI call with a correctly sized, zero-initialised struct.
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) == 0 {
            return MemoryStat::default();
        }
        MemoryStat {
            current: c.WorkingSetSize as u64,
            peak: c.PeakWorkingSetSize as u64,
            peak_private: Some(c.PeakPagefileUsage as u64),
        }
    }
}

#[cfg(windows)]
pub fn current() -> u64 {
    read().current
}

/// Physical memory available to start new work without paging.
#[cfg(target_os = "linux")]
pub fn available() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = s.lines().find(|l| l.starts_with("MemAvailable:"))?;
    line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kb| kb * 1024)
}

#[cfg(windows)]
pub fn available() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain FFI call with a correctly sized, zero-initialised struct.
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        (GlobalMemoryStatusEx(&mut m) != 0).then_some(m.ullAvailPhys)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn available() -> Option<u64> {
    None
}

/// How much more memory the process can get before allocations start to
/// fail. Windows: the commit charge left (physical memory plus the page
/// file). Linux: the available memory (beyond it the kernel kills processes).
#[cfg(windows)]
pub fn headroom() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain FFI call with a correctly sized, zero-initialised struct.
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        (GlobalMemoryStatusEx(&mut m) != 0).then_some(m.ullAvailPageFile)
    }
}

#[cfg(not(windows))]
pub fn headroom() -> Option<u64> {
    available()
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn read() -> MemoryStat {
    MemoryStat::default()
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn current() -> u64 {
    0
}
