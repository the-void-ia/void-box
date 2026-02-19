//! OpenTelemetry Tracing Integration
//!
//! Provides distributed tracing for workflow execution with support for:
//! - Span creation and hierarchy
//! - Trace context propagation
//! - OTLP export to collectors like Jaeger
//! - In-memory collection for testing

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Configuration for the tracer
#[derive(Debug, Clone)]
pub struct TracerConfig {
    /// Service name for traces
    pub service_name: String,
    /// OTLP endpoint for exporting traces
    pub otlp_endpoint: Option<String>,
    /// Sample rate (0.0 to 1.0)
    pub sample_rate: f64,
    /// Maximum spans to keep in memory
    pub max_spans: usize,
    /// Enable in-memory collection (for testing)
    pub in_memory: bool,
}

impl Default for TracerConfig {
    fn default() -> Self {
        Self {
            service_name: "void-box".to_string(),
            otlp_endpoint: None,
            sample_rate: 1.0,
            max_spans: 10000,
            in_memory: false,
        }
    }
}

impl TracerConfig {
    /// Create a config for in-memory tracing (useful for tests)
    pub fn in_memory() -> Self {
        Self {
            in_memory: true,
            ..Default::default()
        }
    }

    /// Set the service name
    pub fn service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }

    /// Set the OTLP endpoint
    pub fn otlp_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.otlp_endpoint = Some(endpoint.into());
        self
    }

    /// Set the sample rate
    pub fn sample_rate(mut self, rate: f64) -> Self {
        self.sample_rate = rate.clamp(0.0, 1.0);
        self
    }
}

/// Trace context for propagation
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanContext {
    /// Trace ID (128-bit)
    pub trace_id: String,
    /// Span ID (64-bit)
    pub span_id: String,
    /// Parent span ID (if any)
    pub parent_span_id: Option<String>,
    /// Trace flags
    pub trace_flags: u8,
}

impl SpanContext {
    /// Generate a new trace ID
    pub fn new_trace_id() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{:032x}", now)
    }

    /// Generate a new span ID
    pub fn new_span_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);

        format!("{:016x}", now ^ counter)
    }
}

/// Status of a span
#[derive(Debug, Default, Clone, PartialEq)]
pub enum SpanStatus {
    /// Unset status
    #[default]
    Unset,
    /// Operation completed successfully
    Ok,
    /// Operation failed with error
    Error(String),
}

/// A span representing a unit of work
#[derive(Debug, Clone)]
pub struct Span {
    /// Span name
    pub name: String,
    /// Span context (trace/span IDs)
    pub context: SpanContext,
    /// Start time
    pub start_time: SystemTime,
    /// Duration (set when span ends)
    pub duration: Option<Duration>,
    /// Span status
    pub status: SpanStatus,
    /// Span attributes (key-value pairs)
    pub attributes: HashMap<String, String>,
    /// Events/logs within the span
    pub events: Vec<SpanEvent>,
}

impl Span {
    /// Create a new span
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            context: SpanContext {
                trace_id: SpanContext::new_trace_id(),
                span_id: SpanContext::new_span_id(),
                parent_span_id: None,
                trace_flags: 1, // Sampled
            },
            start_time: SystemTime::now(),
            duration: None,
            status: SpanStatus::Unset,
            attributes: HashMap::new(),
            events: Vec::new(),
        }
    }

    /// Create a child span
    pub fn child(name: &str, parent: &SpanContext) -> Self {
        Self {
            name: name.to_string(),
            context: SpanContext {
                trace_id: parent.trace_id.clone(),
                span_id: SpanContext::new_span_id(),
                parent_span_id: Some(parent.span_id.clone()),
                trace_flags: parent.trace_flags,
            },
            start_time: SystemTime::now(),
            duration: None,
            status: SpanStatus::Unset,
            attributes: HashMap::new(),
            events: Vec::new(),
        }
    }

    /// Add an attribute to the span
    pub fn set_attribute(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attributes.insert(key.into(), value.into());
    }

    /// Add an event to the span
    pub fn add_event(&mut self, name: impl Into<String>) {
        self.events.push(SpanEvent {
            name: name.into(),
            timestamp: SystemTime::now(),
            attributes: HashMap::new(),
        });
    }

    /// Add an event with attributes
    pub fn add_event_with_attrs(
        &mut self,
        name: impl Into<String>,
        attrs: HashMap<String, String>,
    ) {
        self.events.push(SpanEvent {
            name: name.into(),
            timestamp: SystemTime::now(),
            attributes: attrs,
        });
    }

    /// End the span
    pub fn end(&mut self) {
        if self.duration.is_none() {
            self.duration = Some(
                SystemTime::now()
                    .duration_since(self.start_time)
                    .unwrap_or_default(),
            );
        }
    }
}

/// An event within a span
#[derive(Debug, Clone)]
pub struct SpanEvent {
    /// Event name
    pub name: String,
    /// Event timestamp
    pub timestamp: SystemTime,
    /// Event attributes
    pub attributes: HashMap<String, String>,
}

/// Tracer for creating and managing spans
pub struct Tracer {
    config: TracerConfig,
    /// In-memory span storage
    spans: Mutex<Vec<Span>>,
    /// OTel SDK tracer (when feature enabled and endpoint configured)
    #[cfg(feature = "opentelemetry")]
    otel_tracer: Option<OtelBridge>,
}

/// Bridge to the OTel SDK tracer.
#[cfg(feature = "opentelemetry")]
struct OtelBridge {
    tracer: opentelemetry::global::BoxedTracer,
}

#[cfg(feature = "opentelemetry")]
impl OtelBridge {
    fn export_span(&self, span: &Span) {
        use opentelemetry::trace::{SpanKind, TraceContextExt, Tracer as OtelTracer};
        use opentelemetry::{Context, KeyValue};

        // Parse our internal IDs into OTel types so the SDK uses our IDs
        // instead of generating its own. This ensures parent-child references
        // resolve correctly within a single trace.
        let our_trace_id = opentelemetry::trace::TraceId::from_hex(&span.context.trace_id)
            .unwrap_or(opentelemetry::trace::TraceId::INVALID);
        let our_span_id = opentelemetry::trace::SpanId::from_hex(&span.context.span_id)
            .unwrap_or(opentelemetry::trace::SpanId::INVALID);

        // Build parent context if this span has a parent
        let parent_ctx = if let Some(ref parent_id) = span.context.parent_span_id {
            let parent_span_id = opentelemetry::trace::SpanId::from_hex(parent_id)
                .unwrap_or(opentelemetry::trace::SpanId::INVALID);
            let span_ctx = opentelemetry::trace::SpanContext::new(
                our_trace_id,
                parent_span_id,
                opentelemetry::trace::TraceFlags::SAMPLED,
                true, // remote
                opentelemetry::trace::TraceState::default(),
            );
            Context::new().with_remote_span_context(span_ctx)
        } else {
            Context::new()
        };

        let mut builder = opentelemetry::trace::SpanBuilder::from_name(span.name.clone());
        builder = builder
            .with_kind(SpanKind::Internal)
            .with_start_time(span.start_time)
            .with_attributes(
                span.attributes
                    .iter()
                    .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
            );

        // Force our trace_id for root spans (child spans inherit from parent context).
        // Force our span_id always so child spans' parent references resolve.
        if span.context.parent_span_id.is_none() {
            builder.trace_id = Some(our_trace_id);
        }
        builder.span_id = Some(our_span_id);

        let mut otel_span = self.tracer.build_with_context(builder, &parent_ctx);

        // Set status
        use opentelemetry::trace::Span as OtelSpanTrait;
        match &span.status {
            SpanStatus::Ok => otel_span.set_status(opentelemetry::trace::Status::Ok),
            SpanStatus::Error(msg) => {
                otel_span.set_status(opentelemetry::trace::Status::error(msg.clone()));
            }
            SpanStatus::Unset => {}
        }

        // Add events
        for event in &span.events {
            let attrs: Vec<KeyValue> = event
                .attributes
                .iter()
                .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
                .collect();
            otel_span.add_event_with_timestamp(event.name.clone(), event.timestamp, attrs);
        }

        // End the span (with duration if set)
        if let Some(duration) = span.duration {
            let end_time = span.start_time + duration;
            otel_span.end_with_timestamp(end_time);
        } else {
            otel_span.end();
        }
    }
}

impl Tracer {
    /// Create a new tracer
    pub fn new(config: TracerConfig) -> Self {
        #[cfg(feature = "opentelemetry")]
        let otel_tracer = if config.otlp_endpoint.is_some() {
            Some(OtelBridge {
                tracer: opentelemetry::global::tracer(config.service_name.clone()),
            })
        } else {
            None
        };

        Self {
            config,
            spans: Mutex::new(Vec::new()),
            #[cfg(feature = "opentelemetry")]
            otel_tracer,
        }
    }

    /// Start a new root span
    pub fn start_span(&self, name: &str) -> Span {
        Span::new(name)
    }

    /// Start a child span
    pub fn start_span_with_parent(&self, name: &str, parent: &SpanContext) -> Span {
        Span::child(name, parent)
    }

    /// Record a finished span
    pub fn finish_span(&self, mut span: Span) {
        span.end();

        // Always store in-memory when configured
        if self.config.in_memory {
            let mut spans = self.spans.lock().unwrap();
            if spans.len() >= self.config.max_spans {
                spans.remove(0);
            }
            spans.push(span.clone());
        }

        // Export to OTel SDK when available
        #[cfg(feature = "opentelemetry")]
        if let Some(ref bridge) = self.otel_tracer {
            bridge.export_span(&span);
        }
    }

    /// Get all collected spans (for testing)
    pub fn get_spans(&self) -> Vec<Span> {
        self.spans.lock().unwrap().clone()
    }

    /// Clear collected spans
    pub fn clear_spans(&self) {
        self.spans.lock().unwrap().clear();
    }

    /// Find spans by name prefix
    pub fn find_spans(&self, prefix: &str) -> Vec<Span> {
        self.spans
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.name.starts_with(prefix))
            .cloned()
            .collect()
    }
}

impl SpanContext {
    /// Format this context as a W3C Trace Context `traceparent` header value.
    ///
    /// Format: `00-{trace_id}-{span_id}-{trace_flags:02x}`
    ///
    /// See <https://www.w3.org/TR/trace-context/#traceparent-header>
    pub fn to_traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id, self.span_id, self.trace_flags
        )
    }

    /// Parse a W3C `traceparent` header value into a `SpanContext`.
    ///
    /// Returns `None` if the string is malformed.
    pub fn from_traceparent(value: &str) -> Option<Self> {
        let parts: Vec<&str> = value.split('-').collect();
        if parts.len() != 4 || parts[0] != "00" {
            return None;
        }
        let trace_id = parts[1].to_string();
        let span_id = parts[2].to_string();
        let trace_flags = u8::from_str_radix(parts[3], 16).ok()?;

        if trace_id.len() != 32 || span_id.len() != 16 {
            return None;
        }

        Some(Self {
            trace_id,
            span_id,
            parent_span_id: None,
            trace_flags,
        })
    }
}

/// Builder for creating spans with a fluent API
pub struct SpanBuilder<'a> {
    #[allow(dead_code)]
    tracer: &'a Tracer,
    name: String,
    parent: Option<SpanContext>,
    attributes: HashMap<String, String>,
}

impl<'a> SpanBuilder<'a> {
    /// Create a new span builder
    pub fn new(tracer: &'a Tracer, name: impl Into<String>) -> Self {
        Self {
            tracer,
            name: name.into(),
            parent: None,
            attributes: HashMap::new(),
        }
    }

    /// Set the parent span
    pub fn with_parent(mut self, parent: &SpanContext) -> Self {
        self.parent = Some(parent.clone());
        self
    }

    /// Add an attribute
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Build and start the span
    pub fn start(self) -> Span {
        let mut span = if let Some(parent) = self.parent {
            Span::child(&self.name, &parent)
        } else {
            Span::new(&self.name)
        };

        for (k, v) in self.attributes {
            span.set_attribute(k, v);
        }

        span
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_span_creation() {
        let span = Span::new("test-span");
        assert_eq!(span.name, "test-span");
        assert!(span.duration.is_none());
        assert_eq!(span.status, SpanStatus::Unset);
    }

    #[test]
    fn test_child_span() {
        let parent = Span::new("parent");
        let child = Span::child("child", &parent.context);

        assert_eq!(child.context.trace_id, parent.context.trace_id);
        assert_eq!(
            child.context.parent_span_id,
            Some(parent.context.span_id.clone())
        );
        assert_ne!(child.context.span_id, parent.context.span_id);
    }

    #[test]
    fn test_span_attributes() {
        let mut span = Span::new("test");
        span.set_attribute("key1", "value1");
        span.set_attribute("key2", "value2");

        assert_eq!(span.attributes.get("key1"), Some(&"value1".to_string()));
        assert_eq!(span.attributes.get("key2"), Some(&"value2".to_string()));
    }

    #[test]
    fn test_span_events() {
        let mut span = Span::new("test");
        span.add_event("event1");
        span.add_event("event2");

        assert_eq!(span.events.len(), 2);
        assert_eq!(span.events[0].name, "event1");
    }

    #[test]
    fn test_tracer_in_memory() {
        let tracer = Tracer::new(TracerConfig::in_memory());

        let mut span = tracer.start_span("test");
        span.status = SpanStatus::Ok;
        tracer.finish_span(span);

        let spans = tracer.get_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "test");
    }

    #[test]
    fn test_span_builder() {
        let tracer = Tracer::new(TracerConfig::in_memory());

        let span = SpanBuilder::new(&tracer, "test")
            .with_attribute("key", "value")
            .start();

        assert_eq!(span.name, "test");
        assert_eq!(span.attributes.get("key"), Some(&"value".to_string()));
    }

    #[test]
    fn test_find_spans() {
        let tracer = Tracer::new(TracerConfig::in_memory());

        tracer.finish_span(Span::new("workflow:test1"));
        tracer.finish_span(Span::new("workflow:test2"));
        tracer.finish_span(Span::new("step:step1"));

        let workflows = tracer.find_spans("workflow:");
        assert_eq!(workflows.len(), 2);

        let steps = tracer.find_spans("step:");
        assert_eq!(steps.len(), 1);
    }

    #[test]
    fn test_span_end() {
        let mut span = Span::new("test");
        assert!(span.duration.is_none());

        std::thread::sleep(std::time::Duration::from_millis(10));
        span.end();

        assert!(span.duration.is_some());
        assert!(span.duration.unwrap().as_millis() >= 10);
    }
}
