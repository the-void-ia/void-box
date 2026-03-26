use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::error::ApiError;
use crate::persistence::{
    generate_event_id, legacy_to_v2_event_type, now_ms, now_rfc3339, provider_from_env,
    PersistenceProvider, RunEvent, RunState, RunStatus, SessionMessage,
};
use crate::spec::{RunKind, RunSpec};

#[derive(Clone)]
struct AppState {
    runs: Arc<Mutex<HashMap<String, RunState>>>,
    provider: Arc<dyn PersistenceProvider>,
    telemetry_buffers: Arc<
        Mutex<
            HashMap<
                String,
                std::sync::Arc<std::sync::Mutex<crate::observe::telemetry::TelemetryRingBuffer>>,
            >,
        >,
    >,
    sidecar_handles: Arc<Mutex<HashMap<String, crate::sidecar::SidecarHandle>>>,
}

#[derive(Debug, Deserialize)]
struct CreateRunRequest {
    file: String,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    policy: Option<crate::persistence::RunPolicy>,
    /// Snapshot path or hash-prefix to restore from (explicit opt-in).
    #[serde(default)]
    pub snapshot: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateRunResponse {
    run_id: String,
    attempt_id: u64,
    state: RunStatus,
}

#[derive(Debug, Deserialize)]
struct CancelRunRequest {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct CancelRunResponse {
    run_id: String,
    state: RunStatus,
    terminal_event_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ListRunsResponse {
    runs: Vec<RunState>,
}

#[derive(Debug, Serialize)]
struct Health {
    status: &'static str,
    persistence: &'static str,
}

#[derive(Debug, Deserialize)]
struct AppendMessageRequest {
    role: String,
    content: String,
}

pub async fn serve(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let provider = provider_from_env();
    let initial_runs = provider.load_runs().unwrap_or_default();

    let state = AppState {
        runs: Arc::new(Mutex::new(initial_runs)),
        provider,
        telemetry_buffers: Arc::new(Mutex::new(HashMap::new())),
        sidecar_handles: Arc::new(Mutex::new(HashMap::new())),
    };

    let listener = TcpListener::bind(addr).await?;
    println!("[void-box] daemon listening on http://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_stream(stream, state).await {
                eprintln!("[void-box] daemon connection error: {e}");
            }
        });
    }
}

async fn handle_stream(
    mut stream: TcpStream,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = vec![0u8; 64 * 1024];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let req = String::from_utf8_lossy(&buf[..n]).to_string();
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("");

    let (path, query) = raw_path
        .split_once('?')
        .map_or((raw_path, None), |(p, q)| (p, Some(q)));

    let body = if let Some(idx) = req.find("\r\n\r\n") {
        &req[idx + 4..]
    } else {
        ""
    };

    let (status, content_type, payload) = route_request(method, path, query, body, state).await;
    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        payload.len(),
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

fn parse_query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            Some(v.to_string())
        } else {
            None
        }
    })
}

/// Wraps a JSON `(status, body)` tuple into `(status, content_type, bytes)`.
fn as_json((status, body): (String, String)) -> (String, String, Vec<u8>) {
    (status, "application/json".to_string(), body.into_bytes())
}

async fn route_request(
    method: &str,
    path: &str,
    query: Option<&str>,
    body: &str,
    state: AppState,
) -> (String, String, Vec<u8>) {
    match (method, path) {
        ("GET", "/v1/health") => as_json((
            "200 OK".to_string(),
            serde_json::to_string(&Health {
                status: "ok",
                persistence: state.provider.name(),
            })
            .unwrap_or_else(|_| "{}".into()),
        )),
        ("POST", "/v1/runs") => as_json(create_run(body, state).await),
        ("GET", "/v1/runs") => as_json(list_runs(query, state).await),
        _ => {
            if let Some(id) = path.strip_prefix("/v1/runs/") {
                // /v1/runs/{run_id}/stages/{stage_name}/artifacts/{artifact_name}
                if let Some((rest, artifact_name)) = id.rsplit_once("/artifacts/") {
                    if let Some((run_id, stage_name)) = rest.rsplit_once("/stages/") {
                        if method == "GET" {
                            return get_named_artifact(run_id, stage_name, artifact_name, state)
                                .await;
                        }
                    }
                }
                // /v1/runs/{run_id}/stages/{stage_name}/output-file
                if let Some(rest) = id.strip_suffix("/output-file") {
                    if let Some((run_id, stage_name)) = rest.rsplit_once("/stages/") {
                        if method == "GET" {
                            return get_stage_output_file(run_id, stage_name, query, state).await;
                        }
                    }
                }
                if let Some(id) = id.strip_suffix("/stages") {
                    if method == "GET" {
                        return as_json(get_stages(id, state).await);
                    }
                }
                if let Some(id) = id.strip_suffix("/telemetry") {
                    if method == "GET" {
                        return as_json(get_telemetry(id, query, state).await);
                    }
                }
                if let Some(id) = id.strip_suffix("/events") {
                    return as_json(get_events(id, query, state).await);
                }
                if let Some(id) = id.strip_suffix("/cancel") {
                    if method == "POST" {
                        return as_json(cancel_run(id, body, state).await);
                    }
                }
                // PUT /v1/runs/{id}/inbox
                if let Some(run_id) = id.strip_suffix("/inbox") {
                    if method == "PUT" {
                        return as_json(push_inbox(run_id, body, state).await);
                    }
                }
                // GET /v1/runs/{id}/intents
                if let Some(run_id) = id.strip_suffix("/intents") {
                    if method == "GET" {
                        return as_json(drain_intents(run_id, state).await);
                    }
                }
                // POST /v1/runs/{id}/messages
                if let Some(run_id) = id.strip_suffix("/messages") {
                    if method == "POST" {
                        return as_json(push_message(run_id, body, state).await);
                    }
                }
                if method == "GET" {
                    return as_json(get_run(id, query, state).await);
                }
            }

            if let Some(id) = path.strip_prefix("/v1/sessions/") {
                if let Some(id) = id.strip_suffix("/messages") {
                    if method == "GET" {
                        return as_json(get_session_messages(id, state).await);
                    }
                    if method == "POST" {
                        return as_json(append_session_message(id, body, state).await);
                    }
                }
            }

            as_json((
                "404 Not Found".to_string(),
                ApiError::not_found("route not found").to_json(),
            ))
        }
    }
}

/// Reason for publication failure, used to wire into run failure.
#[derive(Debug)]
enum PublicationFailureReason {
    StructuredOutputMissing(String),
    StructuredOutputMalformed(String),
}

fn build_artifact_publication(
    run_id: &str,
    provider: &Arc<dyn PersistenceProvider>,
    events: &[RunEvent],
    report: Option<&crate::runtime::RunReport>,
) -> (
    crate::persistence::ArtifactPublication,
    Option<PublicationFailureReason>,
) {
    use crate::persistence::{
        ArtifactManifestEntry, ArtifactPublication, ArtifactPublicationStatus,
    };

    // Collect stages that reached a terminal state (completed, failed, skipped)
    // and separately track which ones completed successfully.
    let mut completed_stages: Vec<String> = Vec::new();
    let mut any_stage_ran = false;
    for ev in events {
        if let Some(ref sn) = ev.stage_name {
            match ev.event_type.as_str() {
                "stage.completed" => {
                    if !completed_stages.contains(sn) {
                        completed_stages.push(sn.clone());
                    }
                    any_stage_ran = true;
                }
                "stage.started" | "stage.failed" | "stage.skipped" => {
                    any_stage_ran = true;
                }
                _ => {}
            }
        }
    }

    // No stages ran at all (e.g. spec load failure) — nothing to publish
    if !any_stage_ran {
        return (
            ArtifactPublication {
                status: ArtifactPublicationStatus::NotStarted,
                published_at: None,
                manifest: Vec::new(),
            },
            None,
        );
    }

    // If stages ran but none completed, output is missing only for runs that
    // are expected to publish structured output.
    if completed_stages.is_empty() {
        if !requires_structured_output(report) {
            return (
                ArtifactPublication {
                    status: ArtifactPublicationStatus::NotStarted,
                    published_at: None,
                    manifest: Vec::new(),
                },
                None,
            );
        }
        return (
            ArtifactPublication {
                status: ArtifactPublicationStatus::Failed,
                published_at: None,
                manifest: Vec::new(),
            },
            Some(PublicationFailureReason::StructuredOutputMissing(
                "no stages completed successfully".to_string(),
            )),
        );
    }

    let mut manifest = Vec::new();

    for stage in &completed_stages {
        let data = match provider.load_stage_artifact(run_id, stage) {
            Ok(Some(data)) => data,
            Ok(None) => {
                if !requires_structured_output(report) {
                    continue;
                }
                // Stage completed but no artifact — structured output missing
                return (
                    ArtifactPublication {
                        status: ArtifactPublicationStatus::Failed,
                        published_at: None,
                        manifest: Vec::new(),
                    },
                    Some(PublicationFailureReason::StructuredOutputMissing(format!(
                        "stage '{}' completed without result.json",
                        stage
                    ))),
                );
            }
            Err(_) => {
                if !requires_structured_output(report) {
                    continue;
                }
                return (
                    ArtifactPublication {
                        status: ArtifactPublicationStatus::Failed,
                        published_at: None,
                        manifest: Vec::new(),
                    },
                    Some(PublicationFailureReason::StructuredOutputMissing(format!(
                        "failed to load artifact for stage '{}'",
                        stage
                    ))),
                );
            }
        };

        let parsed = match serde_json::from_slice::<serde_json::Value>(&data) {
            Ok(v) => v,
            Err(_) => {
                if !requires_structured_output(report) {
                    // Non-orchestration run with non-JSON output — skip validation
                    continue;
                }
                return (
                    ArtifactPublication {
                        status: ArtifactPublicationStatus::Failed,
                        published_at: None,
                        manifest: Vec::new(),
                    },
                    Some(PublicationFailureReason::StructuredOutputMalformed(
                        format!("result.json for stage '{}' is not valid JSON", stage),
                    )),
                );
            }
        };

        if parsed.get("status").is_none() {
            if !requires_structured_output(report) {
                // Non-orchestration run with JSON missing status — skip validation
                continue;
            }
            return (
                ArtifactPublication {
                    status: ArtifactPublicationStatus::Failed,
                    published_at: None,
                    manifest: Vec::new(),
                },
                Some(PublicationFailureReason::StructuredOutputMalformed(
                    format!(
                        "result.json for stage '{}' is missing required 'status' field",
                        stage
                    ),
                )),
            );
        }

        manifest.push(ArtifactManifestEntry {
            name: "result.json".to_string(),
            stage: stage.clone(),
            media_type: "application/json".to_string(),
            size_bytes: Some(data.len() as u64),
            retrieval_path: format!("/v1/runs/{}/stages/{}/output-file", run_id, stage),
        });

        if let Some(artifacts) = parsed.get("artifacts").and_then(|a| a.as_array()) {
            for art in artifacts {
                let name = art.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                let media_type = art
                    .get("media_type")
                    .and_then(|m| m.as_str())
                    .unwrap_or("application/octet-stream");
                if !name.is_empty() {
                    let size = provider
                        .load_named_artifact(run_id, stage, name)
                        .ok()
                        .flatten()
                        .map(|d| d.len() as u64);
                    manifest.push(ArtifactManifestEntry {
                        name: name.to_string(),
                        stage: stage.clone(),
                        media_type: media_type.to_string(),
                        size_bytes: size,
                        retrieval_path: format!(
                            "/v1/runs/{}/stages/{}/artifacts/{}",
                            run_id, stage, name
                        ),
                    });
                }
            }
        }
    }

    (
        ArtifactPublication {
            status: if manifest.is_empty() {
                ArtifactPublicationStatus::NotStarted
            } else {
                ArtifactPublicationStatus::Published
            },
            published_at: if manifest.is_empty() {
                None
            } else {
                Some(now_rfc3339())
            },
            manifest,
        },
        None,
    )
}

fn requires_structured_output(report: Option<&crate::runtime::RunReport>) -> bool {
    match report {
        Some(report) if report.kind == "workflow" => report.stages <= 1,
        Some(report) if report.kind == "agent" || report.kind == "pipeline" => true,
        _ => false,
    }
}

fn build_stage_states(events: &[RunEvent]) -> HashMap<String, crate::persistence::StageState> {
    let mut states: HashMap<String, crate::persistence::StageState> = HashMap::new();
    for ev in events {
        let Some(ref sn) = ev.stage_name else {
            continue;
        };
        match ev.event_type.as_str() {
            "stage.started" => {
                states.insert(
                    sn.clone(),
                    crate::persistence::StageState {
                        status: "running".to_string(),
                        started_at: ev.timestamp.clone(),
                        completed_at: None,
                    },
                );
            }
            "stage.completed" => {
                let entry = states
                    .entry(sn.clone())
                    .or_insert(crate::persistence::StageState {
                        status: "succeeded".to_string(),
                        started_at: None,
                        completed_at: ev.timestamp.clone(),
                    });
                entry.status = "succeeded".to_string();
                entry.completed_at = ev.timestamp.clone();
            }
            "stage.failed" => {
                let entry = states
                    .entry(sn.clone())
                    .or_insert(crate::persistence::StageState {
                        status: "failed".to_string(),
                        started_at: None,
                        completed_at: ev.timestamp.clone(),
                    });
                entry.status = "failed".to_string();
                entry.completed_at = ev.timestamp.clone();
            }
            "stage.skipped" => {
                let entry = states
                    .entry(sn.clone())
                    .or_insert(crate::persistence::StageState {
                        status: "skipped".to_string(),
                        started_at: None,
                        completed_at: ev.timestamp.clone(),
                    });
                entry.status = "skipped".to_string();
                entry.completed_at = ev.timestamp.clone();
            }
            _ => {}
        }
    }
    states
}

async fn push_inbox(run_id: &str, body: &str, state: AppState) -> (String, String) {
    let snapshot: crate::sidecar::InboxSnapshot = match serde_json::from_str(body) {
        Ok(s) => s,
        Err(e) => {
            return (
                "400 Bad Request".into(),
                ApiError::invalid_params(e.to_string()).to_json(),
            )
        }
    };
    let handles = state.sidecar_handles.lock().await;
    match handles.get(run_id) {
        Some(handle) => {
            handle.load_inbox(snapshot).await;
            ("200 OK".into(), r#"{"ok":true}"#.into())
        }
        None => (
            "404 Not Found".into(),
            ApiError::not_found("no sidecar for run").to_json(),
        ),
    }
}

async fn drain_intents(run_id: &str, state: AppState) -> (String, String) {
    let handles = state.sidecar_handles.lock().await;
    match handles.get(run_id) {
        Some(handle) => {
            let intents = handle.drain_intents().await;
            ("200 OK".into(), serde_json::to_string(&intents).unwrap())
        }
        None => (
            "404 Not Found".into(),
            ApiError::not_found("no sidecar for run").to_json(),
        ),
    }
}

async fn push_message(run_id: &str, body: &str, state: AppState) -> (String, String) {
    let entry: crate::sidecar::InboxEntry = match serde_json::from_str(body) {
        Ok(e) => e,
        Err(e) => {
            return (
                "400 Bad Request".into(),
                ApiError::invalid_params(e.to_string()).to_json(),
            )
        }
    };
    let handles = state.sidecar_handles.lock().await;
    match handles.get(run_id) {
        Some(handle) => {
            handle.push_message(entry).await;
            ("200 OK".into(), r#"{"ok":true}"#.into())
        }
        None => (
            "404 Not Found".into(),
            ApiError::not_found("no sidecar for run").to_json(),
        ),
    }
}

async fn create_run(body: &str, state: AppState) -> (String, String) {
    let req: CreateRunRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                "400 Bad Request".to_string(),
                ApiError::invalid_spec(format!("invalid JSON: {e}")).to_json(),
            )
        }
    };

    // Validate policy if provided
    if let Some(ref policy) = req.policy {
        if policy.max_parallel_microvms_per_run < 1 {
            return (
                "400 Bad Request".to_string(),
                ApiError::invalid_policy("max_parallel_microvms_per_run must be >= 1").to_json(),
            );
        }
        if policy.stage_timeout_secs == 0 {
            return (
                "400 Bad Request".to_string(),
                ApiError::invalid_policy("stage_timeout_secs must be > 0").to_json(),
            );
        }
    }

    let caller_run_id = req.run_id.clone();
    let run_id = caller_run_id
        .clone()
        .unwrap_or_else(|| format!("run-{}", now_ms()));
    let environment_id = format!("env-{}", run_id);

    // Load and prepare the spec once. The loaded spec (if successful) is passed
    // into the background task — no double-loading, no leaked channels.
    let loaded_spec = crate::spec::load_spec(PathBuf::from(&req.file).as_path());
    let mut planned_events = Vec::new();
    let mut messaging_enabled = false;
    match &loaded_spec {
        Ok(spec) => {
            messaging_enabled = spec
                .agent
                .as_ref()
                .and_then(|a| a.messaging.as_ref())
                .is_some_and(|m| m.enabled);
            planned_events = plan_events_from_spec(&run_id, &environment_id, spec);
        }
        Err(e) => planned_events.push(event(
            &run_id,
            "warn",
            "spec.parse_failed",
            format!("failed to parse spec before run: {e}"),
        )),
    }

    let now = now_rfc3339();

    // Atomic idempotency: acquire lock ONCE and check + insert within the same scope.
    let should_spawn = {
        let mut runs = state.runs.lock().await;

        // Idempotency check for caller-supplied run_id
        if caller_run_id.is_some() {
            if let Some(existing) = runs.get(&run_id) {
                if existing.status.is_active() {
                    // Active run exists → return current state, don't spawn
                    return (
                        "200 OK".to_string(),
                        serde_json::to_string(&CreateRunResponse {
                            run_id: existing.id.clone(),
                            attempt_id: existing.attempt_id,
                            state: existing.status.clone(),
                        })
                        .unwrap_or_else(|_| "{}".into()),
                    );
                } else {
                    // Terminal run → error
                    return (
                        "409 Conflict".to_string(),
                        ApiError::already_terminal(format!(
                            "run '{}' is already in terminal state '{:?}'",
                            run_id, existing.status
                        ))
                        .to_json(),
                    );
                }
            }
        }

        let mut events = vec![event(
            &run_id,
            "info",
            "run.started",
            format!("run created for {}", req.file),
        )];
        events.push(event_with_env(
            &run_id,
            "info",
            "env.provisioned",
            "environment provisioning initialized".to_string(),
            &environment_id,
            None,
        ));
        events.extend(planned_events);
        // Assign seq to all events
        for (i, ev) in events.iter_mut().enumerate() {
            ev.seq = Some(i as u64);
        }

        let run = RunState {
            id: run_id.clone(),
            status: RunStatus::Running,
            file: req.file.clone(),
            report: None,
            error: None,
            events,
            attempt_id: 1,
            started_at: Some(now.clone()),
            updated_at: Some(now),
            terminal_reason: None,
            exit_code: None,
            active_stage_count: 0,
            active_microvm_count: 0,
            policy: req.policy.clone(),
            terminal_event_id: None,
            finished_at: None,
            stage_states: None,
            artifact_publication: None,
        };

        let _ = state.provider.save_run(&run);
        runs.insert(run_id.clone(), run);
        true
    };

    if !should_spawn {
        return (
            "500 Internal Server Error".to_string(),
            ApiError::internal("unexpected state").to_json(),
        );
    }

    // Create telemetry ring buffer for this run
    let buffer_size = std::env::var("VOIDBOX_TELEMETRY_BUFFER_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000usize);
    let ring_buffer = std::sync::Arc::new(std::sync::Mutex::new(
        crate::observe::telemetry::TelemetryRingBuffer::new(buffer_size),
    ));
    {
        let mut bufs = state.telemetry_buffers.lock().await;
        bufs.insert(run_id.clone(), ring_buffer.clone());
    }

    // Start sidecar if messaging is enabled
    if messaging_enabled {
        let sidecar_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        match crate::sidecar::start_sidecar(&run_id, &run_id, &run_id, vec![], sidecar_addr).await {
            Ok(handle) => {
                state
                    .sidecar_handles
                    .lock()
                    .await
                    .insert(run_id.clone(), handle);
            }
            Err(e) => {
                eprintln!("warning: failed to start sidecar for {}: {}", run_id, e);
            }
        }
    }

    // Spawn host metrics collection task (1s interval)
    let host_state = state.clone();
    let host_run_id = run_id.clone();
    let host_rb = ring_buffer.clone();
    tokio::spawn(async move {
        let collector = crate::observe::host_metrics::HostMetricsCollector::new();
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

            // Check if run is terminal
            let is_terminal = {
                let runs = host_state.runs.lock().await;
                runs.get(&host_run_id)
                    .map(|r| r.status.is_terminal())
                    .unwrap_or(true)
            };
            if is_terminal {
                break;
            }

            // Determine current stage name from latest StageStarted event
            let current_stage = {
                let runs = host_state.runs.lock().await;
                runs.get(&host_run_id)
                    .and_then(|r| {
                        r.events
                            .iter()
                            .rev()
                            .find(|e| e.event_type == "stage.started")
                            .and_then(|e| e.stage_name.clone())
                    })
                    .unwrap_or_default()
            };

            let snap = collector.collect();
            if let Ok(mut buf) = host_rb.lock() {
                buf.push(crate::observe::telemetry::TelemetrySample {
                    seq: 0, // assigned by push()
                    timestamp_ms: now_ms(),
                    timestamp: Some(now_rfc3339()),
                    stage_name: current_stage,
                    guest: None,
                    host: Some(crate::observe::telemetry::HostMetricsSample {
                        rss_bytes: snap.rss_bytes,
                        cpu_percent: snap.cpu_percent,
                        io_read_bytes: snap.io_read_bytes,
                        io_write_bytes: snap.io_write_bytes,
                    }),
                });
            }
        }
    });

    // Prepare the spec before spawning the background task: apply overrides,
    // inject snapshot config, and add messaging skill if sidecar is active.
    // This keeps the background task simple — it just calls run_spec.
    let prepared_spec = match loaded_spec {
        Ok(mut spec) => {
            if req.snapshot.is_some() {
                spec.sandbox.snapshot = req.snapshot.clone();
            }
            crate::runtime::apply_llm_overrides_from_env(&mut spec);

            if messaging_enabled {
                let sidecar_port = state
                    .sidecar_handles
                    .lock()
                    .await
                    .get(&run_id)
                    .map(|h| h.addr().port());
                if let Some(port) = sidecar_port {
                    // Inject sidecar URL as env var for void-message CLI
                    spec.sandbox.env.insert(
                        "VOID_SIDECAR_URL".to_string(),
                        format!("http://10.0.2.2:{}", port),
                    );
                    // Inject messaging skill (documents the CLI, not raw HTTP)
                    if let Some(ref mut agent) = spec.agent {
                        agent.skills.push(crate::spec::SkillEntry::Inline {
                            name: "void-messaging".into(),
                            content: crate::sidecar::messaging_skill_content(),
                        });
                    }
                    // If agent uses claude-code, also register void-mcp as MCP server
                    let is_claude = spec.agent.as_ref().is_some_and(|a| {
                        a.skills.iter().any(|s| {
                            matches!(s, crate::spec::SkillEntry::Simple(raw) if raw == "agent:claude-code" || raw == "agent:claude")
                        })
                    });
                    if is_claude {
                        if let Some(ref mut agent) = spec.agent {
                            let mut mcp_env = std::collections::HashMap::new();
                            mcp_env.insert(
                                "VOID_SIDECAR_URL".to_string(),
                                format!("http://10.0.2.2:{}", port),
                            );
                            agent.skills.push(crate::spec::SkillEntry::Mcp {
                                command: "void-mcp".to_string(),
                                args: vec![],
                                env: mcp_env,
                            });
                        }
                    }
                }
            }

            Ok(spec)
        }
        Err(e) => Err(e),
    };

    let state_bg = state.clone();
    let run_id_bg = run_id.clone();
    let policy_bg = req.policy.clone();
    let telemetry_rb = ring_buffer.clone();
    let provider_bg = state.provider.clone();
    tokio::spawn(async move {
        // Stage event channel: execution code sends RunEvents through this sender,
        // and the collector task pushes them into RunState.
        let (stage_tx, mut stage_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::persistence::RunEvent>();

        // Collector task: drains events from the channel into RunState.
        let collector_state = state_bg.clone();
        let collector_run_id = run_id_bg.clone();
        let collector_handle = tokio::spawn(async move {
            while let Some(mut ev) = stage_rx.recv().await {
                let mut runs = collector_state.runs.lock().await;
                if let Some(r) = runs.get_mut(&collector_run_id) {
                    // Terminal guard: skip if run already terminal
                    if r.status.is_terminal() {
                        continue;
                    }
                    ev.seq = Some(r.events.len() as u64);
                    ev.attempt_id = Some(r.attempt_id);
                    ev.run_id = Some(collector_run_id.clone());
                    r.events.push(ev);
                    r.updated_at = Some(now_rfc3339());
                    let _ = collector_state.provider.save_run(r);
                }
            }
        });

        // run_spec always consumes stage_tx (even on failure), so the
        // collector task will exit cleanly when the channel closes.
        let result = match prepared_spec {
            Ok(spec) => {
                crate::runtime::run_spec(
                    &spec,
                    req.input,
                    policy_bg,
                    Some(stage_tx),
                    Some(telemetry_rb),
                    Some(provider_bg),
                    Some(&run_id_bg),
                )
                .await
            }
            Err(e) => {
                // Spec failed to load — run_spec is not called, so we must
                // drop stage_tx explicitly to close the channel.
                drop(stage_tx);
                Err(e)
            }
        };

        // Wait for collector to drain remaining events
        let _ = collector_handle.await;

        let mut runs = state_bg.runs.lock().await;
        if let Some(r) = runs.get_mut(&run_id_bg) {
            // Terminal guard: if the run was already cancelled (or otherwise
            // reached a terminal state) while we were executing, do NOT
            // overwrite the status or append completion events.
            if r.status.is_terminal() {
                return;
            }

            let attempt = r.attempt_id;
            match result {
                Ok(report) => {
                    if report.success {
                        let finished_event = event_with_seq(
                            &run_id_bg,
                            "info",
                            "run.finished",
                            "run completed".to_string(),
                            r.events.len() as u64,
                            attempt,
                        );
                        r.terminal_event_id = finished_event.event_id.clone();
                        r.status = RunStatus::Succeeded;
                        r.events.push(finished_event);
                    } else {
                        let failed_event = event_with_seq(
                            &run_id_bg,
                            "error",
                            "run.failed",
                            "run failed: execution reported unsuccessful result".to_string(),
                            r.events.len() as u64,
                            attempt,
                        );
                        r.terminal_event_id = failed_event.event_id.clone();
                        r.status = RunStatus::Failed;
                        r.error = Some("execution reported unsuccessful result".to_string());
                        r.events.push(failed_event);
                    }
                    if !report.output.is_empty() {
                        for (i, line) in report.output.lines().enumerate() {
                            let seq = r.events.len() as u64;
                            let mut chunk = event_log_chunk(
                                &run_id_bg,
                                i as u64,
                                "stdout",
                                line.to_string(),
                                None,
                                None,
                            );
                            chunk.seq = Some(seq);
                            chunk.attempt_id = Some(attempt);
                            r.events.push(chunk);
                        }
                        let seq = r.events.len() as u64;
                        r.events.push(event_with_seq(
                            &run_id_bg,
                            "info",
                            "log.closed",
                            "stdout stream closed".to_string(),
                            seq,
                            attempt,
                        ));
                    }
                    r.report = Some(report);
                }
                Err(e) => {
                    let failed_event = event_with_seq(
                        &run_id_bg,
                        "error",
                        "run.failed",
                        format!("run failed: {e}"),
                        r.events.len() as u64,
                        attempt,
                    );
                    r.terminal_event_id = failed_event.event_id.clone();
                    r.status = RunStatus::Failed;
                    r.error = Some(e.to_string());
                    r.events.push(failed_event);
                }
            }
            let now_ts = now_rfc3339();
            r.finished_at = Some(now_ts.clone());
            r.stage_states = Some(build_stage_states(&r.events));
            let (publication, pub_failure) = build_artifact_publication(
                &run_id_bg,
                &state_bg.provider,
                &r.events,
                r.report.as_ref(),
            );
            r.artifact_publication = Some(publication);

            // Publication failure flips a successful run to failed
            if let Some(reason) = pub_failure {
                if r.status == RunStatus::Succeeded {
                    let (error_msg, event_msg) = match reason {
                        PublicationFailureReason::StructuredOutputMissing(ref msg) => (
                            msg.clone(),
                            format!("run failed: structured output missing: {msg}"),
                        ),
                        PublicationFailureReason::StructuredOutputMalformed(ref msg) => (
                            msg.clone(),
                            format!("run failed: structured output malformed: {msg}"),
                        ),
                    };
                    let failed_event = event_with_seq(
                        &run_id_bg,
                        "error",
                        "run.failed",
                        event_msg,
                        r.events.len() as u64,
                        attempt,
                    );
                    r.terminal_event_id = failed_event.event_id.clone();
                    r.status = RunStatus::Failed;
                    r.error = Some(error_msg);
                    r.events.push(failed_event);
                }
            }

            r.updated_at = Some(now_ts);
            let _ = state_bg.provider.save_run(r);
        }
        // Drop the runs lock before acquiring sidecar_handles lock
        drop(runs);

        // Clean up sidecar if one was started for this run
        if let Some(handle) = state_bg.sidecar_handles.lock().await.remove(&run_id_bg) {
            handle.stop().await;
        }
    });

    (
        "200 OK".to_string(),
        serde_json::to_string(&CreateRunResponse {
            run_id,
            attempt_id: 1,
            state: RunStatus::Running,
        })
        .unwrap_or_else(|_| "{}".into()),
    )
}

fn is_api_v2(query: Option<&str>) -> bool {
    parse_query_param(query, "api_version").as_deref() == Some("v2")
}

/// In v2 mode, replace `event_type` with the PascalCase `event_type_v2` value.
fn apply_v2_event_names(events: &[RunEvent]) -> Vec<RunEvent> {
    events
        .iter()
        .map(|e| {
            let mut ev = e.clone();
            if let Some(ref v2_name) = ev.event_type_v2 {
                ev.event_type = v2_name.clone();
            }
            ev
        })
        .collect()
}

async fn get_run(id: &str, query: Option<&str>, state: AppState) -> (String, String) {
    // Serialize the run while holding the runs lock, then drop it before
    // acquiring the sidecar_handles lock to avoid lock-order issues.
    let run_json: Option<serde_json::Value> = {
        let runs = state.runs.lock().await;
        if let Some(r) = runs.get(id) {
            let value = if is_api_v2(query) {
                let mut run = r.clone();
                run.events = apply_v2_event_names(&run.events);
                serde_json::to_value(&run).unwrap_or(serde_json::Value::Null)
            } else {
                serde_json::to_value(r).unwrap_or(serde_json::Value::Null)
            };
            Some(value)
        } else {
            None
        }
    };

    let Some(mut run_value) = run_json else {
        return (
            "404 Not Found".to_string(),
            ApiError::not_found(format!("run '{id}' not found")).to_json(),
        );
    };

    // Append sidecar health if a handle exists for this run.
    let sidecar_info: Option<serde_json::Value> = {
        let handles = state.sidecar_handles.lock().await;
        if let Some(handle) = handles.get(id) {
            let (buffer_depth, inbox_version) = handle.state_snapshot().await;
            Some(json!({
                "status": "ok",
                "buffer_depth": buffer_depth,
                "inbox_version": inbox_version
            }))
        } else {
            None
        }
    };

    if let Some(sidecar) = sidecar_info {
        if let serde_json::Value::Object(ref mut map) = run_value {
            map.insert("sidecar".to_string(), sidecar);
        }
    }

    (
        "200 OK".to_string(),
        serde_json::to_string(&run_value).unwrap_or_else(|_| "{}".into()),
    )
}

async fn get_events(id: &str, query: Option<&str>, state: AppState) -> (String, String) {
    let runs = state.runs.lock().await;
    if let Some(r) = runs.get(id) {
        let events = if let Some(from_id) = parse_query_param(query, "from_event_id") {
            // Find the position of the marker event and return only subsequent events.
            // If marker not found, return all events.
            if let Some(pos) = r
                .events
                .iter()
                .position(|e| e.event_id.as_deref() == Some(&from_id))
            {
                &r.events[pos + 1..]
            } else {
                &r.events[..]
            }
        } else {
            &r.events[..]
        };

        let serialized = if is_api_v2(query) {
            let v2_events = apply_v2_event_names(events);
            serde_json::to_string(&v2_events).unwrap_or_else(|_| "[]".into())
        } else {
            serde_json::to_string(events).unwrap_or_else(|_| "[]".into())
        };
        ("200 OK".to_string(), serialized)
    } else {
        (
            "404 Not Found".to_string(),
            ApiError::not_found(format!("run '{id}' not found")).to_json(),
        )
    }
}

async fn cancel_run(id: &str, body: &str, state: AppState) -> (String, String) {
    let reason = serde_json::from_str::<CancelRunRequest>(body)
        .ok()
        .and_then(|r| r.reason);

    let mut runs = state.runs.lock().await;
    if let Some(r) = runs.get_mut(id) {
        // Cancel idempotency: if already terminal, return 200 with stored terminal_event_id
        if r.status.is_terminal() {
            return (
                "200 OK".to_string(),
                serde_json::to_string(&CancelRunResponse {
                    run_id: id.to_string(),
                    state: r.status.clone(),
                    terminal_event_id: r.terminal_event_id.clone(),
                })
                .unwrap_or_else(|_| "{}".into()),
            );
        }

        r.status = RunStatus::Cancelled;
        let cancel_msg = reason.as_deref().unwrap_or("run marked cancelled");
        r.terminal_reason = Some(cancel_msg.to_string());
        let attempt = r.attempt_id;

        // Emit stage cancellation events: scan existing events to determine
        // the current state of each stage, then emit StageSkipped for queued
        // stages and StageFailed for running stages.
        {
            // Track stage states: stage_name -> (state, group_id, box_name, started_ts_ms, stage_attempt)
            let mut stage_states: HashMap<String, (&str, String, Option<String>, u64, u32)> =
                HashMap::new();
            for ev in &r.events {
                if let Some(ref sn) = ev.stage_name {
                    let gid = ev.group_id.clone().unwrap_or_default();
                    let bn = ev.box_name.clone();
                    let sa = ev
                        .payload
                        .as_ref()
                        .and_then(|p| p.get("stage_attempt"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(1) as u32;
                    match ev.event_type.as_str() {
                        "stage.queued" => {
                            stage_states.insert(sn.clone(), ("queued", gid, bn, ev.ts_ms, sa));
                        }
                        "stage.started" => {
                            stage_states.insert(sn.clone(), ("started", gid, bn, ev.ts_ms, sa));
                        }
                        "stage.completed" | "stage.failed" | "stage.skipped" => {
                            stage_states.insert(sn.clone(), ("terminal", gid, bn, ev.ts_ms, sa));
                        }
                        _ => {}
                    }
                }
            }

            // Collect cancellation events, sorted by group_id
            let mut cancel_events: Vec<(String, RunEvent)> = Vec::new();
            let now = now_ms();
            for (sn, (state, gid, bn, started_ts, sa)) in &stage_states {
                match *state {
                    "queued" => {
                        cancel_events.push((
                            gid.clone(),
                            crate::persistence::stage_event_skipped(
                                sn,
                                bn.as_deref(),
                                gid,
                                "run cancelled",
                                *sa,
                            ),
                        ));
                    }
                    "started" => {
                        let duration_ms = now.saturating_sub(*started_ts);
                        cancel_events.push((
                            gid.clone(),
                            crate::persistence::stage_event_failed(
                                sn,
                                bn.as_deref(),
                                gid,
                                duration_ms,
                                -1,
                                "run cancelled",
                                *sa,
                            ),
                        ));
                    }
                    _ => {} // terminal stages are immutable
                }
            }

            // Sort by group_id (lowest first)
            cancel_events.sort_by(|a, b| a.0.cmp(&b.0));

            for (_, mut ev) in cancel_events {
                ev.seq = Some(r.events.len() as u64);
                ev.attempt_id = Some(attempt);
                ev.run_id = Some(id.to_string());
                r.events.push(ev);
            }
        }

        let seq = r.events.len() as u64;
        let cancel_event = event_with_seq(
            id,
            "warn",
            "run.cancelled",
            cancel_msg.to_string(),
            seq,
            attempt,
        );
        let terminal_event_id = cancel_event.event_id.clone();
        r.terminal_event_id = terminal_event_id.clone();
        r.events.push(cancel_event);
        let now_ts = now_rfc3339();
        r.finished_at = Some(now_ts.clone());
        r.stage_states = Some(build_stage_states(&r.events));
        let (publication, _) =
            build_artifact_publication(id, &state.provider, &r.events, r.report.as_ref());
        r.artifact_publication = Some(publication);
        r.updated_at = Some(now_ts);
        let _ = state.provider.save_run(r);
        let response_status = r.status.clone();
        // Drop the runs lock before acquiring sidecar_handles lock
        drop(runs);

        // Clean up sidecar if one was started for this run
        if let Some(handle) = state.sidecar_handles.lock().await.remove(id) {
            handle.stop().await;
        }

        (
            "200 OK".to_string(),
            serde_json::to_string(&CancelRunResponse {
                run_id: id.to_string(),
                state: response_status,
                terminal_event_id,
            })
            .unwrap_or_else(|_| "{}".into()),
        )
    } else {
        (
            "404 Not Found".to_string(),
            ApiError::not_found(format!("run '{id}' not found")).to_json(),
        )
    }
}

async fn list_runs(query: Option<&str>, state: AppState) -> (String, String) {
    let runs = state.runs.lock().await;
    let state_filter = parse_query_param(query, "state");
    let filtered: Vec<RunState> = runs
        .values()
        .filter(|r| match state_filter.as_deref() {
            Some("active") => r.status.is_active(),
            Some("terminal") => r.status.is_terminal(),
            _ => true,
        })
        .cloned()
        .collect();
    (
        "200 OK".to_string(),
        serde_json::to_string(&ListRunsResponse { runs: filtered })
            .unwrap_or_else(|_| r#"{"runs":[]}"#.to_string()),
    )
}

async fn append_session_message(session_id: &str, body: &str, state: AppState) -> (String, String) {
    let req: AppendMessageRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                "400 Bad Request".to_string(),
                ApiError::invalid_spec(format!("invalid JSON: {e}")).to_json(),
            )
        }
    };

    let message = SessionMessage {
        ts_ms: now_ms(),
        role: req.role,
        content: req.content,
    };

    match state.provider.append_session_message(session_id, &message) {
        Ok(_) => (
            "200 OK".to_string(),
            serde_json::to_string(&message).unwrap_or_else(|_| "{}".into()),
        ),
        Err(e) => (
            "500 Internal Server Error".to_string(),
            ApiError::internal(format!("failed to persist message: {e}")).to_json(),
        ),
    }
}

async fn get_session_messages(session_id: &str, state: AppState) -> (String, String) {
    match state.provider.load_session_messages(session_id) {
        Ok(messages) => (
            "200 OK".to_string(),
            serde_json::to_string(&messages).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => (
            "500 Internal Server Error".to_string(),
            ApiError::internal(format!("failed to load messages: {e}")).to_json(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Stage view endpoint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StageView {
    stage_name: String,
    box_name: Option<String>,
    group_id: String,
    depends_on: Vec<String>,
    status: String,
    stage_attempt: u32,
    started_at: Option<String>,
    completed_at: Option<String>,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
}

#[derive(Serialize)]
struct StagesResponse {
    run_id: String,
    attempt_id: u64,
    updated_at: Option<String>,
    stages: Vec<StageView>,
}

async fn get_stages(id: &str, state: AppState) -> (String, String) {
    let runs = state.runs.lock().await;
    let Some(r) = runs.get(id) else {
        return (
            "404 Not Found".to_string(),
            ApiError::not_found(format!("run '{id}' not found")).to_json(),
        );
    };

    // Reconstruct stage graph from stage events
    let mut stages: HashMap<String, StageView> = HashMap::new();
    // Maintain insertion order by tracking order
    let mut order: Vec<String> = Vec::new();

    for ev in &r.events {
        let Some(ref sn) = ev.stage_name else {
            continue;
        };
        let gid = ev.group_id.clone().unwrap_or_default();
        let sa = ev
            .payload
            .as_ref()
            .and_then(|p| p.get("stage_attempt"))
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32;

        match ev.event_type.as_str() {
            "stage.queued" => {
                let depends_on = ev
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("depends_on"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                if !stages.contains_key(sn) {
                    order.push(sn.clone());
                }
                stages.insert(
                    sn.clone(),
                    StageView {
                        stage_name: sn.clone(),
                        box_name: ev.box_name.clone(),
                        group_id: gid,
                        depends_on,
                        status: "queued".to_string(),
                        stage_attempt: sa,
                        started_at: None,
                        completed_at: None,
                        duration_ms: None,
                        exit_code: None,
                    },
                );
            }
            "stage.started" => {
                if let Some(sv) = stages.get_mut(sn) {
                    sv.status = "running".to_string();
                    sv.started_at = ev.timestamp.clone();
                    sv.stage_attempt = sa;
                }
            }
            "stage.completed" => {
                if let Some(sv) = stages.get_mut(sn) {
                    sv.status = "succeeded".to_string();
                    sv.completed_at = ev.timestamp.clone();
                    sv.stage_attempt = sa;
                    sv.exit_code = ev
                        .payload
                        .as_ref()
                        .and_then(|p| p.get("exit_code"))
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                    sv.duration_ms = ev
                        .payload
                        .as_ref()
                        .and_then(|p| p.get("duration_ms"))
                        .and_then(|v| v.as_u64());
                }
            }
            "stage.failed" => {
                if let Some(sv) = stages.get_mut(sn) {
                    sv.status = "failed".to_string();
                    sv.completed_at = ev.timestamp.clone();
                    sv.stage_attempt = sa;
                    sv.exit_code = ev
                        .payload
                        .as_ref()
                        .and_then(|p| p.get("exit_code"))
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                    sv.duration_ms = ev
                        .payload
                        .as_ref()
                        .and_then(|p| p.get("duration_ms"))
                        .and_then(|v| v.as_u64());
                }
            }
            "stage.skipped" => {
                if let Some(sv) = stages.get_mut(sn) {
                    sv.status = "skipped".to_string();
                    sv.completed_at = ev.timestamp.clone();
                    sv.stage_attempt = sa;
                }
            }
            _ => {}
        }
    }

    let stages_vec: Vec<StageView> = order
        .into_iter()
        .filter_map(|name| stages.remove(&name))
        .collect();

    let resp = StagesResponse {
        run_id: id.to_string(),
        attempt_id: r.attempt_id,
        updated_at: r.updated_at.clone(),
        stages: stages_vec,
    };

    (
        "200 OK".to_string(),
        serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()),
    )
}

// ---------------------------------------------------------------------------
// Telemetry endpoint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TelemetryResponse {
    run_id: String,
    attempt_id: u64,
    next_seq: u64,
    samples: Vec<crate::observe::telemetry::TelemetrySample>,
}

async fn get_telemetry(id: &str, query: Option<&str>, state: AppState) -> (String, String) {
    // Parse from_seq — must be numeric if present
    let from_seq = match parse_query_param(query, "from_seq") {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                return (
                    "400 Bad Request".to_string(),
                    ApiError::invalid_params(format!(
                        "from_seq must be a non-negative integer, got '{}'",
                        v
                    ))
                    .to_json(),
                )
            }
        },
        None => 0,
    };

    let stage_name_filter = parse_query_param(query, "stage_name");

    // Check run exists
    let (attempt_id,) = {
        let runs = state.runs.lock().await;
        match runs.get(id) {
            Some(r) => (r.attempt_id,),
            None => {
                return (
                    "404 Not Found".to_string(),
                    ApiError::not_found(format!("run '{id}' not found")).to_json(),
                )
            }
        }
    };

    // Query ring buffer
    let (samples, next_seq) = {
        let bufs = state.telemetry_buffers.lock().await;
        if let Some(rb) = bufs.get(id) {
            if let Ok(buf) = rb.lock() {
                buf.query(from_seq, stage_name_filter.as_deref())
            } else {
                (Vec::new(), from_seq)
            }
        } else {
            (Vec::new(), from_seq)
        }
    };

    let resp = TelemetryResponse {
        run_id: id.to_string(),
        attempt_id,
        next_seq,
        samples,
    };

    (
        "200 OK".to_string(),
        serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()),
    )
}

#[deprecated(note = "use ApiError constructors instead")]
#[allow(dead_code)]
fn json_error(message: String) -> String {
    serde_json::json!({"error": message}).to_string()
}

fn event(run_id: &str, level: &str, event_type: &str, message: String) -> RunEvent {
    RunEvent {
        ts_ms: now_ms(),
        level: level.to_string(),
        event_type: event_type.to_string(),
        message,
        run_id: Some(run_id.to_string()),
        box_name: None,
        skill_id: None,
        skill_type: None,
        environment_id: None,
        mode: None,
        stream: None,
        seq: None,
        payload: None,
        stage_name: None,
        group_id: None,
        event_id: Some(generate_event_id()),
        attempt_id: Some(1),
        timestamp: Some(now_rfc3339()),
        event_type_v2: Some(legacy_to_v2_event_type(event_type)),
    }
}

/// Build an event with a pre-assigned `seq` (from the current run event count).
fn event_with_seq(
    run_id: &str,
    level: &str,
    event_type: &str,
    message: String,
    seq: u64,
    attempt_id: u64,
) -> RunEvent {
    let mut e = event(run_id, level, event_type, message);
    e.seq = Some(seq);
    e.attempt_id = Some(attempt_id);
    e
}

fn event_with_env(
    run_id: &str,
    level: &str,
    event_type: &str,
    message: String,
    environment_id: &str,
    mode: Option<&str>,
) -> RunEvent {
    let mut e = event(run_id, level, event_type, message);
    e.environment_id = Some(environment_id.to_string());
    e.mode = mode.map(ToString::to_string);
    e
}

#[allow(clippy::too_many_arguments)]
fn event_skill(
    run_id: &str,
    event_type: &str,
    message: String,
    skill: &str,
    skill_type: &str,
    environment_id: &str,
    mode: &str,
    box_name: Option<&str>,
) -> RunEvent {
    let mut e = event_with_env(
        run_id,
        "info",
        event_type,
        message,
        environment_id,
        Some(mode),
    );
    e.skill_id = Some(skill.to_string());
    e.skill_type = Some(skill_type.to_string());
    e.box_name = box_name.map(ToString::to_string);
    e
}

fn event_log_chunk(
    run_id: &str,
    seq: u64,
    stream: &str,
    data: String,
    stage_name: Option<&str>,
    group_id: Option<&str>,
) -> RunEvent {
    let mut e = event(run_id, "info", "log.chunk", "stream chunk".to_string());
    e.stream = Some(stream.to_string());
    e.seq = Some(seq);
    e.payload = Some(json!({ "data": data }));
    e.stage_name = stage_name.map(ToString::to_string);
    e.group_id = group_id.map(ToString::to_string);
    e
}

fn plan_events_from_spec(run_id: &str, environment_id: &str, spec: &RunSpec) -> Vec<RunEvent> {
    let mode = spec.sandbox.mode.as_str();
    let mut out = vec![event_with_env(
        run_id,
        "info",
        "run.spec.loaded",
        format!("loaded {} spec '{}'", kind_name(&spec.kind), spec.name),
        environment_id,
        Some(mode),
    )];

    match spec.kind {
        RunKind::Agent => {
            if let Some(agent) = &spec.agent {
                for entry in &agent.skills {
                    if let Some((skill_type, skill_id, display)) = skill_entry_info(entry) {
                        out.push(event_skill(
                            run_id,
                            "skill.mounted",
                            format!("mounted skill {}", display),
                            &skill_id,
                            &skill_type,
                            environment_id,
                            mode,
                            Some(&spec.name),
                        ));
                    }
                }
            }
        }
        RunKind::Pipeline => {
            if let Some(pipeline) = &spec.pipeline {
                for b in &pipeline.boxes {
                    out.push(event_with_env(
                        run_id,
                        "info",
                        "box.started",
                        format!("box '{}' prepared", b.name),
                        environment_id,
                        Some(mode),
                    ));
                    for entry in &b.skills {
                        if let Some((skill_type, skill_id, display)) = skill_entry_info(entry) {
                            out.push(event_skill(
                                run_id,
                                "skill.mounted",
                                format!("mounted skill {}", display),
                                &skill_id,
                                &skill_type,
                                environment_id,
                                mode,
                                Some(&b.name),
                            ));
                        }
                    }
                }
            }
        }
        RunKind::Workflow => {
            if let Some(w) = &spec.workflow {
                out.push(event_with_env(
                    run_id,
                    "info",
                    "workflow.planned",
                    format!("workflow has {} steps", w.steps.len()),
                    environment_id,
                    Some(mode),
                ));
            }
        }
    }

    out
}

/// Extract skill type and id from a SkillEntry for telemetry events.
fn skill_entry_info(entry: &crate::spec::SkillEntry) -> Option<(String, String, String)> {
    match entry {
        crate::spec::SkillEntry::Simple(raw) => {
            let (t, id) = raw.split_once(':')?;
            Some((t.to_string(), id.to_string(), raw.clone()))
        }
        crate::spec::SkillEntry::Oci { image, mount, .. } => Some((
            "oci".to_string(),
            image.clone(),
            format!("oci:{}:{}", image, mount),
        )),
        crate::spec::SkillEntry::Mcp { command, .. } => Some((
            "mcp".to_string(),
            command.clone(),
            format!("mcp:{}", command),
        )),
        crate::spec::SkillEntry::Inline { name, .. } => Some((
            "inline".to_string(),
            name.clone(),
            format!("inline:{}", name),
        )),
    }
}

async fn get_named_artifact(
    run_id: &str,
    stage_name: &str,
    artifact_name: &str,
    state: AppState,
) -> (String, String, Vec<u8>) {
    // Check run exists
    {
        let runs = state.runs.lock().await;
        if !runs.contains_key(run_id) {
            return as_json((
                "404 Not Found".to_string(),
                ApiError::not_found(format!("run '{run_id}' not found")).to_json(),
            ));
        }

        // Check if artifact publication is still in progress
        if let Some(r) = runs.get(run_id) {
            if let Some(ref pub_state) = r.artifact_publication {
                if pub_state.status == crate::persistence::ArtifactPublicationStatus::Publishing {
                    return as_json((
                        "409 Conflict".to_string(),
                        ApiError::artifact_publication_incomplete(format!(
                            "artifact publication in progress for run '{run_id}'"
                        ))
                        .to_json(),
                    ));
                }
            }
        }
    }

    match state
        .provider
        .load_named_artifact(run_id, stage_name, artifact_name)
    {
        Ok(Some(data)) => {
            let content_type = if serde_json::from_slice::<serde_json::Value>(&data).is_ok() {
                "application/json"
            } else if std::str::from_utf8(&data).is_ok() {
                "text/plain"
            } else {
                "application/octet-stream"
            };
            ("200 OK".to_string(), content_type.to_string(), data)
        }
        Ok(None) => as_json((
            "404 Not Found".to_string(),
            ApiError::artifact_not_found(format!(
                "artifact '{artifact_name}' not found for run '{run_id}' stage '{stage_name}'"
            ))
            .to_json(),
        )),
        Err(e) => as_json((
            "500 Internal Server Error".to_string(),
            ApiError::internal(format!("failed to load artifact: {e}")).to_json(),
        )),
    }
}

async fn get_stage_output_file(
    run_id: &str,
    stage_name: &str,
    _query: Option<&str>,
    state: AppState,
) -> (String, String, Vec<u8>) {
    let data = match state.provider.load_stage_artifact(run_id, stage_name) {
        Ok(Some(data)) => data,
        Ok(None) => {
            // Check if the run exists to differentiate error codes
            let runs = state.runs.lock().await;
            if runs.contains_key(run_id) {
                return as_json((
                    "404 Not Found".to_string(),
                    ApiError::structured_output_missing(format!(
                        "stage '{}' completed without result.json for run '{}'",
                        stage_name, run_id
                    ))
                    .to_json(),
                ));
            }
            return as_json((
                "404 Not Found".to_string(),
                ApiError::not_found(format!(
                    "no output file for run '{}' stage '{}'",
                    run_id, stage_name
                ))
                .to_json(),
            ));
        }
        Err(e) => {
            return as_json((
                "500 Internal Server Error".to_string(),
                ApiError::internal(format!("failed to load artifact: {e}")).to_json(),
            ));
        }
    };

    // Validate structured output: must be valid JSON with a "status" field
    match serde_json::from_slice::<serde_json::Value>(&data) {
        Ok(val) => {
            if val.get("status").is_none() {
                return as_json((
                    "422 Unprocessable Entity".to_string(),
                    ApiError::structured_output_malformed(format!(
                        "result.json for run '{}' stage '{}' is missing required 'status' field",
                        run_id, stage_name
                    ))
                    .to_json(),
                ));
            }
            ("200 OK".to_string(), "application/json".to_string(), data)
        }
        Err(_) => as_json((
            "422 Unprocessable Entity".to_string(),
            ApiError::structured_output_malformed(format!(
                "result.json for run '{}' stage '{}' is not valid JSON",
                run_id, stage_name
            ))
            .to_json(),
        )),
    }
}

fn kind_name(kind: &RunKind) -> &'static str {
    match kind {
        RunKind::Agent => "agent",
        RunKind::Pipeline => "pipeline",
        RunKind::Workflow => "workflow",
    }
}
