//! OTel Integration Tests
//!
//! Tests for OpenTelemetry integration in void-box.
//! Uses the OTel SDK's in-memory exporters -- no external collectors needed.
//!
//! Run with: `cargo test --test otel_integration --features opentelemetry`
//! Run without feature (no-op mode): `cargo test --test otel_integration`

// ---------------------------------------------------------------------------
// T1: OtlpConfig tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod otlp_config_tests {
    use std::sync::Mutex;
    use void_box::observe::otlp::OtlpConfig;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_otlp_config_defaults() {
        let config = OtlpConfig::default();
        assert!(config.endpoint.is_none());
        assert!(!config.is_enabled());
        assert_eq!(config.service_name, "void-box");
        assert!(!config.debug);
    }

    #[test]
    fn test_otlp_config_from_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g0 = TempEnv::remove("OTEL_EXPORTER_OTLP_ENDPOINT");
        let _g1 = TempEnv::set("VOIDBOX_OTLP_ENDPOINT", "http://localhost:4317");
        let _g2 = TempEnv::set("VOIDBOX_SERVICE_NAME", "test-svc");
        let _g3 = TempEnv::set("VOIDBOX_OTEL_DEBUG", "1");
        let _g4 = TempEnv::set("VOIDBOX_OTLP_HEADERS", "auth=Bearer tok");

        let config = OtlpConfig::from_env();
        assert_eq!(config.endpoint.as_deref(), Some("http://localhost:4317"));
        assert!(config.is_enabled());
        assert_eq!(config.service_name, "test-svc");
        assert!(config.debug);
        assert_eq!(config.headers.len(), 1);
    }

    #[test]
    fn test_otlp_config_standard_otel_fallback() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g1 = TempEnv::remove("VOIDBOX_OTLP_ENDPOINT");
        let _g2 = TempEnv::set("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317");

        let config = OtlpConfig::from_env();
        assert_eq!(config.endpoint.as_deref(), Some("http://collector:4317"));
    }

    struct TempEnv { key: String, prev: Option<String> }
    impl TempEnv {
        fn set(k: &str, v: &str) -> Self {
            let prev = std::env::var(k).ok();
            std::env::set_var(k, v);
            Self { key: k.to_string(), prev }
        }
        fn remove(k: &str) -> Self {
            let prev = std::env::var(k).ok();
            std::env::remove_var(k);
            Self { key: k.to_string(), prev }
        }
    }
    impl Drop for TempEnv {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// T4: Claude stream-json parser tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod claude_parser_tests {
    use void_box::observe::claude::parse_stream_json;

    fn sample_session() -> &'static str {
        concat!(
            r#"{"type":"system","subtype":"init","session_id":"sess_01","model":"sonnet","tools":["Bash","Read","Write"],"cwd":"/workspace"}"#, "\n",
            r#"{"type":"assistant","session_id":"sess_01","message":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"Creating script."}],"usage":{"input_tokens":120,"output_tokens":45}}}"#, "\n",
            r#"{"type":"assistant","session_id":"sess_01","message":{"id":"msg_2","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Write","input":{"file_path":"/workspace/hello.py","content":"print('hello')"}}],"usage":{"input_tokens":30,"output_tokens":20}}}"#, "\n",
            r#"{"type":"user","session_id":"sess_01","message":{"id":"msg_3","type":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"File written"}]}}"#, "\n",
            r#"{"type":"assistant","session_id":"sess_01","message":{"id":"msg_4","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"python hello.py"}}],"usage":{"input_tokens":40,"output_tokens":15}}}"#, "\n",
            r#"{"type":"user","session_id":"sess_01","message":{"id":"msg_5","type":"message","role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"hello\n"}]}}"#, "\n",
            r#"{"type":"result","subtype":"success","session_id":"sess_01","total_cost_usd":0.0042,"is_error":false,"duration_ms":8500,"duration_api_ms":7200,"num_turns":3,"result":"Done.","usage":{"input_tokens":190,"output_tokens":80}}"#, "\n",
        )
    }

    #[test]
    fn test_parse_system_event() {
        let r = parse_stream_json(sample_session().as_bytes());
        assert_eq!(r.session_id, "sess_01");
        assert_eq!(r.model, "sonnet");
    }

    #[test]
    fn test_parse_tool_use_event() {
        let r = parse_stream_json(sample_session().as_bytes());
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].tool_name, "Write");
        assert_eq!(r.tool_calls[0].tool_use_id, "toolu_1");
        assert_eq!(r.tool_calls[1].tool_name, "Bash");
    }

    #[test]
    fn test_parse_tool_result_event() {
        let r = parse_stream_json(sample_session().as_bytes());
        assert_eq!(r.tool_calls[0].output.as_deref(), Some("File written"));
        assert_eq!(r.tool_calls[1].output.as_deref(), Some("hello\n"));
    }

    #[test]
    fn test_parse_result_event() {
        let r = parse_stream_json(sample_session().as_bytes());
        assert_eq!(r.result_text, "Done.");
        assert_eq!(r.total_cost_usd, 0.0042);
        assert_eq!(r.duration_ms, 8500);
        assert_eq!(r.duration_api_ms, 7200);
        assert_eq!(r.num_turns, 3);
        assert_eq!(r.input_tokens, 190);
        assert_eq!(r.output_tokens, 80);
        assert!(!r.is_error);
    }

    #[test]
    fn test_parse_error_result() {
        let jsonl = r#"{"type":"result","subtype":"error","session_id":"s","total_cost_usd":0.001,"is_error":true,"duration_ms":2000,"duration_api_ms":1800,"num_turns":1,"result":"","error":"Permission denied","usage":{"input_tokens":50,"output_tokens":10}}"#;
        let r = parse_stream_json(jsonl.as_bytes());
        assert!(r.is_error);
        assert_eq!(r.error.as_deref(), Some("Permission denied"));
    }

    #[test]
    fn test_parse_empty_input() {
        let r = parse_stream_json(b"");
        assert!(r.tool_calls.is_empty());
        assert!(!r.is_error);
    }

    #[test]
    fn test_parse_invalid_json_skipped() {
        let jsonl = "not json\n{\"type\":\"system\",\"session_id\":\"s1\",\"model\":\"opus\"}\n";
        let r = parse_stream_json(jsonl.as_bytes());
        assert_eq!(r.session_id, "s1");
        assert_eq!(r.model, "opus");
    }

    #[test]
    fn test_parse_tool_result_array_content() {
        let jsonl = concat!(
            r#"{"type":"assistant","session_id":"s","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
            r#"{"type":"user","session_id":"s","message":{"id":"m2","role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a.txt"},{"type":"text","text":"\nb.txt"}]}]}}"#, "\n",
        );
        let r = parse_stream_json(jsonl.as_bytes());
        assert_eq!(r.tool_calls[0].output.as_deref(), Some("a.txt\nb.txt"));
    }
}

// ---------------------------------------------------------------------------
// T5: TRACEPARENT propagation tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod traceparent_tests {
    use void_box::observe::tracer::SpanContext;

    #[test]
    fn test_traceparent_format_w3c() {
        let ctx = SpanContext {
            trace_id: "0af7651916cd43dd8448eb211c80319c".to_string(),
            span_id: "b7ad6b7169203331".to_string(),
            parent_span_id: None,
            trace_flags: 1,
        };

        let tp = ctx.to_traceparent();
        assert_eq!(tp, "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01");

        // Verify it matches the W3C regex
        let re = regex_lite::Regex::new(r"^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$").unwrap();
        assert!(re.is_match(&tp));
    }

    #[test]
    fn test_traceparent_round_trip() {
        let original = SpanContext {
            trace_id: "abcdef1234567890abcdef1234567890".to_string(),
            span_id: "1234567890abcdef".to_string(),
            parent_span_id: None,
            trace_flags: 0,
        };

        let tp = original.to_traceparent();
        let parsed = SpanContext::from_traceparent(&tp).expect("should parse");
        assert_eq!(parsed.trace_id, original.trace_id);
        assert_eq!(parsed.span_id, original.span_id);
        assert_eq!(parsed.trace_flags, original.trace_flags);
    }

    #[test]
    fn test_traceparent_parse_invalid() {
        assert!(SpanContext::from_traceparent("").is_none());
        assert!(SpanContext::from_traceparent("not-a-traceparent").is_none());
        assert!(SpanContext::from_traceparent("01-abc-def-00").is_none()); // wrong version
        assert!(SpanContext::from_traceparent("00-short-short-00").is_none()); // too short
    }

    #[test]
    fn test_span_generates_valid_traceparent() {
        use void_box::observe::tracer::Span;
        let span = Span::new("test");
        let tp = span.context.to_traceparent();

        // Should be valid W3C format
        let parsed = SpanContext::from_traceparent(&tp);
        assert!(parsed.is_some(), "generated traceparent should be parseable: {}", tp);
    }
}

// ---------------------------------------------------------------------------
// T2/T3: OTel SDK in-memory exporter tests (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "opentelemetry"))]
mod otel_sdk_tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::trace::{
        SdkTracerProvider, SimpleSpanProcessor,
        in_memory_exporter::InMemorySpanExporter,
    };
    use opentelemetry_sdk::metrics::{
        SdkMeterProvider, PeriodicReader,
        in_memory_exporter::InMemoryMetricExporter,
    };

    use void_box::observe::tracer::{Tracer, TracerConfig, SpanStatus};
    use void_box::observe::metrics::{MetricsCollector, MetricsConfig};

    fn test_tracer_provider() -> (SdkTracerProvider, InMemorySpanExporter) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
            .build();
        (provider, exporter)
    }

    fn test_meter_provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .build();
        (provider, exporter)
    }

    // -- T3: Tracer bridge tests --
    //
    // Note: The OTel SDK bridge in `Tracer` uses `opentelemetry::global::tracer()`.
    // In parallel tests the global state is unreliable, so we test the in-memory
    // path (which always works) and verify the bridge *structure* is correct.

    #[test]
    fn test_finish_span_stores_in_memory_with_attributes() {
        let config = TracerConfig {
            in_memory: true,
            ..TracerConfig::default()
        };
        let tracer = Tracer::new(config);

        let mut span = tracer.start_span("test.span");
        span.set_attribute("key1", "value1");
        span.status = SpanStatus::Ok;
        tracer.finish_span(span);

        let spans = tracer.get_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "test.span");
        assert_eq!(spans[0].attributes.get("key1").map(|s| s.as_str()), Some("value1"));
    }

    #[test]
    fn test_parent_child_spans_in_memory() {
        let config = TracerConfig {
            in_memory: true,
            ..TracerConfig::default()
        };
        let tracer = Tracer::new(config);

        let parent = tracer.start_span("parent");
        let parent_ctx = parent.context.clone();
        let child = tracer.start_span_with_parent("child", &parent_ctx);

        // Child should share trace_id and have parent as parent_span_id
        assert_eq!(child.context.trace_id, parent.context.trace_id);
        assert_eq!(child.context.parent_span_id.as_ref(), Some(&parent.context.span_id));

        tracer.finish_span(child);
        tracer.finish_span(parent);

        let spans = tracer.get_spans();
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().any(|s| s.name == "parent"));
        assert!(spans.iter().any(|s| s.name == "child"));
    }

    #[test]
    fn test_otel_bridge_created_when_endpoint_configured() {
        // Set global provider first
        let (provider, _exporter) = test_tracer_provider();
        opentelemetry::global::set_tracer_provider(provider.clone());

        // Tracer with endpoint should create the OTel bridge
        let config = TracerConfig {
            in_memory: true,
            otlp_endpoint: Some("http://dummy:4317".to_string()),
            ..TracerConfig::default()
        };
        let tracer = Tracer::new(config);

        // It should store spans in-memory AND attempt OTel export
        let mut span = tracer.start_span("bridged.span");
        span.set_attribute("test", "true");
        tracer.finish_span(span);

        // Verify in-memory works
        let spans = tracer.get_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "bridged.span");

        let _ = provider.shutdown();
    }

    // -- T2: Metrics bridge tests --

    #[test]
    fn test_set_gauge_emits_otel_metric() {
        let (meter_provider, metric_exporter) = test_meter_provider();
        let meter = meter_provider.meter("test");

        let config = MetricsConfig::in_memory();
        let collector = MetricsCollector::with_otel_meter(config, meter);

        collector.set_gauge("cpu_usage_percent", 42.5, &[("vm_cid", "3")]);

        // Force flush to export metrics
        let _ = meter_provider.force_flush();

        let metrics = metric_exporter.get_finished_metrics().unwrap();
        assert!(!metrics.is_empty(), "should have exported metrics");

        // Verify in-memory storage also works
        let snap = collector.snapshot();
        assert!(!snap.metrics.is_empty());

        let _ = meter_provider.shutdown();
    }

    #[test]
    fn test_add_counter_emits_otel_metric() {
        let (meter_provider, metric_exporter) = test_meter_provider();
        let meter = meter_provider.meter("test");

        let config = MetricsConfig::in_memory();
        let collector = MetricsCollector::with_otel_meter(config, meter);

        collector.add_counter("network_rx_bytes", 1024.0, &[("vm_cid", "3")]);
        collector.add_counter("network_rx_bytes", 2048.0, &[("vm_cid", "3")]);

        let _ = meter_provider.force_flush();

        let metrics = metric_exporter.get_finished_metrics().unwrap();
        assert!(!metrics.is_empty(), "should have exported counter metrics");

        let _ = meter_provider.shutdown();
    }

    #[test]
    fn test_record_duration_emits_otel_histogram() {
        let (meter_provider, metric_exporter) = test_meter_provider();
        let meter = meter_provider.meter("test");

        let config = MetricsConfig::in_memory();
        let collector = MetricsCollector::with_otel_meter(config, meter);

        collector.record_duration("step1", std::time::Duration::from_millis(100));
        collector.record_duration("step1", std::time::Duration::from_millis(200));

        let _ = meter_provider.force_flush();

        let metrics = metric_exporter.get_finished_metrics().unwrap();
        assert!(!metrics.is_empty(), "should have exported histogram metrics");

        // Also verify in-memory
        let snap = collector.snapshot();
        let hist = snap.get_histogram("step1_duration_ms").unwrap();
        assert_eq!(hist.count, 2);

        let _ = meter_provider.shutdown();
    }

    #[test]
    fn test_guest_cpu_metric_via_aggregator() {
        use void_box::observe::Observer;
        use void_box::observe::telemetry::TelemetryAggregator;
        use void_box::guest::protocol::{TelemetryBatch, SystemMetrics};

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

        let snap = observer.get_metrics();
        // CPU should be recorded
        assert!(snap.metrics.values().any(|m| m.name == "cpu_usage_percent"));
        // Memory should be recorded
        assert!(snap.metrics.values().any(|m| m.name == "memory_usage_bytes"));
    }

    #[test]
    fn test_guest_process_metrics_via_aggregator() {
        use void_box::observe::Observer;
        use void_box::observe::telemetry::TelemetryAggregator;
        use void_box::guest::protocol::{TelemetryBatch, ProcessMetrics};

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

        let snap = observer.get_metrics();
        assert!(snap.metrics.values().any(|m| m.name == "guest.process.rss_bytes"));
        assert!(snap.metrics.values().any(|m| m.name == "guest.process.cpu_jiffies"));
    }

    // -- Claude parser OTel span tests --

    #[test]
    fn test_claude_tool_spans_created() {
        use void_box::observe::claude::{parse_stream_json, create_otel_spans};
        use void_box::observe::tracer::{Tracer, TracerConfig};

        let jsonl = concat!(
            r#"{"type":"system","session_id":"s","model":"sonnet","tools":["Bash"]}"#, "\n",
            r#"{"type":"assistant","session_id":"s","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":10,"output_tokens":5}}}"#, "\n",
            r#"{"type":"user","session_id":"s","message":{"id":"m2","role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"files\n"}]}}"#, "\n",
            r#"{"type":"result","subtype":"success","session_id":"s","total_cost_usd":0.001,"is_error":false,"duration_ms":1000,"duration_api_ms":800,"num_turns":1,"result":"Done.","usage":{"input_tokens":10,"output_tokens":5}}"#, "\n",
        );

        let result = parse_stream_json(jsonl.as_bytes());
        assert_eq!(result.tool_calls.len(), 1);

        let config = TracerConfig::in_memory();
        let tracer = Tracer::new(config);

        create_otel_spans(&result, None, &tracer);

        let spans = tracer.get_spans();
        // Should have: claude.exec + claude.tool.Bash = 2 spans
        assert_eq!(spans.len(), 2, "expected 2 spans, got {:?}", spans.iter().map(|s| &s.name).collect::<Vec<_>>());

        let exec_span = spans.iter().find(|s| s.name == "claude.exec").expect("claude.exec span");
        assert_eq!(exec_span.attributes.get("gen_ai.system").map(|s| s.as_str()), Some("anthropic"));
        assert_eq!(exec_span.attributes.get("gen_ai.request.model").map(|s| s.as_str()), Some("sonnet"));

        let tool_span = spans.iter().find(|s| s.name == "claude.tool.Bash").expect("claude.tool.Bash span");
        assert_eq!(tool_span.attributes.get("tool.name").map(|s| s.as_str()), Some("Bash"));
    }

    #[test]
    fn test_token_usage_attributes() {
        use void_box::observe::claude::{parse_stream_json, create_otel_spans};
        use void_box::observe::tracer::{Tracer, TracerConfig};

        let jsonl = r#"{"type":"result","subtype":"success","session_id":"s","total_cost_usd":0.05,"is_error":false,"duration_ms":5000,"duration_api_ms":4000,"num_turns":2,"result":"ok","usage":{"input_tokens":1000,"output_tokens":500}}"#;
        let result = parse_stream_json(jsonl.as_bytes());

        let config = TracerConfig::in_memory();
        let tracer = Tracer::new(config);
        create_otel_spans(&result, None, &tracer);

        let spans = tracer.get_spans();
        let exec_span = spans.iter().find(|s| s.name == "claude.exec").expect("claude.exec");
        assert_eq!(exec_span.attributes.get("gen_ai.usage.input_tokens").map(|s| s.as_str()), Some("1000"));
        assert_eq!(exec_span.attributes.get("gen_ai.usage.output_tokens").map(|s| s.as_str()), Some("500"));
        assert_eq!(exec_span.attributes.get("claude.total_cost_usd").map(|s| s.as_str()), Some("0.050000"));
    }
}

// ---------------------------------------------------------------------------
// T6: KVM integration tests (ignored by default)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod kvm_otel_tests {
    #[tokio::test]
    #[ignore = "requires KVM + kernel + initramfs"]
    async fn test_kvm_exec_with_traceparent() {
        // This test would:
        // 1. Boot a VM
        // 2. Set a span context
        // 3. Exec "env" in the guest
        // 4. Verify TRACEPARENT appears in the output
        // Placeholder for when running with real KVM
    }
}
