use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::types::{InboxSnapshot, StampedIntent, SubmittedIntent};

const MAX_INTENTS_PER_ITERATION: usize = 3;
const MAX_INTENT_PAYLOAD_BYTES: usize = 4096;
const DEFAULT_TTL_ITERATIONS: u32 = 2;

#[derive(Debug)]
pub struct SidecarState {
    run_id: String,
    execution_id: String,
    candidate_id: String,
    peers: Vec<String>,
    inbox: Option<InboxSnapshot>,
    intent_buffer: Vec<StampedIntent>,
    iteration_intent_count: usize,
    content_hashes: HashSet<u64>,
    idempotency_keys: HashMap<String, usize>,
    started_at: Instant,
}

impl SidecarState {
    pub fn new(run_id: &str, execution_id: &str, candidate_id: &str, peers: Vec<String>) -> Self {
        Self {
            run_id: run_id.into(),
            execution_id: execution_id.into(),
            candidate_id: candidate_id.into(),
            peers,
            inbox: None,
            intent_buffer: Vec::new(),
            iteration_intent_count: 0,
            content_hashes: HashSet::new(),
            idempotency_keys: HashMap::new(),
            started_at: Instant::now(),
        }
    }

    pub fn inbox_version(&self) -> u64 {
        self.inbox.as_ref().map_or(0, |i| i.version)
    }

    pub fn buffer_depth(&self) -> usize {
        self.intent_buffer.len()
    }

    pub fn current_iteration(&self) -> u32 {
        self.inbox.as_ref().map_or(0, |i| i.iteration)
    }

    pub fn uptime_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    pub fn candidate_id(&self) -> &str {
        &self.candidate_id
    }

    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    pub fn load_inbox(&mut self, snapshot: InboxSnapshot) {
        self.iteration_intent_count = 0;
        self.content_hashes.clear();
        self.idempotency_keys.clear();
        self.inbox = Some(snapshot);
    }

    pub fn get_inbox(&self, since: Option<u64>) -> InboxSnapshot {
        match &self.inbox {
            Some(inbox) => {
                if let Some(since_version) = since {
                    if since_version >= inbox.version {
                        return InboxSnapshot {
                            version: inbox.version,
                            execution_id: inbox.execution_id.clone(),
                            candidate_id: inbox.candidate_id.clone(),
                            iteration: inbox.iteration,
                            entries: vec![],
                        };
                    }
                }
                inbox.clone()
            }
            None => InboxSnapshot {
                version: 0,
                execution_id: self.execution_id.clone(),
                candidate_id: self.candidate_id.clone(),
                iteration: 0,
                entries: vec![],
            },
        }
    }

    pub fn accept_intent(
        &mut self,
        submitted: SubmittedIntent,
        idempotency_key: Option<String>,
    ) -> Result<StampedIntent, IntentRejection> {
        // Idempotency key dedup
        if let Some(ref key) = idempotency_key {
            if let Some(&idx) = self.idempotency_keys.get(key) {
                return Ok(self.intent_buffer[idx].clone());
            }
        }

        // Content-based dedup
        let content_hash = compute_content_hash(
            &submitted.payload,
            &submitted.audience,
            self.current_iteration(),
        );
        if self.content_hashes.contains(&content_hash) {
            let existing = self.intent_buffer.iter().find(|i| {
                compute_content_hash(&i.payload, &i.audience, i.iteration) == content_hash
            });
            if let Some(existing) = existing {
                return Ok(existing.clone());
            }
        }

        // Payload size check
        let payload_bytes =
            serde_json::to_vec(&submitted.payload).map_err(|_| IntentRejection::PayloadTooLarge)?;
        if payload_bytes.len() > MAX_INTENT_PAYLOAD_BYTES {
            return Err(IntentRejection::PayloadTooLarge);
        }

        // Per-iteration limit
        if self.iteration_intent_count >= MAX_INTENTS_PER_ITERATION {
            return Err(IntentRejection::MaxPerIteration);
        }

        let intent_id = format!("int-{}", uuid_v4());
        let stamped = StampedIntent {
            intent_id,
            from_candidate_id: self.candidate_id.clone(),
            iteration: self.current_iteration(),
            kind: submitted.kind,
            audience: submitted.audience,
            payload: submitted.payload,
            priority: submitted.priority,
            ttl_iterations: DEFAULT_TTL_ITERATIONS,
        };

        let idx = self.intent_buffer.len();
        self.intent_buffer.push(stamped.clone());
        self.iteration_intent_count += 1;
        self.content_hashes.insert(content_hash);

        if let Some(key) = idempotency_key {
            self.idempotency_keys.insert(key, idx);
        }

        Ok(stamped)
    }

    pub fn drain_intents(&mut self) -> Vec<StampedIntent> {
        let drained = std::mem::take(&mut self.intent_buffer);
        self.content_hashes.clear();
        self.idempotency_keys.clear();
        // Do NOT reset iteration_intent_count — that resets on inbox load
        drained
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentRejection {
    MaxPerIteration,
    PayloadTooLarge,
    RateLimited,
}

fn compute_content_hash(payload: &serde_json::Value, audience: &str, iteration: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let normalized = serde_json::to_string(payload).unwrap_or_default();
    normalized.hash(&mut hasher);
    audience.hash(&mut hasher);
    iteration.hash(&mut hasher);
    hasher.finish()
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:016x}{:016x}",
        nanos,
        nanos.wrapping_mul(6364136223846793005)
    )
}
