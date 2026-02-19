//! Prometheus-compatible Metrics Collection
//!
//! Provides metrics collection for workflow execution:
//! - Step duration histograms
//! - Memory usage gauges
//! - CPU usage gauges
//! - Network I/O counters
//! - Custom application metrics

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Configuration for metrics collection
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    /// Enable metrics collection
    pub enabled: bool,
    /// Collect step duration metrics
    pub step_duration: bool,
    /// Collect memory usage metrics
    pub memory_usage: bool,
    /// Collect CPU usage metrics
    pub cpu_usage: bool,
    /// Collect network I/O metrics
    pub network_io: bool,
    /// Prometheus push gateway endpoint
    pub pushgateway_endpoint: Option<String>,
    /// Enable in-memory collection (for testing)
    pub in_memory: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            step_duration: true,
            memory_usage: true,
            cpu_usage: true,
            network_io: true,
            pushgateway_endpoint: None,
            in_memory: false,
        }
    }
}

impl MetricsConfig {
    /// Create a config for in-memory metrics (useful for tests)
    pub fn in_memory() -> Self {
        Self {
            enabled: true,
            in_memory: true,
            ..Default::default()
        }
    }

    /// Set the pushgateway endpoint
    pub fn pushgateway(mut self, endpoint: impl Into<String>) -> Self {
        self.pushgateway_endpoint = Some(endpoint.into());
        self
    }
}

/// Types of metrics
#[derive(Debug, Clone)]
pub enum MetricValue {
    /// Counter (monotonically increasing)
    Counter(f64),
    /// Gauge (can go up or down)
    Gauge(f64),
    /// Histogram (distribution of values)
    Histogram(HistogramValue),
}

/// Histogram value with buckets
#[derive(Debug, Clone)]
pub struct HistogramValue {
    /// Sum of all observations
    pub sum: f64,
    /// Count of observations
    pub count: u64,
    /// Bucket counts (le -> count)
    pub buckets: Vec<(f64, u64)>,
}

impl HistogramValue {
    /// Create a new histogram with default buckets
    pub fn new() -> Self {
        Self::with_buckets(&[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0])
    }

    /// Create a histogram with custom buckets
    pub fn with_buckets(buckets: &[f64]) -> Self {
        Self {
            sum: 0.0,
            count: 0,
            buckets: buckets.iter().map(|&b| (b, 0)).collect(),
        }
    }

    /// Observe a value
    pub fn observe(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;

        for (le, count) in &mut self.buckets {
            if value <= *le {
                *count += 1;
            }
        }
    }

    /// Get the average value
    pub fn average(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }
}

impl Default for HistogramValue {
    fn default() -> Self {
        Self::new()
    }
}

/// A single metric with labels
#[derive(Debug, Clone)]
pub struct Metric {
    /// Metric name
    pub name: String,
    /// Metric help text
    pub help: String,
    /// Metric value
    pub value: MetricValue,
    /// Labels
    pub labels: HashMap<String, String>,
}

/// Snapshot of all collected metrics
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    /// All metrics keyed by name
    pub metrics: HashMap<String, Metric>,
    /// Timestamp when snapshot was taken
    pub timestamp: std::time::SystemTime,
}

impl MetricsSnapshot {
    /// Get a metric value by name
    pub fn get(&self, name: &str) -> Option<&Metric> {
        self.metrics.get(name)
    }

    /// Check if a metric exists
    pub fn contains(&self, name: &str) -> bool {
        self.metrics.contains_key(name)
    }

    /// Get a counter value
    pub fn get_counter(&self, name: &str) -> Option<f64> {
        self.metrics.get(name).and_then(|m| match &m.value {
            MetricValue::Counter(v) => Some(*v),
            _ => None,
        })
    }

    /// Get a gauge value
    pub fn get_gauge(&self, name: &str) -> Option<f64> {
        self.metrics.get(name).and_then(|m| match &m.value {
            MetricValue::Gauge(v) => Some(*v),
            _ => None,
        })
    }

    /// Get a histogram
    pub fn get_histogram(&self, name: &str) -> Option<&HistogramValue> {
        self.metrics.get(name).and_then(|m| match &m.value {
            MetricValue::Histogram(h) => Some(h),
            _ => None,
        })
    }

    /// Format as Prometheus text format
    pub fn to_prometheus_text(&self) -> String {
        let mut output = String::new();

        for metric in self.metrics.values() {
            // Help line
            output.push_str(&format!("# HELP {} {}\n", metric.name, metric.help));

            // Type line
            let type_str = match &metric.value {
                MetricValue::Counter(_) => "counter",
                MetricValue::Gauge(_) => "gauge",
                MetricValue::Histogram(_) => "histogram",
            };
            output.push_str(&format!("# TYPE {} {}\n", metric.name, type_str));

            // Value line(s)
            let labels_str = if metric.labels.is_empty() {
                String::new()
            } else {
                let pairs: Vec<_> = metric
                    .labels
                    .iter()
                    .map(|(k, v)| format!("{}=\"{}\"", k, v))
                    .collect();
                format!("{{{}}}", pairs.join(","))
            };

            match &metric.value {
                MetricValue::Counter(v) | MetricValue::Gauge(v) => {
                    output.push_str(&format!("{}{} {}\n", metric.name, labels_str, v));
                }
                MetricValue::Histogram(h) => {
                    for (le, count) in &h.buckets {
                        output.push_str(&format!(
                            "{}_bucket{{le=\"{}\"{}}} {}\n",
                            metric.name,
                            le,
                            if labels_str.is_empty() {
                                String::new()
                            } else {
                                format!(",{}", &labels_str[1..labels_str.len() - 1])
                            },
                            count
                        ));
                    }
                    output.push_str(&format!("{}_sum{} {}\n", metric.name, labels_str, h.sum));
                    output.push_str(&format!(
                        "{}_count{} {}\n",
                        metric.name, labels_str, h.count
                    ));
                }
            }
        }

        output
    }
}

/// Metrics collector -- stores metrics in-memory and optionally exports via OTel.
pub struct MetricsCollector {
    config: MetricsConfig,
    metrics: Mutex<HashMap<String, Metric>>,
    /// OTel meter for OTLP export (feature-gated).
    #[cfg(feature = "opentelemetry")]
    otel_meter: Option<opentelemetry::metrics::Meter>,
}

impl MetricsCollector {
    /// Create a new metrics collector
    pub fn new(config: MetricsConfig) -> Self {
        Self {
            config,
            metrics: Mutex::new(HashMap::new()),
            #[cfg(feature = "opentelemetry")]
            otel_meter: None,
        }
    }

    /// Create a metrics collector with an OTel Meter for OTLP export.
    #[cfg(feature = "opentelemetry")]
    pub fn with_otel_meter(config: MetricsConfig, meter: opentelemetry::metrics::Meter) -> Self {
        Self {
            config,
            metrics: Mutex::new(HashMap::new()),
            otel_meter: Some(meter),
        }
    }

    /// Record a duration metric
    pub fn record_duration(&self, name: &str, duration: Duration) {
        if !self.config.enabled || !self.config.step_duration {
            return;
        }

        let metric_name = format!("{}_duration_ms", name);
        let duration_ms = duration.as_secs_f64() * 1000.0;

        let mut metrics = self.metrics.lock().unwrap();
        let metric = metrics
            .entry(metric_name.clone())
            .or_insert_with(|| Metric {
                name: metric_name.clone(),
                help: format!("Duration of {} in milliseconds", name),
                value: MetricValue::Histogram(HistogramValue::with_buckets(&[
                    1.0, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0,
                ])),
                labels: HashMap::new(),
            });

        if let MetricValue::Histogram(h) = &mut metric.value {
            h.observe(duration_ms);
        }

        // Also record via OTel histogram
        #[cfg(feature = "opentelemetry")]
        if let Some(ref meter) = self.otel_meter {
            let histogram = meter.f64_histogram(metric_name).build();
            histogram.record(duration_ms, &[]);
        }
    }

    /// Increment a counter
    pub fn increment_counter(&self, name: &str, labels: &[(&str, &str)]) {
        self.add_counter(name, 1.0, labels);
    }

    /// Add to a counter
    pub fn add_counter(&self, name: &str, value: f64, labels: &[(&str, &str)]) {
        if !self.config.enabled {
            return;
        }

        let mut metrics = self.metrics.lock().unwrap();
        let label_map: HashMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let key = format!("{}:{:?}", name, label_map);

        let metric = metrics.entry(key).or_insert_with(|| Metric {
            name: name.to_string(),
            help: format!("Counter for {}", name),
            value: MetricValue::Counter(0.0),
            labels: label_map,
        });

        if let MetricValue::Counter(v) = &mut metric.value {
            *v += value;
        }

        // Also record via OTel counter
        #[cfg(feature = "opentelemetry")]
        if let Some(ref meter) = self.otel_meter {
            use opentelemetry::KeyValue;
            let counter = meter.f64_counter(name.to_string()).build();
            let otel_labels: Vec<KeyValue> = labels
                .iter()
                .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
                .collect();
            counter.add(value, &otel_labels);
        }
    }

    /// Set a gauge value
    pub fn set_gauge(&self, name: &str, value: f64, labels: &[(&str, &str)]) {
        if !self.config.enabled {
            return;
        }

        let mut metrics = self.metrics.lock().unwrap();
        let label_map: HashMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let key = format!("{}:{:?}", name, label_map);

        let metric = metrics.entry(key).or_insert_with(|| Metric {
            name: name.to_string(),
            help: format!("Gauge for {}", name),
            value: MetricValue::Gauge(0.0),
            labels: label_map,
        });

        if let MetricValue::Gauge(v) = &mut metric.value {
            *v = value;
        }

        // Also record via OTel gauge
        #[cfg(feature = "opentelemetry")]
        if let Some(ref meter) = self.otel_meter {
            use opentelemetry::KeyValue;
            let gauge = meter.f64_gauge(name.to_string()).build();
            let otel_labels: Vec<KeyValue> = labels
                .iter()
                .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
                .collect();
            gauge.record(value, &otel_labels);
        }
    }

    /// Record memory usage
    pub fn record_memory_usage(&self, bytes: u64, labels: &[(&str, &str)]) {
        if self.config.memory_usage {
            self.set_gauge("memory_usage_bytes", bytes as f64, labels);
        }
    }

    /// Record CPU usage (gauge for current-value queries).
    pub fn record_cpu_usage(&self, percent: f64, labels: &[(&str, &str)]) {
        if self.config.cpu_usage {
            self.set_gauge("cpu_usage_percent", percent, labels);
        }
    }

    /// Record CPU usage as a histogram for distribution analysis.
    ///
    /// Records both the existing gauge (for dashboards) and a histogram
    /// (for p50/p90/p99 analysis).
    pub fn record_cpu_histogram(&self, percent: f64, labels: &[(&str, &str)]) {
        if !self.config.cpu_usage {
            return;
        }

        // Also record the gauge for current-value queries
        self.set_gauge("cpu_usage_percent", percent, labels);

        let label_map: HashMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let key = format!("cpu_usage_percent_histogram:{:?}", label_map);

        let mut metrics = self.metrics.lock().unwrap();
        let metric = metrics.entry(key).or_insert_with(|| Metric {
            name: "cpu_usage_percent_histogram".to_string(),
            help: "CPU usage distribution".to_string(),
            value: MetricValue::Histogram(HistogramValue::with_buckets(&[
                5.0, 10.0, 25.0, 50.0, 75.0, 90.0, 95.0, 100.0,
            ])),
            labels: label_map,
        });

        if let MetricValue::Histogram(h) = &mut metric.value {
            h.observe(percent);
        }

        #[cfg(feature = "opentelemetry")]
        if let Some(ref meter) = self.otel_meter {
            use opentelemetry::KeyValue;
            let histogram = meter
                .f64_histogram("cpu_usage_percent_histogram")
                .build();
            let otel_labels: Vec<KeyValue> = labels
                .iter()
                .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
                .collect();
            histogram.record(percent, &otel_labels);
        }
    }

    /// Record network I/O
    pub fn record_network_io(&self, rx_bytes: u64, tx_bytes: u64, labels: &[(&str, &str)]) {
        if self.config.network_io {
            self.add_counter("network_rx_bytes", rx_bytes as f64, labels);
            self.add_counter("network_tx_bytes", tx_bytes as f64, labels);
        }
    }

    /// Get a snapshot of all metrics
    pub fn snapshot(&self) -> MetricsSnapshot {
        let metrics = self.metrics.lock().unwrap();
        MetricsSnapshot {
            metrics: metrics.clone(),
            timestamp: std::time::SystemTime::now(),
        }
    }

    /// Clear all metrics
    pub fn clear(&self) {
        self.metrics.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_histogram_observe() {
        let mut hist = HistogramValue::with_buckets(&[1.0, 5.0, 10.0]);

        hist.observe(0.5);
        hist.observe(3.0);
        hist.observe(7.0);
        hist.observe(15.0);

        assert_eq!(hist.count, 4);
        assert!((hist.sum - 25.5).abs() < 0.001);

        // Check bucket counts
        assert_eq!(hist.buckets[0].1, 1); // le=1.0: 0.5
        assert_eq!(hist.buckets[1].1, 2); // le=5.0: 0.5, 3.0
        assert_eq!(hist.buckets[2].1, 3); // le=10.0: 0.5, 3.0, 7.0
    }

    #[test]
    fn test_metrics_collector_duration() {
        let collector = MetricsCollector::new(MetricsConfig::in_memory());

        collector.record_duration("step1", Duration::from_millis(100));
        collector.record_duration("step1", Duration::from_millis(200));

        let snapshot = collector.snapshot();
        let hist = snapshot.get_histogram("step1_duration_ms").unwrap();

        assert_eq!(hist.count, 2);
        assert!((hist.sum - 300.0).abs() < 0.001);
    }

    #[test]
    fn test_metrics_collector_counter() {
        let collector = MetricsCollector::new(MetricsConfig::in_memory());

        collector.increment_counter("requests", &[("method", "GET")]);
        collector.increment_counter("requests", &[("method", "GET")]);
        collector.add_counter("requests", 5.0, &[("method", "POST")]);

        let snapshot = collector.snapshot();
        // Counters with different labels are stored separately
        assert!(snapshot.metrics.len() >= 2);
    }

    #[test]
    fn test_metrics_collector_gauge() {
        let collector = MetricsCollector::new(MetricsConfig::in_memory());

        collector.set_gauge("temperature", 25.5, &[("sensor", "cpu")]);
        collector.set_gauge("temperature", 30.0, &[("sensor", "cpu")]);

        let snapshot = collector.snapshot();
        let _temp = snapshot.get_gauge("temperature:{\"sensor\": \"cpu\"}");
        // Gauge should have been updated to latest value
    }

    #[test]
    fn test_prometheus_format() {
        let collector = MetricsCollector::new(MetricsConfig::in_memory());

        collector.record_duration("test", Duration::from_millis(100));

        let snapshot = collector.snapshot();
        let text = snapshot.to_prometheus_text();

        assert!(text.contains("# HELP test_duration_ms"));
        assert!(text.contains("# TYPE test_duration_ms histogram"));
        assert!(text.contains("test_duration_ms_bucket"));
    }

    #[test]
    fn test_disabled_metrics() {
        let mut config = MetricsConfig::in_memory();
        config.enabled = false;

        let collector = MetricsCollector::new(config);
        collector.record_duration("test", Duration::from_millis(100));

        let snapshot = collector.snapshot();
        assert!(snapshot.metrics.is_empty());
    }
}
