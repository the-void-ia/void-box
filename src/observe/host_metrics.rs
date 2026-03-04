//! Host Metrics Collector
//!
//! Collects daemon-process metrics from the host OS. On Linux, reads
//! `/proc/self/statm`, `/proc/self/stat`, and `/proc/self/io`. On other
//! platforms, returns zeros.

/// Snapshot of host daemon metrics.
#[derive(Debug, Clone, Default)]
pub struct HostSnapshot {
    pub rss_bytes: u64,
    pub cpu_percent: f64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
}

/// Collects host-side daemon metrics. Maintains previous CPU tick values for
/// delta-based CPU percentage calculation.
#[derive(Default)]
pub struct HostMetricsCollector {
    #[cfg(target_os = "linux")]
    prev_cpu_ticks: std::sync::Mutex<(u64, u64)>,
}

impl HostMetricsCollector {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            prev_cpu_ticks: std::sync::Mutex::new((0, 0)),
        }
    }

    /// Collect a snapshot of current host metrics.
    pub fn collect(&self) -> HostSnapshot {
        #[cfg(target_os = "linux")]
        {
            self.collect_linux()
        }
        #[cfg(not(target_os = "linux"))]
        {
            HostSnapshot::default()
        }
    }

    #[cfg(target_os = "linux")]
    fn collect_linux(&self) -> HostSnapshot {
        let rss_bytes = read_rss_bytes().unwrap_or(0);
        let (io_read_bytes, io_write_bytes) = read_io_bytes().unwrap_or((0, 0));
        let cpu_percent = self.read_cpu_percent().unwrap_or(0.0);

        HostSnapshot {
            rss_bytes,
            cpu_percent,
            io_read_bytes,
            io_write_bytes,
        }
    }

    #[cfg(target_os = "linux")]
    fn read_cpu_percent(&self) -> Option<f64> {
        // Read /proc/self/stat for utime + stime (fields 14, 15, 1-indexed)
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // Fields after the comm (which may contain spaces and parens):
        // skip past the closing paren
        let after_comm = stat.rsplit_once(')')?.1;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // field[11] = utime, field[12] = stime (0-indexed from after comm+state)
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        let process_ticks = utime + stime;

        // Read /proc/stat for total CPU ticks
        let proc_stat = std::fs::read_to_string("/proc/stat").ok()?;
        let cpu_line = proc_stat.lines().next()?;
        let cpu_fields: Vec<u64> = cpu_line
            .split_whitespace()
            .skip(1) // skip "cpu"
            .filter_map(|f| f.parse().ok())
            .collect();
        let total_ticks: u64 = cpu_fields.iter().sum();

        let mut prev = self.prev_cpu_ticks.lock().ok()?;
        let (prev_proc, prev_total) = *prev;

        let delta_proc = process_ticks.saturating_sub(prev_proc);
        let delta_total = total_ticks.saturating_sub(prev_total);

        *prev = (process_ticks, total_ticks);

        if delta_total == 0 {
            return Some(0.0);
        }

        Some((delta_proc as f64 / delta_total as f64) * 100.0)
    }
}

#[cfg(target_os = "linux")]
fn read_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Some(rss_pages * page_size)
}

#[cfg(target_os = "linux")]
fn read_io_bytes() -> Option<(u64, u64)> {
    let io = std::fs::read_to_string("/proc/self/io").ok()?;
    let mut read_bytes = 0u64;
    let mut write_bytes = 0u64;
    for line in io.lines() {
        if let Some(val) = line.strip_prefix("read_bytes: ") {
            read_bytes = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("write_bytes: ") {
            write_bytes = val.trim().parse().unwrap_or(0);
        }
    }
    Some((read_bytes, write_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_metrics_collector_returns_snapshot() {
        let collector = HostMetricsCollector::new();
        let snap = collector.collect();
        // On Linux CI, we should get non-zero RSS
        #[cfg(target_os = "linux")]
        assert!(snap.rss_bytes > 0, "Expected non-zero RSS on Linux");
        // On non-Linux, we get zeros (that's fine)
        let _ = snap;
    }

    #[test]
    fn test_host_metrics_collector_cpu_delta() {
        let collector = HostMetricsCollector::new();
        // First call establishes baseline
        let _snap1 = collector.collect();
        // Second call computes delta
        let snap2 = collector.collect();
        // CPU percent should be in [0, 100]
        assert!(snap2.cpu_percent >= 0.0);
        assert!(snap2.cpu_percent <= 100.0);
    }
}
