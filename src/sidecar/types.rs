use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxSnapshot {
    pub version: u64,
    pub execution_id: String,
    pub candidate_id: String,
    pub iteration: u32,
    pub entries: Vec<InboxEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    pub message_id: String,
    pub from_candidate_id: String,
    pub kind: String,
    pub payload: Value,
}

/// Intent as submitted by the agent (no auto-filled fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmittedIntent {
    pub kind: String,
    pub audience: String,
    pub payload: Value,
    pub priority: String,
}

/// Intent after sidecar stamps iteration, candidate_id, intent_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StampedIntent {
    pub intent_id: String,
    pub from_candidate_id: String,
    pub iteration: u32,
    pub kind: String,
    pub audience: String,
    pub payload: Value,
    pub priority: String,
    pub ttl_iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarContext {
    pub execution_id: String,
    pub candidate_id: String,
    pub iteration: u32,
    pub role: String,
    pub peers: Vec<String>,
    pub sidecar_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarHealth {
    pub status: String,
    pub sidecar_version: String,
    pub run_id: String,
    pub buffer_depth: usize,
    pub inbox_version: u64,
    pub uptime_ms: u64,
}
