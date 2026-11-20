//! Process statistics for the benchmark harness.

/// Resident set size of this process in bytes, when the platform exposes it.
pub fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Ask the allocator to hand large freed buffers straight back to the OS.
///
/// A merge allocates and frees tens of megabytes of key and entry buffers;
/// glibc's dynamic mmap threshold otherwise keeps those on the heap, and the
/// process holds its high-water mark long after the scan. Only glibc has
/// `mallopt`; elsewhere this is a no-op. Safe to call more than once.
pub fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: mallopt only sets allocator parameters.
            unsafe {
                libc::mallopt(libc::M_MMAP_THRESHOLD, 1 << 20);
                libc::mallopt(libc::M_TRIM_THRESHOLD, 4 << 20);
            }
        });
    }
}
