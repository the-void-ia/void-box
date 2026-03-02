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

#[allow(dead_code)]
fn _ensure_ascii_path(_p: &Path) {}
