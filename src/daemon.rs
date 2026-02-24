use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::persistence::{
    now_ms, provider_from_env, PersistenceProvider, RunEvent, RunState, RunStatus, SessionMessage,
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
}

#[derive(Debug, Serialize)]
struct CreateRunResponse {
    run_id: String,
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
    let path = parts.next().unwrap_or("");

    let body = if let Some(idx) = req.find("\r\n\r\n") {
        &req[idx + 4..]
    } else {
        ""
    };

    let (status, payload) = route_request(method, path, body, state).await;
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        payload.len(),
        payload
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn route_request(method: &str, path: &str, body: &str, state: AppState) -> (String, String) {
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
        _ => {
            if let Some(id) = path.strip_prefix("/v1/runs/") {
                if let Some(id) = id.strip_suffix("/events") {
                    return get_events(id, state).await;
                }
                if let Some(id) = id.strip_suffix("/cancel") {
                    if method == "POST" {
                        return cancel_run(id, state).await;
                    }
                }
                if method == "GET" {
                    return get_run(id, state).await;
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
                json_error("route not found".to_string()),
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
                json_error(format!("invalid JSON: {e}")),
            )
        }
    };

    let run_id = format!("run-{}", now_ms());
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

    {
        let mut runs = state.runs.lock().await;
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

        let run = RunState {
            id: run_id.clone(),
            status: RunStatus::Running,
            file: req.file.clone(),
            report: None,
            error: None,
            events,
        };

        let _ = state.provider.save_run(&run);
        runs.insert(run_id.clone(), run);
    }

    let state_bg = state.clone();
    let run_id_bg = run_id.clone();
    tokio::spawn(async move {
        let path = PathBuf::from(&req.file);
        let result = run_file(&path, req.input).await;

        let mut runs = state_bg.runs.lock().await;
        if let Some(r) = runs.get_mut(&run_id_bg) {
            match result {
                Ok(report) => {
                    r.status = RunStatus::Completed;
                    r.events.push(event(
                        &run_id_bg,
                        "info",
                        "run.finished",
                        "run completed".to_string(),
                    ));
                    if !report.output.is_empty() {
                        for (i, line) in report.output.lines().enumerate() {
                            r.events.push(event_log_chunk(
                                &run_id_bg,
                                i as u64,
                                "stdout",
                                line.to_string(),
                            ));
                        }
                        r.events.push(event(
                            &run_id_bg,
                            "info",
                            "log.closed",
                            "stdout stream closed".to_string(),
                        ));
                    }
                    r.report = Some(report);
                }
                Err(e) => {
                    r.status = RunStatus::Failed;
                    r.error = Some(e.to_string());
                    r.events.push(event(
                        &run_id_bg,
                        "error",
                        "run.failed",
                        format!("run failed: {e}"),
                    ));
                }
            }
            let _ = state_bg.provider.save_run(r);
        }
    });

    (
        "200 OK".to_string(),
        serde_json::to_string(&CreateRunResponse { run_id }).unwrap_or_else(|_| "{}".into()),
    )
}

async fn get_run(id: &str, state: AppState) -> (String, String) {
    let runs = state.runs.lock().await;
    if let Some(r) = runs.get(id) {
        (
            "200 OK".to_string(),
            serde_json::to_string(r).unwrap_or_else(|_| "{}".into()),
        )
    } else {
        (
            "404 Not Found".to_string(),
            json_error(format!("run '{id}' not found")),
        )
    }
}

async fn get_events(id: &str, state: AppState) -> (String, String) {
    let runs = state.runs.lock().await;
    if let Some(r) = runs.get(id) {
        (
            "200 OK".to_string(),
            serde_json::to_string(&r.events).unwrap_or_else(|_| "[]".into()),
        )
    } else {
        (
            "404 Not Found".to_string(),
            json_error(format!("run '{id}' not found")),
        )
    }
}

async fn cancel_run(id: &str, state: AppState) -> (String, String) {
    let mut runs = state.runs.lock().await;
    if let Some(r) = runs.get_mut(id) {
        r.status = RunStatus::Cancelled;
        r.events.push(event(
            id,
            "warn",
            "run.cancelled",
            "run marked cancelled".to_string(),
        ));
        let _ = state.provider.save_run(r);

        (
            "200 OK".to_string(),
            serde_json::to_string(r).unwrap_or_else(|_| "{}".into()),
        )
    } else {
        (
            "404 Not Found".to_string(),
            json_error(format!("run '{id}' not found")),
        )
    }
}

async fn append_session_message(session_id: &str, body: &str, state: AppState) -> (String, String) {
    let req: AppendMessageRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                "400 Bad Request".to_string(),
                json_error(format!("invalid JSON: {e}")),
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
            json_error(format!("failed to persist message: {e}")),
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
            json_error(format!("failed to load messages: {e}")),
        ),
    }
}

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
    }
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
