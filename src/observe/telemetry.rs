//! Guest Telemetry Aggregator
//!
//! Ingests telemetry batches from the guest VM and feeds them into the
//! existing Observer's MetricsCollector. This bridges the guest-to-host
//! telemetry pipeline without introducing new metric backends.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use super::Observer;
use crate::guest::protocol::{SystemMetrics, TelemetryBatch};

// ---------------------------------------------------------------------------
// Telemetry ring buffer types
// ---------------------------------------------------------------------------

/// A single telemetry sample stored in the ring buffer.
#[derive(Debug, Clone, Serialize)]
pub struct TelemetrySample {
    pub seq: u64,
    pub timestamp_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub stage_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest: Option<GuestMetricsSample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<HostMetricsSample>,
}

/// Guest VM metrics snapshot (mirrors fields from `SystemMetrics`).
#[derive(Debug, Clone, Serialize)]
pub struct GuestMetricsSample {
    pub cpu_percent: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    pub procs_running: u32,
    pub open_fds: u32,
}

/// Host daemon metrics snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct HostMetricsSample {
    pub rss_bytes: u64,
    pub cpu_percent: f64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
}

/// Fixed-capacity ring buffer for telemetry samples. Oldest samples are evicted
/// when capacity is reached.
pub struct TelemetryRingBuffer {
    samples: VecDeque<TelemetrySample>,
    capacity: usize,
    next_seq: u64,
}

impl TelemetryRingBuffer {
    /// Create a new ring buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity.min(8192)),
            capacity,
            next_seq: 1,
        }
    }

    /// Push a sample into the buffer. The `seq` field is assigned automatically.
    pub fn push(&mut self, mut sample: TelemetrySample) {
        sample.seq = self.next_seq;
        self.next_seq += 1;
        if self.samples.len() >= self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Query samples with `seq > from_seq`, optionally filtered by stage_name.
    /// Returns `(matching_samples, next_seq)`.
    pub fn query(
        &self,
        from_seq: u64,
        stage_name_filter: Option<&str>,
    ) -> (Vec<TelemetrySample>, u64) {
        let filtered: Vec<TelemetrySample> = self
            .samples
            .iter()
            .filter(|s| s.seq > from_seq)
            .filter(|s| match stage_name_filter {
                Some(name) => s.stage_name == name,
                None => true,
            })
            .cloned()
            .collect();
        (filtered, self.next_seq.saturating_sub(1).max(from_seq))
    }

    /// Current next_seq value (the seq that would be assigned to the next push).
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

impl GuestMetricsSample {
    /// Build a `GuestMetricsSample` from the protocol's `SystemMetrics`.
    pub fn from_system_metrics(sys: &SystemMetrics) -> Self {
        Self {
            cpu_percent: sys.cpu_percent,
            memory_used_bytes: sys.memory_used_bytes,
            memory_total_bytes: sys.memory_total_bytes,
            net_rx_bytes: sys.net_rx_bytes,
            net_tx_bytes: sys.net_tx_bytes,
            procs_running: sys.procs_running,
            open_fds: sys.open_fds,
        }
    }
}

/// Aggregates telemetry data from a guest VM into the Observer's metrics.
pub struct TelemetryAggregator {
    observer: Observer,
    cid: u32,
    latest: Mutex<Option<TelemetryBatch>>,
    /// Optional ring buffer for the `/v1/runs/{id}/telemetry` endpoint.
    ring_buffer: Option<Arc<Mutex<TelemetryRingBuffer>>>,
    /// Current stage name — updated externally when StageStarted events arrive.
    current_stage: Arc<Mutex<String>>,
}

impl TelemetryAggregator {
    /// Create a new aggregator for a guest VM with the given CID.
    pub fn new(observer: Observer, cid: u32) -> Self {
        Self {
            observer,
            cid,
            latest: Mutex::new(None),
            ring_buffer: None,
            current_stage: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Create an aggregator with a shared ring buffer for telemetry queries.
    pub fn with_ring_buffer(
        observer: Observer,
        cid: u32,
        ring_buffer: Arc<Mutex<TelemetryRingBuffer>>,
    ) -> Self {
        Self {
            observer,
            cid,
            latest: Mutex::new(None),
            ring_buffer: Some(ring_buffer),
            current_stage: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Set the current stage name (called when StageStarted event is observed).
    pub fn set_current_stage(&self, stage_name: &str) {
        if let Ok(mut s) = self.current_stage.lock() {
            *s = stage_name.to_string();
        }
    }

    /// Get the current stage name.
    pub fn current_stage_name(&self) -> String {
        self.current_stage
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
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

        // Push into ring buffer if configured
        if let Some(ref rb) = self.ring_buffer {
            let stage_name = self.current_stage_name();
            let guest = batch
                .system
                .as_ref()
                .map(GuestMetricsSample::from_system_metrics);
            if let Ok(mut buf) = rb.lock() {
                buf.push(TelemetrySample {
                    seq: 0, // assigned by push()
                    timestamp_ms: batch.timestamp_ms,
                    timestamp: None,
                    stage_name,
                    guest,
                    host: None,
                });
            }
        }
    }

    fn ingest_system(&self, sys: &SystemMetrics, labels: &[(&str, &str)]) {
        let metrics = self.observer.metrics();

        // CPU as histogram (distribution analysis) — also records the gauge
        metrics.record_cpu_histogram(sys.cpu_percent, labels);
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

    // -- TelemetryRingBuffer tests --

    #[test]
    fn test_ring_buffer_push_and_query() {
        let mut rb = TelemetryRingBuffer::new(10);
        assert_eq!(rb.next_seq(), 1);

        rb.push(TelemetrySample {
            seq: 0,
            timestamp_ms: 1000,
            timestamp: None,
            stage_name: "build".into(),
            guest: None,
            host: None,
        });
        rb.push(TelemetrySample {
            seq: 0,
            timestamp_ms: 2000,
            timestamp: None,
            stage_name: "test".into(),
            guest: None,
            host: None,
        });

        assert_eq!(rb.next_seq(), 3);

        let (samples, next) = rb.query(0, None);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].seq, 1);
        assert_eq!(samples[1].seq, 2);
        assert_eq!(next, 2);
    }

    #[test]
    fn test_ring_buffer_eviction() {
        let mut rb = TelemetryRingBuffer::new(3);
        for i in 0..5 {
            rb.push(TelemetrySample {
                seq: 0,
                timestamp_ms: i * 1000,
                timestamp: None,
                stage_name: format!("s{}", i),
                guest: None,
                host: None,
            });
        }

        // Capacity is 3, so only the last 3 samples remain
        let (samples, _) = rb.query(0, None);
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].seq, 3); // first two were evicted
        assert_eq!(samples[2].seq, 5);
    }

    #[test]
    fn test_ring_buffer_query_with_from_seq() {
        let mut rb = TelemetryRingBuffer::new(10);
        for _ in 0..5 {
            rb.push(TelemetrySample {
                seq: 0,
                timestamp_ms: 1000,
                timestamp: None,
                stage_name: "a".into(),
                guest: None,
                host: None,
            });
        }

        let (samples, next) = rb.query(3, None);
        assert_eq!(samples.len(), 2); // seq 4 and 5
        assert_eq!(samples[0].seq, 4);
        assert_eq!(next, 5);
    }

    #[test]
    fn test_ring_buffer_query_with_stage_filter() {
        let mut rb = TelemetryRingBuffer::new(10);
        rb.push(TelemetrySample {
            seq: 0,
            timestamp_ms: 1000,
            timestamp: None,
            stage_name: "build".into(),
            guest: None,
            host: None,
        });
        rb.push(TelemetrySample {
            seq: 0,
            timestamp_ms: 2000,
            timestamp: None,
            stage_name: "test".into(),
            guest: None,
            host: None,
        });
        rb.push(TelemetrySample {
            seq: 0,
            timestamp_ms: 3000,
            timestamp: None,
            stage_name: "build".into(),
            guest: None,
            host: None,
        });

        let (samples, _) = rb.query(0, Some("build"));
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|s| s.stage_name == "build"));
    }

    #[test]
    fn test_ring_buffer_empty_query() {
        let rb = TelemetryRingBuffer::new(10);
        let (samples, next) = rb.query(0, None);
        assert!(samples.is_empty());
        assert_eq!(next, 0);
    }

    #[test]
    fn test_ring_buffer_query_beyond_latest() {
        let mut rb = TelemetryRingBuffer::new(10);
        rb.push(TelemetrySample {
            seq: 0,
            timestamp_ms: 1000,
            timestamp: None,
            stage_name: "a".into(),
            guest: None,
            host: None,
        });

        let (samples, next) = rb.query(100, None);
        assert!(samples.is_empty());
        assert_eq!(next, 100); // returns from_seq when no new samples
    }
}
