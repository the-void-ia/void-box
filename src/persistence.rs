use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Starting,
    Running,
    #[serde(alias = "completed")]
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub fn is_active(&self) -> bool {
        !self.is_terminal()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub ts_ms: u64,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub event_type: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub box_name: Option<String>,
    #[serde(default)]
    pub skill_id: Option<String>,
    #[serde(default)]
    pub skill_type: Option<String>,
    #[serde(default)]
    pub environment_id: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub stream: Option<String>,
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub payload: Option<Value>,
    // --- stage-scoping fields ---
    #[serde(default)]
    pub stage_name: Option<String>,
    #[serde(default)]
    pub group_id: Option<String>,
    // --- v2 orchestration fields ---
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub attempt_id: Option<u64>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub event_type_v2: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactPublicationStatus {
    NotStarted,
    Publishing,
    Published,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactManifestEntry {
    pub name: String,
    pub stage: String,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    pub retrieval_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactPublication {
    pub status: ArtifactPublicationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(default)]
    pub manifest: Vec<ArtifactManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageState {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub id: String,
    pub status: RunStatus,
    pub file: String,
    pub report: Option<crate::runtime::RunReport>,
    pub error: Option<String>,
    pub events: Vec<RunEvent>,
    // --- v2 orchestration fields ---
    #[serde(default = "default_attempt_id")]
    pub attempt_id: u64,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub terminal_reason: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub active_stage_count: u32,
    #[serde(default)]
    pub active_microvm_count: u32,
    #[serde(default)]
    pub policy: Option<RunPolicy>,
    #[serde(default)]
    pub terminal_event_id: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub stage_states: Option<HashMap<String, StageState>>,
    #[serde(default)]
    pub artifact_publication: Option<ArtifactPublication>,
    /// True once a service agent has published its structured output.
    /// One-shot: once set, never reverts. Allows collection while still Running.
    #[serde(default)]
    pub output_ready: bool,
}

fn default_attempt_id() -> u64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPolicy {
    #[serde(default = "default_max_parallel_microvms")]
    pub max_parallel_microvms_per_run: u32,
    #[serde(default = "default_max_stage_retries")]
    pub max_stage_retries: u32,
    #[serde(default = "default_stage_timeout_secs")]
    pub stage_timeout_secs: u64,
    #[serde(default = "default_cancel_grace_period_secs")]
    pub cancel_grace_period_secs: u64,
}

fn default_max_parallel_microvms() -> u32 {
    4
}
fn default_max_stage_retries() -> u32 {
    3
}
fn default_stage_timeout_secs() -> u64 {
    3600
}
fn default_cancel_grace_period_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    pub ts_ms: u64,
    pub role: String,
    pub content: String,
}

pub trait PersistenceProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn load_runs(&self) -> Result<HashMap<String, RunState>>;
    fn save_run(&self, run: &RunState) -> Result<()>;
    fn append_session_message(&self, session_id: &str, message: &SessionMessage) -> Result<()>;
    fn load_session_messages(&self, session_id: &str) -> Result<Vec<SessionMessage>>;

    fn save_stage_artifact(&self, _run_id: &str, _stage_name: &str, _data: &[u8]) -> Result<()> {
        Ok(())
    }
    fn load_stage_artifact(&self, _run_id: &str, _stage_name: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn save_named_artifact(
        &self,
        _run_id: &str,
        _stage_name: &str,
        _name: &str,
        _data: &[u8],
    ) -> Result<()> {
        Ok(())
    }
    fn load_named_artifact(
        &self,
        _run_id: &str,
        _stage_name: &str,
        _name: &str,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn list_stage_artifacts(&self, _run_id: &str, _stage_name: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

pub fn provider_from_env() -> Arc<dyn PersistenceProvider> {
    let provider = std::env::var("VOIDBOX_PERSISTENCE_PROVIDER")
        .unwrap_or_else(|_| "disk".to_string())
        .to_ascii_lowercase();

    match provider.as_str() {
        "sqlite" => Arc::new(SqliteExampleProvider::new(default_state_dir())),
        "valkey" | "redis" => Arc::new(ValkeyExampleProvider::new(default_state_dir())),
        _ => Arc::new(DiskPersistenceProvider::new(default_state_dir())),
    }
}

fn default_state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("VOIDBOX_STATE_DIR") {
        return PathBuf::from(dir);
    }

    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state/void-box");
    }

    PathBuf::from("/tmp/void-box-state")
}

pub struct DiskPersistenceProvider {
    state_dir: PathBuf,
    lock: Mutex<()>,
}

impl DiskPersistenceProvider {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            lock: Mutex::new(()),
        }
    }

    fn runs_dir(&self) -> PathBuf {
        self.state_dir.join("runs")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.state_dir.join("artifacts")
    }

    fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.runs_dir())
            .map_err(|e| Error::Config(format!("failed to create runs dir: {e}")))?;
        fs::create_dir_all(self.sessions_dir())
            .map_err(|e| Error::Config(format!("failed to create sessions dir: {e}")))?;
        Ok(())
    }
}

impl PersistenceProvider for DiskPersistenceProvider {
    fn name(&self) -> &'static str {
        "disk"
    }

    fn load_runs(&self) -> Result<HashMap<String, RunState>> {
        let _g = self
            .lock
            .lock()
            .map_err(|_| Error::Config("persistence lock poisoned".into()))?;
        self.ensure_dirs()?;

        let mut out = HashMap::new();
        for entry in fs::read_dir(self.runs_dir())
            .map_err(|e| Error::Config(format!("failed to read runs dir: {e}")))?
        {
            let entry = entry.map_err(|e| Error::Config(format!("read_dir entry error: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let data = fs::read_to_string(&path)
                .map_err(|e| Error::Config(format!("failed reading {}: {e}", path.display())))?;
            let run: RunState = serde_json::from_str(&data)
                .map_err(|e| Error::Config(format!("invalid run file {}: {e}", path.display())))?;
            out.insert(run.id.clone(), run);
        }
        Ok(out)
    }

    fn save_run(&self, run: &RunState) -> Result<()> {
        let _g = self
            .lock
            .lock()
            .map_err(|_| Error::Config("persistence lock poisoned".into()))?;
        self.ensure_dirs()?;

        let path = self.runs_dir().join(format!("{}.json", run.id));
        let data = serde_json::to_vec_pretty(run)
            .map_err(|e| Error::Config(format!("serialize run failed: {e}")))?;
        fs::write(&path, data)
            .map_err(|e| Error::Config(format!("failed writing {}: {e}", path.display())))?;
        Ok(())
    }

    fn append_session_message(&self, session_id: &str, message: &SessionMessage) -> Result<()> {
        let _g = self
            .lock
            .lock()
            .map_err(|_| Error::Config("persistence lock poisoned".into()))?;
        self.ensure_dirs()?;

        let path = self.sessions_dir().join(format!("{}.jsonl", session_id));
        let line = serde_json::to_string(message)
            .map_err(|e| Error::Config(format!("serialize message failed: {e}")))?;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::Config(format!("failed opening {}: {e}", path.display())))?;

        writeln!(file, "{}", line)
            .map_err(|e| Error::Config(format!("failed appending {}: {e}", path.display())))?;
        Ok(())
    }

    fn load_session_messages(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        let _g = self
            .lock
            .lock()
            .map_err(|_| Error::Config("persistence lock poisoned".into()))?;
        self.ensure_dirs()?;

        let path = self.sessions_dir().join(format!("{}.jsonl", session_id));
        if !path.exists() {
            return Ok(Vec::new());
        }

        let text = fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("failed reading {}: {e}", path.display())))?;

        let mut out = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let msg: SessionMessage = serde_json::from_str(line).map_err(|e| {
                Error::Config(format!("invalid session line in {}: {e}", path.display()))
            })?;
            out.push(msg);
        }

        Ok(out)
    }

    fn save_stage_artifact(&self, run_id: &str, stage_name: &str, data: &[u8]) -> Result<()> {
        let dir = self.artifacts_dir().join(run_id).join(stage_name);
        fs::create_dir_all(&dir)
            .map_err(|e| Error::Config(format!("failed to create artifact dir: {e}")))?;
        let path = dir.join("output.json");
        fs::write(&path, data).map_err(|e| {
            Error::Config(format!("failed writing artifact {}: {e}", path.display()))
        })?;
        Ok(())
    }

    fn load_stage_artifact(&self, run_id: &str, stage_name: &str) -> Result<Option<Vec<u8>>> {
        let path = self
            .artifacts_dir()
            .join(run_id)
            .join(stage_name)
            .join("output.json");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path).map_err(|e| {
            Error::Config(format!("failed reading artifact {}: {e}", path.display()))
        })?;
        Ok(Some(data))
    }

    fn save_named_artifact(
        &self,
        run_id: &str,
        stage_name: &str,
        name: &str,
        data: &[u8],
    ) -> Result<()> {
        if name.contains("..") || name.contains('/') || name.contains('\\') || name.is_empty() {
            return Err(Error::Config(format!("invalid artifact name: {name}")));
        }
        let dir = self.artifacts_dir().join(run_id).join(stage_name);
        fs::create_dir_all(&dir)
            .map_err(|e| Error::Config(format!("failed to create artifact dir: {e}")))?;
        let path = dir.join(name);
        fs::write(&path, data).map_err(|e| {
            Error::Config(format!(
                "failed writing named artifact {}: {e}",
                path.display()
            ))
        })?;
        Ok(())
    }

    fn load_named_artifact(
        &self,
        run_id: &str,
        stage_name: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>> {
        if name.contains("..") || name.contains('/') || name.contains('\\') || name.is_empty() {
            return Err(Error::Config(format!("invalid artifact name: {name}")));
        }
        let path = self
            .artifacts_dir()
            .join(run_id)
            .join(stage_name)
            .join(name);
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path).map_err(|e| {
            Error::Config(format!(
                "failed reading named artifact {}: {e}",
                path.display()
            ))
        })?;
        Ok(Some(data))
    }

    fn list_stage_artifacts(&self, run_id: &str, stage_name: &str) -> Result<Vec<String>> {
        let dir = self.artifacts_dir().join(run_id).join(stage_name);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir)
            .map_err(|e| Error::Config(format!("failed reading artifact dir: {e}")))?
        {
            let entry = entry.map_err(|e| Error::Config(format!("read_dir entry error: {e}")))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }
}

pub struct SqliteExampleProvider {
    fallback: DiskPersistenceProvider,
}

impl SqliteExampleProvider {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            fallback: DiskPersistenceProvider::new(state_dir),
        }
    }

    #[allow(dead_code)]
    fn sqlite_db_path(&self) -> PathBuf {
        self.fallback.state_dir.join("void-box.sqlite3")
    }
}

impl PersistenceProvider for SqliteExampleProvider {
    fn name(&self) -> &'static str {
        "sqlite-example"
    }

    fn load_runs(&self) -> Result<HashMap<String, RunState>> {
        // Example adapter: delegates to disk today. Swap with rusqlite/sqlx backend later.
        self.fallback.load_runs()
    }

    fn save_run(&self, run: &RunState) -> Result<()> {
        self.fallback.save_run(run)
    }

    fn append_session_message(&self, session_id: &str, message: &SessionMessage) -> Result<()> {
        self.fallback.append_session_message(session_id, message)
    }

    fn load_session_messages(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        self.fallback.load_session_messages(session_id)
    }
}

pub struct ValkeyExampleProvider {
    fallback: DiskPersistenceProvider,
}

impl ValkeyExampleProvider {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            fallback: DiskPersistenceProvider::new(state_dir),
        }
    }

    #[allow(dead_code)]
    fn valkey_url(&self) -> String {
        std::env::var("VOIDBOX_VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into())
    }
}

impl PersistenceProvider for ValkeyExampleProvider {
    fn name(&self) -> &'static str {
        "valkey-example"
    }

    fn load_runs(&self) -> Result<HashMap<String, RunState>> {
        // Example adapter: delegates to disk today. Swap with redis/valkey backend later.
        self.fallback.load_runs()
    }

    fn save_run(&self, run: &RunState) -> Result<()> {
        self.fallback.save_run(run)
    }

    fn append_session_message(&self, session_id: &str, message: &SessionMessage) -> Result<()> {
        self.fallback.append_session_message(session_id, message)
    }

    fn load_session_messages(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        self.fallback.load_session_messages(session_id)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// RFC 3339 timestamp for the current time.
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339(SystemTime::now()).to_string()
}

/// Generate a UUID v7 event ID (time-ordered).
pub fn generate_event_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Map legacy dotted event types to PascalCase v2 names.
pub fn legacy_to_v2_event_type(legacy: &str) -> String {
    match legacy {
        "run.started" => "RunStarted".to_string(),
        "run.finished" => "RunCompleted".to_string(),
        "run.failed" => "RunFailed".to_string(),
        "run.cancelled" => "RunCancelled".to_string(),
        "run.spec.loaded" => "SpecLoaded".to_string(),
        "env.provisioned" => "EnvironmentProvisioned".to_string(),
        "box.started" => "BoxStarted".to_string(),
        "skill.mounted" => "SkillMounted".to_string(),
        "workflow.planned" => "WorkflowPlanned".to_string(),
        "log.chunk" => "LogChunk".to_string(),
        "log.closed" => "LogClosed".to_string(),
        "spec.parse_failed" => "SpecParseFailed".to_string(),
        "stage.queued" => "StageQueued".to_string(),
        "stage.started" => "StageStarted".to_string(),
        "stage.completed" => "StageSucceeded".to_string(),
        "stage.failed" => "StageFailed".to_string(),
        "stage.skipped" => "StageSkipped".to_string(),
        other => {
            // Best-effort PascalCase: "foo.bar_baz" → "FooBarBaz"
            other
                .split(['.', '_'])
                .filter(|s| !s.is_empty())
                .map(|s| {
                    let mut chars = s.chars();
                    match chars.next() {
                        Some(c) => {
                            let mut out = c.to_uppercase().to_string();
                            out.extend(chars);
                            out
                        }
                        None => String::new(),
                    }
                })
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Stage event builders
// ---------------------------------------------------------------------------

/// Build a `StageQueued` event. `seq` and `attempt_id` are left unset — the
/// collector task in `daemon.rs` assigns them when the event is pushed into
/// `RunState`.
pub fn stage_event_queued(
    stage_name: &str,
    box_name: Option<&str>,
    group_id: &str,
    depends_on: &[String],
) -> RunEvent {
    RunEvent {
        ts_ms: now_ms(),
        level: "info".to_string(),
        event_type: "stage.queued".to_string(),
        message: format!("stage '{}' queued", stage_name),
        run_id: None,
        box_name: box_name.map(ToString::to_string),
        skill_id: None,
        skill_type: None,
        environment_id: None,
        mode: None,
        stream: None,
        seq: None,
        payload: Some(serde_json::json!({ "depends_on": depends_on })),
        stage_name: Some(stage_name.to_string()),
        group_id: Some(group_id.to_string()),
        event_id: Some(generate_event_id()),
        attempt_id: None,
        timestamp: Some(now_rfc3339()),
        event_type_v2: Some(legacy_to_v2_event_type("stage.queued")),
    }
}

pub fn stage_event_started(
    stage_name: &str,
    box_name: Option<&str>,
    group_id: &str,
    stage_attempt: u32,
) -> RunEvent {
    RunEvent {
        ts_ms: now_ms(),
        level: "info".to_string(),
        event_type: "stage.started".to_string(),
        message: format!("stage '{}' started (attempt {})", stage_name, stage_attempt),
        run_id: None,
        box_name: box_name.map(ToString::to_string),
        skill_id: None,
        skill_type: None,
        environment_id: None,
        mode: None,
        stream: None,
        seq: None,
        payload: Some(serde_json::json!({ "stage_attempt": stage_attempt })),
        stage_name: Some(stage_name.to_string()),
        group_id: Some(group_id.to_string()),
        event_id: Some(generate_event_id()),
        attempt_id: None,
        timestamp: Some(now_rfc3339()),
        event_type_v2: Some(legacy_to_v2_event_type("stage.started")),
    }
}

pub fn stage_event_succeeded(
    stage_name: &str,
    box_name: Option<&str>,
    group_id: &str,
    duration_ms: u64,
    exit_code: i32,
    stage_attempt: u32,
) -> RunEvent {
    RunEvent {
        ts_ms: now_ms(),
        level: "info".to_string(),
        event_type: "stage.completed".to_string(),
        message: format!("stage '{}' succeeded in {}ms", stage_name, duration_ms),
        run_id: None,
        box_name: box_name.map(ToString::to_string),
        skill_id: None,
        skill_type: None,
        environment_id: None,
        mode: None,
        stream: None,
        seq: None,
        payload: Some(serde_json::json!({
            "duration_ms": duration_ms,
            "exit_code": exit_code,
            "stage_attempt": stage_attempt,
        })),
        stage_name: Some(stage_name.to_string()),
        group_id: Some(group_id.to_string()),
        event_id: Some(generate_event_id()),
        attempt_id: None,
        timestamp: Some(now_rfc3339()),
        event_type_v2: Some(legacy_to_v2_event_type("stage.completed")),
    }
}

pub fn stage_event_failed(
    stage_name: &str,
    box_name: Option<&str>,
    group_id: &str,
    duration_ms: u64,
    exit_code: i32,
    error: &str,
    stage_attempt: u32,
) -> RunEvent {
    RunEvent {
        ts_ms: now_ms(),
        level: "error".to_string(),
        event_type: "stage.failed".to_string(),
        message: format!("stage '{}' failed: {}", stage_name, error),
        run_id: None,
        box_name: box_name.map(ToString::to_string),
        skill_id: None,
        skill_type: None,
        environment_id: None,
        mode: None,
        stream: None,
        seq: None,
        payload: Some(serde_json::json!({
            "duration_ms": duration_ms,
            "exit_code": exit_code,
            "error": error,
            "stage_attempt": stage_attempt,
        })),
        stage_name: Some(stage_name.to_string()),
        group_id: Some(group_id.to_string()),
        event_id: Some(generate_event_id()),
        attempt_id: None,
        timestamp: Some(now_rfc3339()),
        event_type_v2: Some(legacy_to_v2_event_type("stage.failed")),
    }
}

pub fn stage_event_skipped(
    stage_name: &str,
    box_name: Option<&str>,
    group_id: &str,
    reason: &str,
    stage_attempt: u32,
) -> RunEvent {
    RunEvent {
        ts_ms: now_ms(),
        level: "warn".to_string(),
        event_type: "stage.skipped".to_string(),
        message: format!("stage '{}' skipped: {}", stage_name, reason),
        run_id: None,
        box_name: box_name.map(ToString::to_string),
        skill_id: None,
        skill_type: None,
        environment_id: None,
        mode: None,
        stream: None,
        seq: None,
        payload: Some(serde_json::json!({
            "reason": reason,
            "stage_attempt": stage_attempt,
        })),
        stage_name: Some(stage_name.to_string()),
        group_id: Some(group_id.to_string()),
        event_id: Some(generate_event_id()),
        attempt_id: None,
        timestamp: Some(now_rfc3339()),
        event_type_v2: Some(legacy_to_v2_event_type("stage.skipped")),
    }
}

#[allow(dead_code)]
fn _ensure_ascii_path(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stage_event_queued_fields() {
        let ev = stage_event_queued("build", Some("build-box"), "g1", &["fetch".to_string()]);
        assert_eq!(ev.event_type, "stage.queued");
        assert_eq!(ev.event_type_v2.as_deref(), Some("StageQueued"));
        assert_eq!(ev.stage_name.as_deref(), Some("build"));
        assert_eq!(ev.box_name.as_deref(), Some("build-box"));
        assert_eq!(ev.group_id.as_deref(), Some("g1"));
        assert!(ev.payload.is_some());
        let deps = ev.payload.unwrap();
        assert_eq!(deps["depends_on"][0].as_str(), Some("fetch"));
    }

    #[test]
    fn test_stage_event_started_fields() {
        let ev = stage_event_started("build", Some("build-box"), "g1", 2);
        assert_eq!(ev.event_type, "stage.started");
        assert_eq!(ev.event_type_v2.as_deref(), Some("StageStarted"));
        assert_eq!(ev.stage_name.as_deref(), Some("build"));
        let payload = ev.payload.unwrap();
        assert_eq!(payload["stage_attempt"], 2);
    }

    #[test]
    fn test_stage_event_succeeded_fields() {
        let ev = stage_event_succeeded("build", Some("build-box"), "g1", 4500, 0, 1);
        assert_eq!(ev.event_type, "stage.completed");
        assert_eq!(ev.event_type_v2.as_deref(), Some("StageSucceeded"));
        let payload = ev.payload.unwrap();
        assert_eq!(payload["duration_ms"], 4500);
        assert_eq!(payload["exit_code"], 0);
        assert_eq!(payload["stage_attempt"], 1);
    }

    #[test]
    fn test_stage_event_failed_fields() {
        let ev = stage_event_failed(
            "build",
            None,
            "g0",
            1200,
            1,
            "command exited with code 1",
            1,
        );
        assert_eq!(ev.event_type, "stage.failed");
        assert_eq!(ev.event_type_v2.as_deref(), Some("StageFailed"));
        assert!(ev.box_name.is_none());
        let payload = ev.payload.unwrap();
        assert_eq!(payload["duration_ms"], 1200);
        assert_eq!(payload["exit_code"], 1);
        assert_eq!(payload["error"], "command exited with code 1");
    }

    #[test]
    fn test_stage_event_skipped_fields() {
        let ev = stage_event_skipped("deploy", None, "g2", "dependency \"build\" failed", 1);
        assert_eq!(ev.event_type, "stage.skipped");
        assert_eq!(ev.event_type_v2.as_deref(), Some("StageSkipped"));
        let payload = ev.payload.unwrap();
        assert_eq!(payload["reason"], "dependency \"build\" failed");
    }

    #[test]
    fn test_legacy_to_v2_stage_mappings() {
        assert_eq!(legacy_to_v2_event_type("stage.queued"), "StageQueued");
        assert_eq!(legacy_to_v2_event_type("stage.started"), "StageStarted");
        assert_eq!(legacy_to_v2_event_type("stage.completed"), "StageSucceeded");
        assert_eq!(legacy_to_v2_event_type("stage.failed"), "StageFailed");
        assert_eq!(legacy_to_v2_event_type("stage.skipped"), "StageSkipped");
    }

    #[test]
    fn test_stage_artifact_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let provider = DiskPersistenceProvider::new(dir.path().to_path_buf());
        let data = br#"{"result": "ok"}"#;

        provider
            .save_stage_artifact("run-1", "build", data)
            .unwrap();
        let loaded = provider.load_stage_artifact("run-1", "build").unwrap();
        assert_eq!(loaded.as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn test_stage_artifact_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let provider = DiskPersistenceProvider::new(dir.path().to_path_buf());
        let loaded = provider
            .load_stage_artifact("no-such-run", "no-stage")
            .unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_named_artifact_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let provider = DiskPersistenceProvider::new(dir.path().to_path_buf());
        let data = b"# Report\nAll good";
        provider
            .save_named_artifact("run-1", "main", "report.md", data)
            .unwrap();
        let loaded = provider
            .load_named_artifact("run-1", "main", "report.md")
            .unwrap();
        assert_eq!(loaded, Some(data.to_vec()));
    }

    #[test]
    fn test_named_artifact_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let provider = DiskPersistenceProvider::new(dir.path().to_path_buf());
        let loaded = provider
            .load_named_artifact("run-x", "main", "missing.md")
            .unwrap();
        assert_eq!(loaded, None);
    }

    #[test]
    fn test_list_stage_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = DiskPersistenceProvider::new(dir.path().to_path_buf());
        provider
            .save_named_artifact("run-1", "main", "report.md", b"data1")
            .unwrap();
        provider
            .save_named_artifact("run-1", "main", "metrics.json", b"data2")
            .unwrap();
        let names = provider.list_stage_artifacts("run-1", "main").unwrap();
        assert!(names.contains(&"report.md".to_string()));
        assert!(names.contains(&"metrics.json".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn test_run_status_terminal() {
        assert!(RunStatus::Succeeded.is_terminal());
        assert!(RunStatus::Failed.is_terminal());
        assert!(RunStatus::Cancelled.is_terminal());
        assert!(!RunStatus::Pending.is_terminal());
        assert!(!RunStatus::Starting.is_terminal());
        assert!(!RunStatus::Running.is_terminal());
    }

    #[test]
    fn test_run_state_round_trip_with_artifact_publication() {
        let dir = tempfile::tempdir().unwrap();
        let provider = DiskPersistenceProvider::new(dir.path().to_path_buf());

        let mut stage_states = HashMap::new();
        stage_states.insert(
            "main".to_string(),
            StageState {
                status: "succeeded".to_string(),
                started_at: Some("2026-03-20T18:19:00Z".to_string()),
                completed_at: Some("2026-03-20T18:20:00Z".to_string()),
            },
        );

        let run = RunState {
            id: "round-trip-test".to_string(),
            status: RunStatus::Succeeded,
            file: "test.yaml".to_string(),
            report: None,
            error: None,
            events: Vec::new(),
            attempt_id: 1,
            started_at: Some("2026-03-20T18:19:00Z".to_string()),
            updated_at: Some("2026-03-20T18:20:00Z".to_string()),
            terminal_reason: None,
            exit_code: Some(0),
            active_stage_count: 0,
            active_microvm_count: 0,
            policy: None,
            terminal_event_id: Some("evt_123".to_string()),
            finished_at: Some("2026-03-20T18:20:00Z".to_string()),
            stage_states: Some(stage_states),
            artifact_publication: Some(ArtifactPublication {
                status: ArtifactPublicationStatus::Published,
                published_at: Some("2026-03-20T18:20:00Z".to_string()),
                manifest: vec![ArtifactManifestEntry {
                    name: "result.json".to_string(),
                    stage: "main".to_string(),
                    media_type: "application/json".to_string(),
                    size_bytes: Some(128),
                    retrieval_path: "/v1/runs/round-trip-test/stages/main/output-file".to_string(),
                }],
            }),
            output_ready: false,
        };

        provider.save_run(&run).unwrap();

        let loaded = provider.load_runs().unwrap();
        let loaded_run = loaded.get("round-trip-test").unwrap();
        assert_eq!(loaded_run.finished_at, run.finished_at);
        assert!(loaded_run.stage_states.is_some());
        let ss = loaded_run.stage_states.as_ref().unwrap();
        assert_eq!(ss.get("main").unwrap().status, "succeeded");
        assert!(loaded_run.artifact_publication.is_some());
        let ap = loaded_run.artifact_publication.as_ref().unwrap();
        assert_eq!(ap.status, ArtifactPublicationStatus::Published);
        assert_eq!(ap.manifest.len(), 1);
        assert_eq!(ap.manifest[0].name, "result.json");
    }

    #[test]
    fn output_ready_defaults_to_false() {
        let json = r#"{"id":"test","status":"running","file":"test.yaml","events":[]}"#;
        let run: RunState = serde_json::from_str(json).unwrap();
        assert!(!run.output_ready);
    }

    #[test]
    fn output_ready_round_trips() {
        let json = r#"{"id":"test","status":"running","file":"test.yaml","events":[],"output_ready":true}"#;
        let run: RunState = serde_json::from_str(json).unwrap();
        assert!(run.output_ready);
        let serialized = serde_json::to_string(&run).unwrap();
        assert!(serialized.contains("\"output_ready\":true"));
    }
}
