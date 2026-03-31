pub mod server;
pub mod state;
pub mod types;

pub use server::{start_sidecar, SidecarHandle};
pub use state::{IntentRejection, SidecarState};
pub use types::*;

/// Generate the messaging skill content for a generic agent.
/// Documents the `void-message` CLI which reads VOID_SIDECAR_URL from env.
pub fn messaging_skill_content() -> String {
    r#"# Collaboration Protocol

You are part of a multi-agent execution. Use the `void-message` CLI to coordinate.

## Read your identity
```
void-message context
```

Returns your execution_id, candidate_id, iteration, role, and peer list.

## Read inbox
```
void-message inbox
```

Returns messages from other agents. For incremental polling:
```
void-message inbox --since 3
```

## Send an intent
```
void-message send --kind <kind> --audience <audience> --summary "<text>" [--priority <priority>]
```

### Kinds
- `proposal` — a concrete solution or approach you want to share
- `signal` — an observation or status update other agents should know
- `evaluation` — your assessment of another agent's proposal

### Audience
- `broadcast` — send to all agents
- `leader` — send to the coordinator only

### Priority
- `high` — urgent, should be processed first
- `normal` — standard priority (default)
- `low` — background information

### Examples
```
void-message send --kind signal --audience broadcast --summary "cache misses dominate p99"
void-message send --kind proposal --audience leader --summary "promote cache-aware variant"
void-message send --kind evaluation --audience broadcast --summary "approach A handles edge cases better" --priority high
```

## Limits
- Maximum 3 intents per iteration
- Maximum 4096 bytes per intent payload

## Health check
```
void-message health
```
"#
    .to_string()
}
