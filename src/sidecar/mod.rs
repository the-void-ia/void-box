pub mod server;
pub mod state;
pub mod types;

pub use server::{start_sidecar, SidecarHandle};
pub use state::{IntentRejection, SidecarState};
pub use types::*;

/// Generate the messaging skill content for a generic agent.
/// The port is the sidecar's listen port on the host.
/// Agents inside the VM reach it via SLIRP at 10.0.2.2:<port>.
pub fn messaging_skill_content(port: u16) -> String {
    format!(
        r#"# Collaboration Protocol

You are part of a multi-agent execution. Use the messaging sidecar to coordinate with other agents.

## Your Identity
GET http://10.0.2.2:{port}/v1/context

Returns your execution_id, candidate_id, iteration, role, and peer list.

## Reading Messages
GET http://10.0.2.2:{port}/v1/inbox

Returns an InboxSnapshot with messages from other agents. Supports incremental polling:
GET http://10.0.2.2:{port}/v1/inbox?since=<version>

## Sending Messages
POST http://10.0.2.2:{port}/v1/intents
Content-Type: application/json

{{"kind": "<kind>", "audience": "<audience>", "payload": {{"summary_text": "..."}}, "priority": "<priority>"}}

### Message Kinds
- proposal: A concrete solution or approach you want to share
- signal: An observation or status update other agents should know
- evaluation: Your assessment of another agent's proposal

### Audience
- broadcast: Send to all agents
- leader: Send to the coordinator only

### Priority
- high: Urgent, should be processed first
- normal: Standard priority
- low: Background information

## Limits
- Maximum 3 intents per iteration
- Maximum 4096 bytes per intent payload
- Include an Idempotency-Key header for retry safety

## Health Check
GET http://10.0.2.2:{port}/v1/health
"#
    )
}
