//! Process memory for the header badge: the backend's own RSS and the RSS of
//! the webview processes the shell spawned. Proves the webview memory bound
//! from inside the app; Linux only, elsewhere the numbers are zero.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStats {
    pub backend_rss: u64,
    pub webview_rss: u64,
    /// `name (rss)` of every descendant process counted as webview.
    pub processes: Vec<String>,
}

pub fn stats() -> MemoryStats {
    let mut s = MemoryStats {
        backend_rss: keyfold_core::sysinfo::rss_bytes().unwrap_or(0),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    {
        let me = std::process::id();
        let mut procs: Vec<(u32, u32, String, u64)> = Vec::new();
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
                    continue;
                };
                let mut name = String::new();
                let mut ppid = 0u32;
                let mut rss = 0u64;
                for line in status.lines() {
                    if let Some(v) = line.strip_prefix("Name:") {
                        name = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("PPid:") {
                        ppid = v.trim().parse().unwrap_or(0);
                    } else if let Some(v) = line.strip_prefix("VmRSS:") {
                        rss = v.split_whitespace().next().and_then(|k| k.parse::<u64>().ok()).unwrap_or(0) * 1024;
                    }
                }
                procs.push((pid, ppid, name, rss));
            }
        }
        // descendants of this process, any depth (WebKit may sandbox through bwrap)
        let mut family = vec![me];
        let mut changed = true;
        while changed {
            changed = false;
            for (pid, ppid, _, _) in &procs {
                if family.contains(ppid) && !family.contains(pid) {
                    family.push(*pid);
                    changed = true;
                }
            }
        }
        for (pid, _, name, rss) in &procs {
            if *pid != me && family.contains(pid) && *rss > 0 {
                if name.contains("WebKit") || name.contains("webkit") {
                    s.webview_rss += rss;
                }
                s.processes.push(format!("{name} ({} MB)", rss >> 20));
            }
        }
    }
    s
}
