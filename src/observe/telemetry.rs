//! Guest Telemetry Aggregator
//!
//! Ingests telemetry batches from the guest VM and feeds them into the
//! existing Observer's MetricsCollector. This bridges the guest-to-host
//! telemetry pipeline without introducing new metric backends.

use std::sync::Mutex;

use super::Observer;
use crate::guest::protocol::{SystemMetrics, TelemetryBatch};

/// Aggregates telemetry data from a guest VM into the Observer's metrics.
pub struct TelemetryAggregator {
    observer: Observer,
    cid: u32,
    latest: Mutex<Option<TelemetryBatch>>,
}

impl TelemetryAggregator {
    /// Create a new aggregator for a guest VM with the given CID.
    pub fn new(observer: Observer, cid: u32) -> Self {
        Self {
            observer,
            cid,
            latest: Mutex::new(None),
        }
    }

    /// Ingest a telemetry batch from the guest and record into the Observer's MetricsCollector.
    pub fn ingest(&self, batch: &TelemetryBatch) {
        let cid_str = self.cid.to_string();
        let labels: &[(&str, &str)] = &[("vm_cid", &cid_str)];

        if let Some(ref sys) = batch.system {
            self.ingest_system(sys, labels);
        }

        for proc in &batch.processes {
            let pid_str = proc.pid.to_string();
            let proc_labels: &[(&str, &str)] = &[
                ("vm_cid", &cid_str),
                ("pid", &pid_str),
                ("comm", &proc.comm),
            ];
            self.observer.metrics().set_gauge(
                "guest.process.rss_bytes",
                proc.rss_bytes as f64,
                proc_labels,
            );
            self.observer.metrics().set_gauge(
                "guest.process.cpu_jiffies",
                proc.cpu_jiffies as f64,
                proc_labels,
            );
        }

        // Store latest batch
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(batch.clone());
        }
    }

    fn ingest_system(&self, sys: &SystemMetrics, labels: &[(&str, &str)]) {
        let metrics = self.observer.metrics();

        metrics.record_cpu_usage(sys.cpu_percent, labels);
        metrics.record_memory_usage(sys.memory_used_bytes, labels);
        metrics.record_network_io(sys.net_rx_bytes, sys.net_tx_bytes, labels);
        metrics.set_gauge(
            "guest.memory_total_bytes",
            sys.memory_total_bytes as f64,
            labels,
        );
        metrics.set_gauge("guest.procs_running", sys.procs_running as f64, labels);
        metrics.set_gauge("guest.open_fds", sys.open_fds as f64, labels);
    }

    /// Get the latest telemetry batch received from the guest.
    pub fn latest_batch(&self) -> Option<TelemetryBatch> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }

    /// Get the CID this aggregator is tracking.
    pub fn cid(&self) -> u32 {
        self.cid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::protocol::{ProcessMetrics, SystemMetrics, TelemetryBatch};

    #[test]
    fn test_ingest_system_metrics() {
        let observer = Observer::test();
        let aggregator = TelemetryAggregator::new(observer.clone(), 42);

        let batch = TelemetryBatch {
            seq: 0,
            timestamp_ms: 1700000000000,
            system: Some(SystemMetrics {
                cpu_percent: 55.0,
                memory_used_bytes: 512 * 1024 * 1024,
                memory_total_bytes: 1024 * 1024 * 1024,
                net_rx_bytes: 1000,
                net_tx_bytes: 2000,
                procs_running: 3,
                open_fds: 64,
            }),
            processes: vec![],
            trace_context: None,
        };

        aggregator.ingest(&batch);

        let snapshot = observer.get_metrics();
        // CPU and memory gauges should be recorded
        assert!(snapshot
            .metrics
            .values()
            .any(|m| m.name == "cpu_usage_percent"));
        assert!(snapshot
            .metrics
            .values()
            .any(|m| m.name == "memory_usage_bytes"));
    }

    #[test]
    fn test_ingest_process_metrics() {
        let observer = Observer::test();
        let aggregator = TelemetryAggregator::new(observer.clone(), 42);

        let batch = TelemetryBatch {
            seq: 1,
            timestamp_ms: 1700000000000,
            system: None,
            processes: vec![ProcessMetrics {
                pid: 1,
                comm: "init".to_string(),
                rss_bytes: 8192,
                cpu_jiffies: 100,
                state: 'S',
            }],
            trace_context: None,
        };

        aggregator.ingest(&batch);

        let snapshot = observer.get_metrics();
        assert!(snapshot
            .metrics
            .values()
            .any(|m| m.name == "guest.process.rss_bytes"));
    }

    #[test]
    fn test_latest_batch() {
        let observer = Observer::test();
        let aggregator = TelemetryAggregator::new(observer, 42);

        assert!(aggregator.latest_batch().is_none());

        let batch = TelemetryBatch {
            seq: 5,
            timestamp_ms: 1700000000000,
            system: None,
            processes: vec![],
            trace_context: None,
        };
        aggregator.ingest(&batch);

        let latest = aggregator.latest_batch().unwrap();
        assert_eq!(latest.seq, 5);
    }
}
