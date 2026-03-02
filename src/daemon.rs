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
use crate::runtime::run_file;
use crate::spec::{RunKind, RunSpec};

#[derive(Clone)]
struct AppState {
    runs: Arc<Mutex<HashMap<String, RunState>>>,
    provider: Arc<dyn PersistenceProvider>,
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

    let (status, payload) = route_request(method, path, query, body, state).await;
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        payload.len(),
        payload
    );
    stream.write_all(response.as_bytes()).await?;
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

async fn route_request(
    method: &str,
    path: &str,
    query: Option<&str>,
    body: &str,
    state: AppState,
) -> (String, String) {
    match (method, path) {
        ("GET", "/v1/health") => (
            "200 OK".to_string(),
            serde_json::to_string(&Health {
                status: "ok",
                persistence: state.provider.name(),
            })
            .unwrap_or_else(|_| "{}".into()),
        ),
        ("POST", "/v1/runs") => create_run(body, state).await,
        ("GET", "/v1/runs") => list_runs(query, state).await,
        _ => {
            if let Some(id) = path.strip_prefix("/v1/runs/") {
                if let Some(id) = id.strip_suffix("/events") {
                    return get_events(id, query, state).await;
                }
                if let Some(id) = id.strip_suffix("/cancel") {
                    if method == "POST" {
                        return cancel_run(id, body, state).await;
                    }
                }
                if method == "GET" {
                    return get_run(id, query, state).await;
                }
            }

            if let Some(id) = path.strip_prefix("/v1/sessions/") {
                if let Some(id) = id.strip_suffix("/messages") {
                    if method == "GET" {
                        return get_session_messages(id, state).await;
                    }
                    if method == "POST" {
                        return append_session_message(id, body, state).await;
                    }
                }
            }

            (
                "404 Not Found".to_string(),
                ApiError::not_found("route not found").to_json(),
            )
        }
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

    let mut planned_events = Vec::new();
    match crate::spec::load_spec(PathBuf::from(&req.file).as_path()) {
        Ok(spec) => planned_events = plan_events_from_spec(&run_id, &environment_id, &spec),
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

    let state_bg = state.clone();
    let run_id_bg = run_id.clone();
    let policy_bg = req.policy.clone();
    tokio::spawn(async move {
        let path = PathBuf::from(&req.file);
        let result = run_file(&path, req.input, policy_bg).await;

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
                            let mut chunk =
                                event_log_chunk(&run_id_bg, i as u64, "stdout", line.to_string());
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
            r.updated_at = Some(now_rfc3339());
            let _ = state_bg.provider.save_run(r);
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
    let runs = state.runs.lock().await;
    if let Some(r) = runs.get(id) {
        if is_api_v2(query) {
            let mut run = r.clone();
            run.events = apply_v2_event_names(&run.events);
            (
                "200 OK".to_string(),
                serde_json::to_string(&run).unwrap_or_else(|_| "{}".into()),
            )
        } else {
            (
                "200 OK".to_string(),
                serde_json::to_string(r).unwrap_or_else(|_| "{}".into()),
            )
        }
    } else {
        (
            "404 Not Found".to_string(),
            ApiError::not_found(format!("run '{id}' not found")).to_json(),
        )
    }
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
        let seq = r.events.len() as u64;
        let attempt = r.attempt_id;
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
        r.updated_at = Some(now_rfc3339());
        let _ = state.provider.save_run(r);

        (
            "200 OK".to_string(),
            serde_json::to_string(&CancelRunResponse {
                run_id: id.to_string(),
                state: r.status.clone(),
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

fn event_log_chunk(run_id: &str, seq: u64, stream: &str, data: String) -> RunEvent {
    let mut e = event(run_id, "info", "log.chunk", "stream chunk".to_string());
    e.stream = Some(stream.to_string());
    e.seq = Some(seq);
    e.payload = Some(json!({ "data": data }));
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
    }
}

fn kind_name(kind: &RunKind) -> &'static str {
    match kind {
        RunKind::Agent => "agent",
        RunKind::Pipeline => "pipeline",
        RunKind::Workflow => "workflow",
    }
}
