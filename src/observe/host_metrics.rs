//! Host Metrics Collector
//!
//! Collects daemon-process metrics from the host OS. On Linux, reads
//! `/proc/self/statm`, `/proc/self/stat`, and `/proc/self/io`. On macOS, uses
//! Mach task APIs for RSS and `getrusage()` for CPU time.

#[cfg(target_os = "macos")]
use std::time::Instant;

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
    #[cfg(target_os = "macos")]
    prev_cpu_sample: std::sync::Mutex<Option<(u64, Instant)>>,
}

impl HostMetricsCollector {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            prev_cpu_ticks: std::sync::Mutex::new((0, 0)),
            #[cfg(target_os = "macos")]
            prev_cpu_sample: std::sync::Mutex::new(None),
        }
    }

    /// Collect a snapshot of current host metrics.
    pub fn collect(&self) -> HostSnapshot {
        #[cfg(target_os = "linux")]
        {
            self.collect_linux()
        }
        #[cfg(target_os = "macos")]
        {
            self.collect_macos()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

    #[cfg(target_os = "macos")]
    fn collect_macos(&self) -> HostSnapshot {
        HostSnapshot {
            rss_bytes: read_rss_bytes_macos().unwrap_or(0),
            cpu_percent: self.read_cpu_percent_macos().unwrap_or(0.0),
            io_read_bytes: 0,
            io_write_bytes: 0,
        }
    }

    #[cfg(target_os = "macos")]
    fn read_cpu_percent_macos(&self) -> Option<f64> {
        let cpu_nanos = read_cpu_time_nanos_macos()?;
        let now = Instant::now();
        let mut previous = self.prev_cpu_sample.lock().ok()?;
        let Some((previous_cpu_nanos, previous_instant)) = *previous else {
            *previous = Some((cpu_nanos, now));
            return Some(0.0);
        };

        let elapsed_nanos = now.duration_since(previous_instant).as_nanos() as u64;
        let delta_cpu_nanos = cpu_nanos.saturating_sub(previous_cpu_nanos);
        *previous = Some((cpu_nanos, now));

        if elapsed_nanos == 0 {
            return Some(0.0);
        }

        Some(((delta_cpu_nanos as f64 / elapsed_nanos as f64) * 100.0).min(100.0))
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

#[cfg(target_os = "macos")]
type IntegerT = libc::c_int;
#[cfg(target_os = "macos")]
type MachMsgTypeNumberT = libc::c_uint;
#[cfg(target_os = "macos")]
type MachPortNameT = libc::c_uint;
#[cfg(target_os = "macos")]
type TaskFlavorT = libc::c_uint;

#[cfg(target_os = "macos")]
const KERN_SUCCESS: libc::c_int = 0;
#[cfg(target_os = "macos")]
const MACH_TASK_BASIC_INFO: TaskFlavorT = 20;

#[cfg(target_os = "macos")]
#[repr(C)]
struct TimeValue {
    seconds: IntegerT,
    microseconds: IntegerT,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTaskBasicInfoData {
    virtual_size: u64,
    resident_size: u64,
    resident_size_max: u64,
    user_time: TimeValue,
    system_time: TimeValue,
    policy: IntegerT,
    suspend_count: IntegerT,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_task_self() -> MachPortNameT;
    fn task_info(
        target_task: MachPortNameT,
        flavor: TaskFlavorT,
        task_info_out: *mut IntegerT,
        task_info_out_count: *mut MachMsgTypeNumberT,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
fn read_rss_bytes_macos() -> Option<u64> {
    let mut info = MachTaskBasicInfoData {
        virtual_size: 0,
        resident_size: 0,
        resident_size_max: 0,
        user_time: TimeValue {
            seconds: 0,
            microseconds: 0,
        },
        system_time: TimeValue {
            seconds: 0,
            microseconds: 0,
        },
        policy: 0,
        suspend_count: 0,
    };
    let mut count = (std::mem::size_of::<MachTaskBasicInfoData>() / std::mem::size_of::<IntegerT>())
        as MachMsgTypeNumberT;
    let result = unsafe {
        task_info(
            mach_task_self(),
            MACH_TASK_BASIC_INFO,
            (&mut info as *mut MachTaskBasicInfoData).cast::<IntegerT>(),
            &mut count,
        )
    };
    if result != KERN_SUCCESS {
        return None;
    }
    Some(info.resident_size)
}

#[cfg(target_os = "macos")]
fn read_cpu_time_nanos_macos() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    let user_nanos = timeval_to_nanos(usage.ru_utime);
    let system_nanos = timeval_to_nanos(usage.ru_stime);
    Some(user_nanos.saturating_add(system_nanos))
}

#[cfg(target_os = "macos")]
fn timeval_to_nanos(timeval: libc::timeval) -> u64 {
    let seconds = u64::try_from(timeval.tv_sec).unwrap_or(0);
    let micros = u64::try_from(timeval.tv_usec).unwrap_or(0);
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(micros.saturating_mul(1_000))
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
        #[cfg(target_os = "macos")]
        assert!(snap.rss_bytes > 0, "Expected non-zero RSS on macOS");
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
