//! OpenTelemetry OTLP Export Configuration
//!
//! Provides initialization and configuration for exporting traces, metrics,
//! and logs via the OpenTelemetry Protocol (OTLP) to collectors like Jaeger,
//! Grafana Tempo, or any OTLP-compatible backend.
//!
//! All types and functions in this module are gated behind
//! `#[cfg(feature = "opentelemetry")]`. When the feature is disabled,
//! the module provides no-op stubs so the rest of the codebase compiles unchanged.
//!
//! # Environment Variables
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `VOIDBOX_OTLP_ENDPOINT` | (none) | OTLP gRPC endpoint; also respects `OTEL_EXPORTER_OTLP_ENDPOINT` |
//! | `VOIDBOX_OTLP_HEADERS` | (none) | Comma-separated `key=value` headers for auth |
//! | `VOIDBOX_SERVICE_NAME` | `void-box` | Service name in exported telemetry |
//! | `VOIDBOX_OTEL_DEBUG` | (none) | If set, enables verbose OTel internal logging |

/// Configuration for OTLP export, read from environment variables.
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// OTLP endpoint (gRPC).  `None` means OTLP export is disabled.
    pub endpoint: Option<String>,
    /// Extra headers for OTLP requests (e.g. auth tokens).
    pub headers: Vec<(String, String)>,
    /// Service name reported in all telemetry.
    pub service_name: String,
    /// Enable verbose OTel internal logging.
    pub debug: bool,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            headers: Vec::new(),
            service_name: "void-box".to_string(),
            debug: false,
        }
    }
}

impl OtlpConfig {
    /// Build an `OtlpConfig` from environment variables.
    ///
    /// Priority for the endpoint:
    /// 1. `VOIDBOX_OTLP_ENDPOINT`
    /// 2. `OTEL_EXPORTER_OTLP_ENDPOINT` (standard OTel env var)
    pub fn from_env() -> Self {
        let endpoint = std::env::var("VOIDBOX_OTLP_ENDPOINT")
            .ok()
            .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok())
            .filter(|s| !s.is_empty());

        let headers = std::env::var("VOIDBOX_OTLP_HEADERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next()?.trim().to_string();
                let value = parts.next()?.trim().to_string();
                if key.is_empty() {
                    None
                } else {
                    Some((key, value))
                }
            })
            .collect();

        let service_name =
            std::env::var("VOIDBOX_SERVICE_NAME").unwrap_or_else(|_| "void-box".to_string());

        let debug = std::env::var("VOIDBOX_OTEL_DEBUG").is_ok();

        Self {
            endpoint,
            headers,
            service_name,
            debug,
        }
    }

    /// Returns `true` if an OTLP endpoint is configured and export should happen.
    pub fn is_enabled(&self) -> bool {
        self.endpoint.is_some()
    }
}

// ---------------------------------------------------------------------------
// OTel SDK initialization (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "opentelemetry")]
mod otel_init {
    use super::OtlpConfig;
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::trace::{BatchSpanProcessor, SdkTracerProvider};
    use opentelemetry_sdk::Resource;

    /// Initialize an OTel `TracerProvider` that exports spans via OTLP/gRPC.
    ///
    /// Returns the provider and a `Tracer` handle.  Call `provider.shutdown()`
    /// when done to flush pending spans.
    pub fn init_otlp_tracer(
        config: &OtlpConfig,
    ) -> Result<SdkTracerProvider, Box<dyn std::error::Error>> {
        let endpoint = config
            .endpoint
            .as_deref()
            .ok_or("OTLP endpoint not configured")?;

        let exporter_builder = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint);

        // Note: header injection for tonic requires metadata; for simplicity
        // we rely on OTEL_EXPORTER_OTLP_HEADERS being picked up by the SDK.
        let exporter = exporter_builder.build()?;

        let resource = Resource::builder()
            .with_attributes([KeyValue::new("service.name", config.service_name.clone())])
            .build();

        let provider = SdkTracerProvider::builder()
            .with_span_processor(BatchSpanProcessor::builder(exporter).build())
            .with_resource(resource.clone())
            .build();

        // Set as global so other code can use `opentelemetry::global::tracer()`
        opentelemetry::global::set_tracer_provider(provider.clone());

        Ok(provider)
    }

    /// Initialize an OTel `MeterProvider` that exports metrics via OTLP/gRPC.
    pub fn init_otlp_metrics(
        config: &OtlpConfig,
    ) -> Result<SdkMeterProvider, Box<dyn std::error::Error>> {
        let endpoint = config
            .endpoint
            .as_deref()
            .ok_or("OTLP endpoint not configured")?;

        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?;

        let resource = Resource::builder()
            .with_attributes([KeyValue::new("service.name", config.service_name.clone())])
            .build();

        let reader = PeriodicReader::builder(exporter).build();

        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();

        // Set as global so MetricsCollector can use `opentelemetry::global::meter()`
        opentelemetry::global::set_meter_provider(provider.clone());

        Ok(provider)
    }

    /// Flush and shut down all global OTel providers.
    ///
    /// This drops the global tracer provider, which flushes pending spans.
    /// The MeterProvider is shut down on drop.
    pub fn shutdown_otlp(
        tracer_provider: Option<SdkTracerProvider>,
        meter_provider: Option<SdkMeterProvider>,
    ) {
        if let Some(tp) = tracer_provider {
            let _ = tp.shutdown();
        }
        if let Some(mp) = meter_provider {
            let _ = mp.shutdown();
        }
    }
}

#[cfg(feature = "opentelemetry")]
pub use otel_init::{init_otlp_metrics, init_otlp_tracer, shutdown_otlp};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env var tests must run serially since env vars are process-global.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_otlp_config_defaults() {
        let config = OtlpConfig::default();
        assert!(config.endpoint.is_none());
        assert!(!config.is_enabled());
        assert_eq!(config.service_name, "void-box");
        assert!(!config.debug);
        assert!(config.headers.is_empty());
    }

    #[test]
    fn test_otlp_config_from_env() {
        let _lock = ENV_LOCK.lock().unwrap();

        let _guard0 = TempEnv::remove("OTEL_EXPORTER_OTLP_ENDPOINT");
        let _guard1 = TempEnv::set("VOIDBOX_OTLP_ENDPOINT", "http://localhost:4317");
        let _guard2 = TempEnv::set("VOIDBOX_SERVICE_NAME", "my-service");
        let _guard3 = TempEnv::set(
            "VOIDBOX_OTLP_HEADERS",
            "authorization=Bearer token123,x-custom=value",
        );
        let _guard4 = TempEnv::set("VOIDBOX_OTEL_DEBUG", "1");

        let config = OtlpConfig::from_env();
        assert_eq!(config.endpoint, Some("http://localhost:4317".to_string()));
        assert!(config.is_enabled());
        assert_eq!(config.service_name, "my-service");
        assert!(config.debug);
        assert_eq!(config.headers.len(), 2);
        assert_eq!(
            config.headers[0],
            ("authorization".to_string(), "Bearer token123".to_string())
        );
        assert_eq!(
            config.headers[1],
            ("x-custom".to_string(), "value".to_string())
        );
    }

    #[test]
    fn test_otlp_config_respects_standard_otel_env() {
        let _lock = ENV_LOCK.lock().unwrap();

        let _guard1 = TempEnv::remove("VOIDBOX_OTLP_ENDPOINT");
        let _guard2 = TempEnv::set("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317");

        let config = OtlpConfig::from_env();
        assert_eq!(config.endpoint, Some("http://collector:4317".to_string()));
        assert!(config.is_enabled());
    }

    #[test]
    fn test_otlp_config_voidbox_takes_priority() {
        let _lock = ENV_LOCK.lock().unwrap();

        let _guard1 = TempEnv::set("VOIDBOX_OTLP_ENDPOINT", "http://voidbox:4317");
        let _guard2 = TempEnv::set("OTEL_EXPORTER_OTLP_ENDPOINT", "http://otel:4317");

        let config = OtlpConfig::from_env();
        assert_eq!(config.endpoint, Some("http://voidbox:4317".to_string()));
    }

    // Simple RAII guard for temporarily setting/unsetting env vars in tests.
    struct TempEnv {
        key: String,
        previous: Option<String>,
    }

    impl TempEnv {
        fn set(key: &str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key: key.to_string(),
                previous,
            }
        }

        fn remove(key: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::remove_var(key);
            Self {
                key: key.to_string(),
                previous,
            }
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            match &self.previous {
                Some(val) => std::env::set_var(&self.key, val),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}
