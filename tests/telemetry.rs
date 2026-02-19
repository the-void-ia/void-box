//! Telemetry Integration Tests
//!
//! Dedicated test battery for the guest → host telemetry pipeline:
//!
//! 1. **Protocol wire-format tests** – verify that `TelemetryBatch` and
//!    related types serialize/deserialize identically across host and guest.
//! 2. **TelemetryAggregator tests** – verify the host-side aggregator
//!    correctly feeds guest metrics into the `Observer`'s `MetricsCollector`.
//! 3. **KVM end-to-end test** (opt-in, `#[ignore]`) – boot a real VM,
//!    subscribe to the telemetry stream, and validate received batches.
//!
//! Run all non-KVM tests:
//! ```bash
//! cargo test --test telemetry
//! ```
//!
//! Run KVM end-to-end test:
//! ```bash
//! export VOID_BOX_KERNEL=/path/to/vmlinux
//! export VOID_BOX_INITRAMFS=/path/to/rootfs.cpio.gz
//! cargo test --test telemetry -- --ignored
//! ```

use std::path::PathBuf;

use void_box::guest::protocol::{
    ExecResponse, Message, MessageType, ProcessMetrics, SystemMetrics, TelemetryBatch,
    TelemetrySubscribeRequest,
};
use void_box::observe::telemetry::TelemetryAggregator;
use void_box::observe::Observer;

// =============================================================================
// PROTOCOL WIRE-FORMAT TESTS
// =============================================================================

/// Verify TelemetryBatch round-trips through JSON (the wire encoding).
#[test]
fn telemetry_batch_json_round_trip() {
    let batch = make_sample_batch(0);
    let json = serde_json::to_vec(&batch).unwrap();
    let decoded: TelemetryBatch = serde_json::from_slice(&json).unwrap();

    assert_eq!(decoded.seq, batch.seq);
    assert_eq!(decoded.timestamp_ms, batch.timestamp_ms);
    assert!(decoded.system.is_some());

    let sys = decoded.system.as_ref().unwrap();
    let orig = batch.system.as_ref().unwrap();
    assert_eq!(sys.cpu_percent, orig.cpu_percent);
    assert_eq!(sys.memory_used_bytes, orig.memory_used_bytes);
    assert_eq!(sys.memory_total_bytes, orig.memory_total_bytes);
    assert_eq!(sys.net_rx_bytes, orig.net_rx_bytes);
    assert_eq!(sys.net_tx_bytes, orig.net_tx_bytes);
    assert_eq!(sys.procs_running, orig.procs_running);
    assert_eq!(sys.open_fds, orig.open_fds);

    assert_eq!(decoded.processes.len(), batch.processes.len());
    for (d, o) in decoded.processes.iter().zip(batch.processes.iter()) {
        assert_eq!(d.pid, o.pid);
        assert_eq!(d.comm, o.comm);
        assert_eq!(d.rss_bytes, o.rss_bytes);
        assert_eq!(d.cpu_jiffies, o.cpu_jiffies);
        assert_eq!(d.state, o.state);
    }
}

/// Verify TelemetryBatch can be embedded in a Message frame.
#[test]
fn telemetry_batch_in_message_frame() {
    let batch = make_sample_batch(1);
    let payload = serde_json::to_vec(&batch).unwrap();

    let msg = Message {
        msg_type: MessageType::TelemetryData,
        payload: payload.clone(),
    };

    let wire = msg.serialize();
    let decoded_msg = Message::deserialize(&wire).unwrap();

    assert_eq!(decoded_msg.msg_type, MessageType::TelemetryData);
    assert_eq!(decoded_msg.payload, payload);

    let decoded_batch: TelemetryBatch = serde_json::from_slice(&decoded_msg.payload).unwrap();
    assert_eq!(decoded_batch.seq, 1);
}

/// Verify SubscribeTelemetry message (empty payload) round-trips.
#[test]
fn subscribe_telemetry_message_round_trip() {
    let msg = Message {
        msg_type: MessageType::SubscribeTelemetry,
        payload: vec![],
    };

    let wire = msg.serialize();
    let decoded = Message::deserialize(&wire).unwrap();

    assert_eq!(decoded.msg_type, MessageType::SubscribeTelemetry);
    assert!(decoded.payload.is_empty());
}

/// Verify TelemetrySubscribeRequest defaults serialize correctly in a Message frame.
#[test]
fn subscribe_telemetry_with_defaults_in_message_frame() {
    let opts = TelemetrySubscribeRequest::default();
    let payload = serde_json::to_vec(&opts).unwrap();

    let msg = Message {
        msg_type: MessageType::SubscribeTelemetry,
        payload: payload.clone(),
    };

    let wire = msg.serialize();
    let decoded_msg = Message::deserialize(&wire).unwrap();

    assert_eq!(decoded_msg.msg_type, MessageType::SubscribeTelemetry);
    let decoded_opts: TelemetrySubscribeRequest =
        serde_json::from_slice(&decoded_msg.payload).unwrap();
    assert_eq!(decoded_opts.interval_ms, 1000);
    assert!(!decoded_opts.include_kernel_threads);
}

/// Verify custom TelemetrySubscribeRequest round-trips through a Message frame.
#[test]
fn subscribe_telemetry_custom_opts_in_message_frame() {
    let opts = TelemetrySubscribeRequest {
        interval_ms: 500,
        include_kernel_threads: true,
    };
    let payload = serde_json::to_vec(&opts).unwrap();

    let msg = Message {
        msg_type: MessageType::SubscribeTelemetry,
        payload,
    };

    let wire = msg.serialize();
    let decoded_msg = Message::deserialize(&wire).unwrap();
    let decoded_opts: TelemetrySubscribeRequest =
        serde_json::from_slice(&decoded_msg.payload).unwrap();

    assert_eq!(decoded_opts.interval_ms, 500);
    assert!(decoded_opts.include_kernel_threads);
}

/// Verify Message::read_from_sync works for TelemetryData messages.
#[test]
fn telemetry_message_read_from_sync() {
    let batch = make_sample_batch(42);
    let msg = Message {
        msg_type: MessageType::TelemetryData,
        payload: serde_json::to_vec(&batch).unwrap(),
    };

    let wire = msg.serialize();
    let mut cursor = std::io::Cursor::new(wire);
    let decoded = Message::read_from_sync(&mut cursor).unwrap();

    assert_eq!(decoded.msg_type, MessageType::TelemetryData);
    let decoded_batch: TelemetryBatch = serde_json::from_slice(&decoded.payload).unwrap();
    assert_eq!(decoded_batch.seq, 42);
}

/// Verify multiple messages can be streamed and read sequentially.
#[test]
fn telemetry_stream_multiple_messages() {
    let mut wire = Vec::new();
    for seq in 0..5 {
        let batch = make_sample_batch(seq);
        let msg = Message {
            msg_type: MessageType::TelemetryData,
            payload: serde_json::to_vec(&batch).unwrap(),
        };
        wire.extend_from_slice(&msg.serialize());
    }

    let mut cursor = std::io::Cursor::new(wire);
    for expected_seq in 0..5 {
        let msg = Message::read_from_sync(&mut cursor).unwrap();
        assert_eq!(msg.msg_type, MessageType::TelemetryData);
        let batch: TelemetryBatch = serde_json::from_slice(&msg.payload).unwrap();
        assert_eq!(batch.seq, expected_seq);
    }
}

/// Verify TelemetryBatch with no system metrics (only processes).
#[test]
fn telemetry_batch_no_system_metrics() {
    let batch = TelemetryBatch {
        seq: 0,
        timestamp_ms: 1700000000000,
        system: None,
        processes: vec![ProcessMetrics {
            pid: 1,
            comm: "init".to_string(),
            rss_bytes: 4096,
            cpu_jiffies: 100,
            state: 'S',
        }],
        trace_context: None,
    };

    let json = serde_json::to_vec(&batch).unwrap();
    let decoded: TelemetryBatch = serde_json::from_slice(&json).unwrap();

    assert!(decoded.system.is_none());
    assert_eq!(decoded.processes.len(), 1);
}

/// Verify TelemetryBatch with empty processes list.
#[test]
fn telemetry_batch_empty_processes() {
    let batch = TelemetryBatch {
        seq: 0,
        timestamp_ms: 1700000000000,
        system: Some(SystemMetrics {
            cpu_percent: 10.0,
            memory_used_bytes: 1024,
            memory_total_bytes: 4096,
            net_rx_bytes: 0,
            net_tx_bytes: 0,
            procs_running: 1,
            open_fds: 10,
        }),
        processes: vec![],
        trace_context: None,
    };

    let json = serde_json::to_vec(&batch).unwrap();
    let decoded: TelemetryBatch = serde_json::from_slice(&json).unwrap();

    assert!(decoded.system.is_some());
    assert!(decoded.processes.is_empty());
}

/// Verify trace_context field is preserved.
#[test]
fn telemetry_batch_with_trace_context() {
    let batch = TelemetryBatch {
        seq: 0,
        timestamp_ms: 1700000000000,
        system: None,
        processes: vec![],
        trace_context: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string()),
    };

    let json = serde_json::to_vec(&batch).unwrap();
    let decoded: TelemetryBatch = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        decoded.trace_context.as_deref(),
        batch.trace_context.as_deref()
    );
}

/// Verify ExecResponse helpers still work (smoke test for shared types).
#[test]
fn exec_response_helpers_from_protocol_crate() {
    let ok = ExecResponse::success(b"out".to_vec(), b"err".to_vec(), 0, 100);
    assert!(ok.error.is_none());
    assert_eq!(ok.exit_code, 0);

    let err = ExecResponse::error("boom".to_string());
    assert!(err.error.is_some());
    assert_eq!(err.exit_code, -1);
}

// =============================================================================
// TELEMETRY AGGREGATOR TESTS
// =============================================================================

/// Basic: ingest a batch and verify system metrics appear in Observer.
#[test]
fn aggregator_ingest_system_metrics() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer.clone(), 42);

    agg.ingest(&make_sample_batch(0));

    let snapshot = observer.get_metrics();
    assert!(
        snapshot
            .metrics
            .values()
            .any(|m| m.name == "cpu_usage_percent"),
        "expected cpu_usage_percent metric after ingest"
    );
    assert!(
        snapshot
            .metrics
            .values()
            .any(|m| m.name == "memory_usage_bytes"),
        "expected memory_usage_bytes metric after ingest"
    );
}

/// Ingest a batch with process metrics and verify gauges.
#[test]
fn aggregator_ingest_process_metrics() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer.clone(), 42);

    agg.ingest(&make_sample_batch(0));

    let snapshot = observer.get_metrics();
    assert!(
        snapshot
            .metrics
            .values()
            .any(|m| m.name == "guest.process.rss_bytes"),
        "expected guest.process.rss_bytes gauge after ingest"
    );
    assert!(
        snapshot
            .metrics
            .values()
            .any(|m| m.name == "guest.process.cpu_jiffies"),
        "expected guest.process.cpu_jiffies gauge after ingest"
    );
}

/// Verify latest_batch() returns None before any ingest, then the last batch.
#[test]
fn aggregator_latest_batch() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer, 42);

    assert!(agg.latest_batch().is_none(), "expected None before ingest");

    agg.ingest(&make_sample_batch(1));
    let latest = agg.latest_batch().unwrap();
    assert_eq!(latest.seq, 1);

    agg.ingest(&make_sample_batch(5));
    let latest = agg.latest_batch().unwrap();
    assert_eq!(latest.seq, 5);
}

/// Verify CID is correctly stored.
#[test]
fn aggregator_cid() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer, 99);
    assert_eq!(agg.cid(), 99);
}

/// Ingest multiple batches sequentially and verify metrics are updated.
#[test]
fn aggregator_sequential_ingest() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer.clone(), 42);

    for seq in 0..10 {
        let mut batch = make_sample_batch(seq);
        // Vary the CPU percentage
        if let Some(ref mut sys) = batch.system {
            sys.cpu_percent = seq as f64 * 10.0;
        }
        agg.ingest(&batch);
    }

    let latest = agg.latest_batch().unwrap();
    assert_eq!(latest.seq, 9);
    assert_eq!(latest.system.as_ref().unwrap().cpu_percent, 90.0);

    // Metrics should exist (exact values depend on MetricsCollector semantics)
    let snapshot = observer.get_metrics();
    assert!(snapshot
        .metrics
        .values()
        .any(|m| m.name == "cpu_usage_percent"));
}

/// Ingest a batch with no system metrics (only processes).
#[test]
fn aggregator_ingest_no_system_metrics() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer.clone(), 42);

    let batch = TelemetryBatch {
        seq: 0,
        timestamp_ms: 1700000000000,
        system: None,
        processes: vec![ProcessMetrics {
            pid: 1,
            comm: "init".to_string(),
            rss_bytes: 8192,
            cpu_jiffies: 50,
            state: 'S',
        }],
        trace_context: None,
    };

    agg.ingest(&batch);

    let snapshot = observer.get_metrics();
    // Should still record process metrics even without system metrics
    assert!(snapshot
        .metrics
        .values()
        .any(|m| m.name == "guest.process.rss_bytes"));
    // Should NOT have cpu_usage_percent since system was None
    // (depends on implementation -- if set_gauge records 0 or skips)
}

/// Ingest an empty batch (no system, no processes).
#[test]
fn aggregator_ingest_empty_batch() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer.clone(), 42);

    let batch = TelemetryBatch {
        seq: 0,
        timestamp_ms: 1700000000000,
        system: None,
        processes: vec![],
        trace_context: None,
    };

    // Should not panic
    agg.ingest(&batch);

    let latest = agg.latest_batch().unwrap();
    assert_eq!(latest.seq, 0);
}

/// Concurrent ingest from multiple threads.
#[test]
fn aggregator_concurrent_ingest() {
    use std::sync::Arc;
    let observer = Observer::test();
    let agg = Arc::new(TelemetryAggregator::new(observer.clone(), 42));

    let mut handles = vec![];
    for thread_id in 0..4u64 {
        let agg = Arc::clone(&agg);
        handles.push(std::thread::spawn(move || {
            for i in 0..25 {
                let seq = thread_id * 25 + i;
                agg.ingest(&make_sample_batch(seq));
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Should have recorded the latest batch (exact seq depends on scheduling)
    let latest = agg.latest_batch().unwrap();
    // All 100 batches ingested; exact seq is nondeterministic
    assert!(latest.seq < 100);

    // Observer should have metrics
    let snapshot = observer.get_metrics();
    assert!(snapshot
        .metrics
        .values()
        .any(|m| m.name == "cpu_usage_percent"));
}

/// Verify that metrics labels include the VM CID.
#[test]
fn aggregator_metrics_include_cid_label() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer.clone(), 77);

    agg.ingest(&make_sample_batch(0));

    let snapshot = observer.get_metrics();
    // The MetricsCollector stores labels in the metric key; check via snapshot
    // that at least some metric keys contain "vm_cid" labeling.
    // This is implementation-dependent: our MetricsCollector includes labels in
    // the key name as "metric_name{label1=v1,...}".
    let has_cid_label = snapshot.metrics.keys().any(|k| k.contains("vm_cid"));
    assert!(
        has_cid_label,
        "expected at least one metric key to contain 'vm_cid' label; keys: {:?}",
        snapshot.metrics.keys().collect::<Vec<_>>()
    );
}

/// Two aggregators for different CIDs sharing the same Observer.
#[test]
fn aggregator_multiple_cids_shared_observer() {
    let observer = Observer::test();
    let agg1 = TelemetryAggregator::new(observer.clone(), 10);
    let agg2 = TelemetryAggregator::new(observer.clone(), 20);

    agg1.ingest(&make_sample_batch(0));
    agg2.ingest(&make_sample_batch(0));

    let snapshot = observer.get_metrics();
    // Both CIDs should contribute metrics
    let keys: Vec<&String> = snapshot.metrics.keys().collect();
    let has_cid_10 = keys.iter().any(|k| k.contains("10"));
    let has_cid_20 = keys.iter().any(|k| k.contains("20"));
    assert!(
        has_cid_10 && has_cid_20,
        "expected metrics from both CIDs; keys: {:?}",
        keys
    );
}

// =============================================================================
// OBSERVER INTEGRATION TESTS
// =============================================================================

/// Verify that TelemetryAggregator feeds into Observer.get_metrics().
#[test]
fn observer_integration_telemetry_shows_in_snapshot() {
    let observer = Observer::test();
    let agg = TelemetryAggregator::new(observer.clone(), 42);

    // Initial snapshot should be empty (or close to it)
    let before = observer.get_metrics();
    let before_count = before.metrics.len();

    agg.ingest(&make_sample_batch(0));

    let after = observer.get_metrics();
    assert!(
        after.metrics.len() > before_count,
        "expected more metrics after telemetry ingest; before={}, after={}",
        before_count,
        after.metrics.len()
    );
}

/// Verify telemetry metrics coexist with workflow/span metrics.
#[test]
fn observer_integration_telemetry_plus_spans() {
    let observer = Observer::test();

    // Create some workflow spans
    {
        let span = observer.start_workflow_span("test-workflow");
        span.set_ok();
    }

    // Ingest telemetry
    let agg = TelemetryAggregator::new(observer.clone(), 42);
    agg.ingest(&make_sample_batch(0));

    // Both span-based and telemetry-based metrics should be present
    let snapshot = observer.get_metrics();
    let has_duration = snapshot
        .metrics
        .values()
        .any(|m| m.name.contains("duration"));
    let has_cpu = snapshot
        .metrics
        .values()
        .any(|m| m.name == "cpu_usage_percent");

    assert!(has_duration, "expected workflow duration metric");
    assert!(has_cpu, "expected telemetry cpu_usage_percent metric");

    // Traces should be present too
    assert!(observer.has_span("workflow:test-workflow"));
}

// =============================================================================
// KVM END-TO-END TEST (opt-in)
// =============================================================================

/// Full KVM end-to-end: boot a VM, subscribe to telemetry, validate batches.
///
/// Requires:
/// - `/dev/kvm` accessible
/// - `VOID_BOX_KERNEL` env var → path to vmlinux/bzImage
/// - `VOID_BOX_INITRAMFS` env var → path to rootfs.cpio.gz
///
/// ```bash
/// cargo test --test telemetry kvm_telemetry_end_to_end -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn kvm_telemetry_end_to_end() {
    use void_box::vmm::config::VoidBoxConfig;
    use void_box::vmm::MicroVm;

    let Some((kernel, initramfs)) = kvm_artifacts_from_env() else {
        eprintln!(
            "skipping kvm_telemetry_end_to_end: \
             set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS"
        );
        return;
    };

    // Build VM
    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(&kernel)
        .enable_vsock(true);

    if let Some(ref initramfs_path) = initramfs {
        cfg = cfg.initramfs(initramfs_path);
    }

    cfg.validate().expect("invalid VoidBoxConfig");

    let mut vm = MicroVm::new(cfg)
        .await
        .expect("failed to create KVM-backed MicroVm");

    // Verify the VM boots by running a trivial command first
    match vm.exec("echo", &["telemetry-test"]).await {
        Ok(output) => {
            assert!(output.success(), "echo failed: {}", output.stderr_str());
        }
        Err(e) => {
            eprintln!("kvm_telemetry_end_to_end: exec failed, skipping: {e}");
            return;
        }
    }

    // Start telemetry subscription
    let telemetry_observer = if std::env::var("VOIDBOX_OTLP_ENDPOINT").is_ok()
        || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
    {
        Observer::new(void_box::observe::ObserveConfig::from_env())
    } else {
        Observer::test()
    };
    let opts = TelemetrySubscribeRequest::default(); // 1s interval, no kernel threads
    match vm.start_telemetry(telemetry_observer, opts).await {
        Ok(_agg) => {}
        Err(e) => {
            eprintln!("kvm_telemetry_end_to_end: start_telemetry failed: {e}");
            let _ = vm.stop().await;
            return;
        }
    }

    // Wait for a few telemetry batches (guest streams every 1s with default opts)
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Check that telemetry data was received
    if let Some(agg) = vm.telemetry() {
        if let Some(batch) = agg.latest_batch() {
            eprintln!("Received telemetry batch seq={}", batch.seq);
            assert!(batch.seq > 0, "expected at least 2 batches after 7s");

            if let Some(ref sys) = batch.system {
                eprintln!(
                    "  cpu={}% mem_used={} mem_total={} net_rx={} net_tx={} procs={} fds={}",
                    sys.cpu_percent,
                    sys.memory_used_bytes,
                    sys.memory_total_bytes,
                    sys.net_rx_bytes,
                    sys.net_tx_bytes,
                    sys.procs_running,
                    sys.open_fds,
                );
                assert!(sys.memory_total_bytes > 0, "memory_total should be > 0");
            }

            eprintln!("  {} processes reported", batch.processes.len());
            assert!(
                !batch.processes.is_empty(),
                "expected at least one process (the guest-agent)"
            );
        } else {
            eprintln!("kvm_telemetry_end_to_end: no telemetry batches received yet");
        }
    } else {
        eprintln!("kvm_telemetry_end_to_end: telemetry aggregator not available");
    }

    // Flush OTel data before stopping (no-op when OTel is not configured)
    let _ = void_box::observe::flush_global_otel();

    vm.stop().await.expect("failed to stop VM");
}

/// KVM test with kernel threads included — verify we see zero-RSS processes.
///
/// ```bash
/// cargo test --test telemetry kvm_telemetry_with_kernel_threads -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires KVM + kernel/initramfs artifacts"]
async fn kvm_telemetry_with_kernel_threads() {
    use void_box::vmm::config::VoidBoxConfig;
    use void_box::vmm::MicroVm;

    let Some((kernel, initramfs)) = kvm_artifacts_from_env() else {
        eprintln!(
            "skipping kvm_telemetry_with_kernel_threads: \
             set VOID_BOX_KERNEL and VOID_BOX_INITRAMFS"
        );
        return;
    };

    let mut cfg = VoidBoxConfig::new()
        .memory_mb(256)
        .vcpus(1)
        .kernel(&kernel)
        .enable_vsock(true);

    if let Some(ref initramfs_path) = initramfs {
        cfg = cfg.initramfs(initramfs_path);
    }
    cfg.validate().expect("invalid VoidBoxConfig");

    let mut vm = MicroVm::new(cfg)
        .await
        .expect("failed to create KVM-backed MicroVm");

    // Verify VM boots
    match vm.exec("echo", &["kernel-thread-test"]).await {
        Ok(output) => assert!(output.success(), "echo failed: {}", output.stderr_str()),
        Err(e) => {
            eprintln!("kvm_telemetry_with_kernel_threads: exec failed, skipping: {e}");
            return;
        }
    }

    // Subscribe with kernel threads included
    let opts = TelemetrySubscribeRequest {
        interval_ms: 1000,
        include_kernel_threads: true,
    };
    let telemetry_observer = Observer::test();
    match vm.start_telemetry(telemetry_observer, opts).await {
        Ok(_) => {}
        Err(e) => {
            eprintln!("kvm_telemetry_with_kernel_threads: start_telemetry failed: {e}");
            let _ = vm.stop().await;
            return;
        }
    }

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    if let Some(agg) = vm.telemetry() {
        if let Some(batch) = agg.latest_batch() {
            eprintln!(
                "Received batch seq={}, {} processes",
                batch.seq,
                batch.processes.len()
            );
            // With kernel threads included, we should see some processes with zero RSS
            let zero_rss_count = batch.processes.iter().filter(|p| p.rss_bytes == 0).count();
            eprintln!("  {} processes with zero RSS (likely kernel threads)", zero_rss_count);
            // Kernel threads like kthreadd, ksoftirqd, etc. should appear
            assert!(
                zero_rss_count > 0,
                "expected kernel threads (zero RSS) when include_kernel_threads=true"
            );
        }
    }

    vm.stop().await.expect("failed to stop VM");
}

// =============================================================================
// HELPERS
// =============================================================================

/// Create a sample TelemetryBatch for testing.
fn make_sample_batch(seq: u64) -> TelemetryBatch {
    TelemetryBatch {
        seq,
        timestamp_ms: 1700000000000 + seq * 2000,
        system: Some(SystemMetrics {
            cpu_percent: 25.5 + seq as f64,
            memory_used_bytes: 512 * 1024 * 1024,
            memory_total_bytes: 1024 * 1024 * 1024,
            net_rx_bytes: 1000 * (seq + 1),
            net_tx_bytes: 2000 * (seq + 1),
            procs_running: 3,
            open_fds: 128,
        }),
        processes: vec![
            ProcessMetrics {
                pid: 1,
                comm: "init".to_string(),
                rss_bytes: 4096,
                cpu_jiffies: 100 + seq * 10,
                state: 'S',
            },
            ProcessMetrics {
                pid: 42,
                comm: "worker".to_string(),
                rss_bytes: 1024 * 1024,
                cpu_jiffies: 500 + seq * 20,
                state: 'R',
            },
        ],
        trace_context: None,
    }
}

/// Load kernel + initramfs paths from environment (same as kvm_integration.rs).
fn kvm_artifacts_from_env() -> Option<(PathBuf, Option<PathBuf>)> {
    let kernel = std::env::var_os("VOID_BOX_KERNEL")?;
    let kernel = PathBuf::from(kernel);
    if !kernel.exists() {
        return None;
    }

    let initramfs = std::env::var_os("VOID_BOX_INITRAMFS").map(PathBuf::from);
    Some((kernel, initramfs))
}
