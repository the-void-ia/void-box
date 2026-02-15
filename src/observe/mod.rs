//! Native Observability Module
//!
//! This module provides first-class observability for void-box workflows:
//! - OpenTelemetry tracing integration
//! - Prometheus-compatible metrics
//! - Structured logging with correlation IDs
//!
//! These capabilities are built in and available for every observed workflow run.
//!
//! # Example
//!
//! ```no_run
//! use void_box::observe::{ObserveConfig, Tracer, MetricsCollector};
//!
//! // Configure observability
//! let config = ObserveConfig::default()
//!     .otlp_endpoint("http://jaeger:4317")
//!     .enable_metrics(true)
//!     .enable_logs(true);
//!
//! // Traces, metrics, and logs are automatically captured during workflow execution
//! ```

pub mod claude;
pub mod logs;
pub mod metrics;
pub mod otlp;
pub mod telemetry;
pub mod tracer;

use std::sync::Arc;
#[cfg(feature = "opentelemetry")]
use std::sync::Mutex;
#[cfg(feature = "opentelemetry")]
use std::sync::OnceLock;
use std::time::Instant;

pub use logs::{LogConfig, LogEntry, LogLevel, StructuredLogger};
pub use metrics::{MetricsCollector, MetricsConfig, MetricsSnapshot};
pub use tracer::{Span, SpanContext, SpanStatus, Tracer, TracerConfig};

/// Configuration for observability features
#[derive(Debug, Clone)]
pub struct ObserveConfig {
    /// OpenTelemetry configuration
    pub tracer: TracerConfig,
    /// Metrics configuration
    pub metrics: MetricsConfig,
    /// Logging configuration
    pub logs: LogConfig,
    /// Enable real-time WebSocket updates
    pub enable_websocket: bool,
    /// Enable point-in-time snapshots
    pub enable_snapshot: bool,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
            tracer: TracerConfig::default(),
            metrics: MetricsConfig::default(),
            logs: LogConfig::default(),
            enable_websocket: false,
            enable_snapshot: true,
        }
    }
}

impl ObserveConfig {
    /// Create a new observability configuration with defaults
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an observability configuration from environment variables.
    ///
    /// Reads `VOIDBOX_OTLP_ENDPOINT` (or `OTEL_EXPORTER_OTLP_ENDPOINT`),
    /// `VOIDBOX_SERVICE_NAME`, `VOIDBOX_OTLP_HEADERS`, and `VOIDBOX_OTEL_DEBUG`.
    /// If an endpoint is found, configures the tracer for OTLP export.
    pub fn from_env() -> Self {
        let otlp = otlp::OtlpConfig::from_env();
        let mut config = Self::default();
        if let Some(ref endpoint) = otlp.endpoint {
            config.tracer.otlp_endpoint = Some(endpoint.clone());
            config.tracer.service_name = otlp.service_name.clone();
        }
        config
    }

    /// Configure for testing (in-memory collectors)
    pub fn test() -> Self {
        Self {
            tracer: TracerConfig::in_memory(),
            metrics: MetricsConfig::in_memory(),
            logs: LogConfig::in_memory(),
            enable_websocket: false,
            enable_snapshot: true,
        }
    }

    /// Set the OTLP endpoint for trace export
    pub fn otlp_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.tracer.otlp_endpoint = Some(endpoint.into());
        self
    }

    /// Enable or disable metrics collection
    pub fn enable_metrics(mut self, enable: bool) -> Self {
        self.metrics.enabled = enable;
        self
    }

    /// Enable or disable log collection
    pub fn enable_logs(mut self, enable: bool) -> Self {
        self.logs.enabled = enable;
        self
    }

    /// Set the log level
    pub fn log_level(mut self, level: LogLevel) -> Self {
        self.logs.level = level;
        self
    }

    /// Enable WebSocket for real-time updates
    pub fn enable_websocket(mut self, enable: bool) -> Self {
        self.enable_websocket = enable;
        self
    }
}

/// Observer instance that collects traces, metrics, and logs
#[derive(Clone)]
pub struct Observer {
    #[allow(dead_code)]
    config: ObserveConfig,
    tracer: Arc<Tracer>,
    metrics: Arc<MetricsCollector>,
    logger: Arc<StructuredLogger>,
}

#[cfg(feature = "opentelemetry")]
#[derive(Default)]
struct OtlpProviderState {
    tracer: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    meter: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
}

/// Force-flush globally configured OTLP providers.
///
/// This is primarily useful in short-lived binaries (examples/CLI tools) so
/// traces and metrics are exported before process exit.
#[cfg(feature = "opentelemetry")]
pub fn flush_global_otel() -> crate::Result<()> {
    let Some(state) = otlp_provider_state().get() else {
        return Ok(());
    };

    let mut state = state
        .lock()
        .map_err(|e| crate::Error::Guest(format!("OTLP provider mutex poisoned: {e}")))?;
    let tracer = state.tracer.take();
    let meter = state.meter.take();
    drop(state);

    let mut errors = Vec::new();

    if let Some(tracer) = tracer {
        if let Err(e) = tracer.force_flush() {
            errors.push(format!("Failed to flush OTLP tracer: {e}"));
        }
        if let Err(e) = tracer.shutdown() {
            errors.push(format!("Failed to shutdown OTLP tracer: {e}"));
        }
    }

    if let Some(meter) = meter {
        if let Err(e) = meter.force_flush() {
            errors.push(format!("Failed to flush OTLP metrics: {e}"));
        }
        if let Err(e) = meter.shutdown() {
            errors.push(format!("Failed to shutdown OTLP metrics: {e}"));
        }
    }

    if !errors.is_empty() {
        return Err(crate::Error::Guest(errors.join("; ")));
    }

    Ok(())
}

/// No-op when OpenTelemetry feature is disabled.
#[cfg(not(feature = "opentelemetry"))]
pub fn flush_global_otel() -> crate::Result<()> {
    Ok(())
}

impl Observer {
    /// Create a new observer with the given configuration
    pub fn new(config: ObserveConfig) -> Self {
        #[cfg(feature = "opentelemetry")]
        maybe_init_global_otel(&config);
        let tracer = Arc::new(Tracer::new(config.tracer.clone()));
        let metrics = Arc::new(build_metrics_collector(&config));
        let logger = Arc::new(StructuredLogger::new(config.logs.clone()));

        Self {
            config,
            tracer,
            metrics,
            logger,
        }
    }

    /// Create a test observer that captures everything in memory
    pub fn test() -> Self {
        Self::new(ObserveConfig::test())
    }

    /// Get the tracer
    pub fn tracer(&self) -> &Arc<Tracer> {
        &self.tracer
    }

    /// Get the metrics collector
    pub fn metrics(&self) -> &Arc<MetricsCollector> {
        &self.metrics
    }

    /// Get the structured logger
    pub fn logger(&self) -> &Arc<StructuredLogger> {
        &self.logger
    }

    /// Start a new span for a workflow
    pub fn start_workflow_span(&self, name: &str) -> SpanGuard {
        let span = self.tracer.start_span(&format!("workflow:{}", name));
        self.logger
            .info(&format!("Starting workflow: {}", name), &[]);
        SpanGuard {
            span,
            tracer: self.tracer.clone(),
            metrics: self.metrics.clone(),
            logger: self.logger.clone(),
            start_time: Instant::now(),
            name: name.to_string(),
        }
    }

    /// Start a new span for a workflow step
    pub fn start_step_span(&self, name: &str, parent: Option<&SpanContext>) -> SpanGuard {
        let span = if let Some(parent) = parent {
            self.tracer
                .start_span_with_parent(&format!("step:{}", name), parent)
        } else {
            self.tracer.start_span(&format!("step:{}", name))
        };
        self.logger.debug(&format!("Starting step: {}", name), &[]);
        SpanGuard {
            span,
            tracer: self.tracer.clone(),
            metrics: self.metrics.clone(),
            logger: self.logger.clone(),
            start_time: Instant::now(),
            name: name.to_string(),
        }
    }

    /// Get collected traces
    pub fn get_traces(&self) -> Vec<Span> {
        self.tracer.get_spans()
    }

    /// Get collected metrics
    pub fn get_metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Get collected logs
    pub fn get_logs(&self) -> Vec<LogEntry> {
        self.logger.get_entries()
    }

    /// Check if a span with the given name exists
    pub fn has_span(&self, name: &str) -> bool {
        self.tracer.get_spans().iter().any(|s| s.name == name)
    }
}

fn build_metrics_collector(config: &ObserveConfig) -> MetricsCollector {
    #[cfg(feature = "opentelemetry")]
    {
        maybe_init_global_otel(config);
        if config.tracer.otlp_endpoint.is_some() {
            let meter = opentelemetry::global::meter("void-box");
            return MetricsCollector::with_otel_meter(config.metrics.clone(), meter);
        }
    }

    MetricsCollector::new(config.metrics.clone())
}

#[cfg(feature = "opentelemetry")]
fn maybe_init_global_otel(config: &ObserveConfig) {
    static OTEL_INIT: OnceLock<()> = OnceLock::new();
    OTEL_INIT.get_or_init(|| {
        let Some(endpoint) = config.tracer.otlp_endpoint.clone() else {
            return;
        };

        let otlp_config = crate::observe::otlp::OtlpConfig {
            endpoint: Some(endpoint),
            headers: Vec::new(),
            service_name: config.tracer.service_name.clone(),
            debug: false,
        };

        let mut state = OtlpProviderState::default();

        match crate::observe::otlp::init_otlp_tracer(&otlp_config) {
            Ok(provider) => state.tracer = Some(provider),
            Err(e) => eprintln!("[observe] WARN: failed to initialize OTLP tracer export: {e}"),
        }
        match crate::observe::otlp::init_otlp_metrics(&otlp_config) {
            Ok(provider) => state.meter = Some(provider),
            Err(e) => eprintln!("[observe] WARN: failed to initialize OTLP metrics export: {e}"),
        }

        if state.tracer.is_some() || state.meter.is_some() {
            let lock =
                otlp_provider_state().get_or_init(|| Mutex::new(OtlpProviderState::default()));
            if let Ok(mut slot) = lock.lock() {
                *slot = state;
            }
        }
    });
}

#[cfg(feature = "opentelemetry")]
fn otlp_provider_state() -> &'static OnceLock<Mutex<OtlpProviderState>> {
    static OTLP_PROVIDER_STATE: OnceLock<Mutex<OtlpProviderState>> = OnceLock::new();
    &OTLP_PROVIDER_STATE
}

/// RAII guard for spans that records metrics on drop
pub struct SpanGuard {
    span: Span,
    tracer: Arc<Tracer>,
    metrics: Arc<MetricsCollector>,
    logger: Arc<StructuredLogger>,
    start_time: Instant,
    name: String,
}

impl SpanGuard {
    /// Get the span context for creating child spans
    pub fn context(&self) -> SpanContext {
        self.span.context.clone()
    }

    /// Mark the span as successful
    pub fn set_ok(mut self) {
        self.span.status = SpanStatus::Ok;
        self.finish();
    }

    /// Mark the span as failed with an error message
    pub fn set_error(mut self, message: &str) {
        self.span.status = SpanStatus::Error(message.to_string());
        self.logger
            .error(&format!("Step {} failed: {}", self.name, message), &[]);
        self.finish();
    }

    /// Add an attribute to the span
    pub fn set_attribute(&mut self, key: &str, value: impl Into<String>) {
        self.span.attributes.insert(key.to_string(), value.into());
    }

    /// Record stdout output
    pub fn record_stdout(&mut self, size: usize) {
        self.span
            .attributes
            .insert("stdout_bytes".to_string(), size.to_string());
    }

    /// Record stderr output
    pub fn record_stderr(&mut self, size: usize) {
        self.span
            .attributes
            .insert("stderr_bytes".to_string(), size.to_string());
    }

    /// Record the command that was executed
    pub fn record_exec(&mut self, program: &str, args: &[&str]) {
        let cmd = format!("{} {}", program, args.join(" "));
        self.span.attributes.insert("exec".to_string(), cmd);
    }

    fn finish(mut self) {
        let duration = self.start_time.elapsed();
        self.span.duration = Some(duration);

        // Record metrics
        self.metrics.record_duration(&self.name, duration);

        // Record to tracer
        self.tracer.finish_span(self.span.clone());

        self.logger.debug(
            &format!("Finished {}: {:?}", self.name, duration),
            &[("duration_ms", &duration.as_millis().to_string())],
        );
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        // If not explicitly finished, mark as completed
        if self.span.duration.is_none() {
            let duration = self.start_time.elapsed();
            self.span.duration = Some(duration);
            self.metrics.record_duration(&self.name, duration);
            self.tracer.finish_span(self.span.clone());
        }
    }
}

/// Result of an observed workflow execution
#[derive(Debug, Clone)]
pub struct ObservedResult<T> {
    /// The actual result
    pub result: T,
    /// Collected traces
    traces: Vec<Span>,
    /// Collected metrics
    metrics: MetricsSnapshot,
    /// Collected logs
    logs: Vec<LogEntry>,
}

impl<T> ObservedResult<T> {
    /// Create a new observed result
    pub fn new(result: T, observer: &Observer) -> Self {
        Self {
            result,
            traces: observer.get_traces(),
            metrics: observer.get_metrics(),
            logs: observer.get_logs(),
        }
    }

    /// Get the traces
    pub fn traces(&self) -> &[Span] {
        &self.traces
    }

    /// Get the metrics
    pub fn metrics(&self) -> &MetricsSnapshot {
        &self.metrics
    }

    /// Get the logs
    pub fn logs(&self) -> &[LogEntry] {
        &self.logs
    }

    /// Map the result value
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> ObservedResult<U> {
        ObservedResult {
            result: f(self.result),
            traces: self.traces,
            metrics: self.metrics,
            logs: self.logs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_observe_config_default() {
        let config = ObserveConfig::default();
        assert!(config.enable_snapshot);
        assert!(!config.enable_websocket);
    }

    #[test]
    fn test_observe_config_builder() {
        let config = ObserveConfig::new()
            .otlp_endpoint("http://localhost:4317")
            .enable_metrics(true)
            .log_level(LogLevel::Debug);

        assert_eq!(
            config.tracer.otlp_endpoint,
            Some("http://localhost:4317".to_string())
        );
        assert!(config.metrics.enabled);
        assert_eq!(config.logs.level, LogLevel::Debug);
    }

    #[test]
    fn test_observer_workflow_span() {
        let observer = Observer::test();

        {
            let span = observer.start_workflow_span("test-workflow");
            span.set_ok();
        }

        let traces = observer.get_traces();
        assert!(!traces.is_empty());
        assert!(traces.iter().any(|s| s.name == "workflow:test-workflow"));
    }

    #[test]
    fn test_observer_step_span() {
        let observer = Observer::test();

        {
            let workflow_span = observer.start_workflow_span("test-workflow");
            let ctx = workflow_span.context();

            {
                let mut step_span = observer.start_step_span("step1", Some(&ctx));
                step_span.record_exec("echo", &["hello"]);
                step_span.set_ok();
            }

            workflow_span.set_ok();
        }

        let traces = observer.get_traces();
        assert!(traces.iter().any(|s| s.name == "step:step1"));
    }

    #[test]
    fn test_observer_metrics() {
        let observer = Observer::test();

        {
            let span = observer.start_workflow_span("test");
            std::thread::sleep(std::time::Duration::from_millis(10));
            span.set_ok();
        }

        let metrics = observer.get_metrics();
        assert!(metrics.get("test_duration_ms").is_some());
    }

    #[test]
    fn test_has_span() {
        let observer = Observer::test();

        {
            let span = observer.start_workflow_span("my-workflow");
            span.set_ok();
        }

        assert!(observer.has_span("workflow:my-workflow"));
        assert!(!observer.has_span("workflow:other"));
    }
}
