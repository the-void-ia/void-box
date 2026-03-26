use serde_json::{json, Value};

use crate::http;

pub fn tool_list() -> Value {
    json!({
        "tools": [
            {
                "name": "read_shared_context",
                "description": "Read the shared execution context for this candidate before evaluating the assigned role. Returns your execution identity, candidate ID, iteration number, role, and peer list.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "read_peer_messages",
                "description": "Read observations already shared by sibling candidates. Returns peer-visible inbox content with message entries from other agents in the swarm.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "since": {
                            "type": "integer",
                            "description": "Only return messages added after this version number (for incremental polling)."
                        }
                    },
                    "required": []
                }
            },
            {
                "name": "broadcast_observation",
                "description": "Share a concise finding that could help sibling candidates refine or compare their work. This sends a signal-type message visible to all agents in the swarm.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "summary_text": {
                            "type": "string",
                            "description": "A concise observation to share with all agents."
                        },
                        "priority": {
                            "type": "string",
                            "enum": ["high", "normal", "low"],
                            "description": "Message priority (defaults to normal)."
                        }
                    },
                    "required": ["summary_text"]
                }
            },
            {
                "name": "recommend_to_leader",
                "description": "Send a short recommendation to the leader about whether this candidate's approach should be promoted, refined, or rejected.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "summary_text": {
                            "type": "string",
                            "description": "A concise recommendation for the leader."
                        },
                        "disposition": {
                            "type": "string",
                            "enum": ["promote", "refine", "reject"],
                            "description": "Suggested action for the leader (defaults to promote)."
                        },
                        "priority": {
                            "type": "string",
                            "enum": ["high", "normal", "low"],
                            "description": "Message priority (defaults to normal)."
                        }
                    },
                    "required": ["summary_text"]
                }
            }
        ]
    })
}

pub fn handle_call(base_url: &str, name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "read_shared_context" => {
            let body = http::get(base_url, "/v1/context")?;
            Ok(json!({
                "content": [{"type": "text", "text": body}]
            }))
        }
        "read_peer_messages" => {
            let path = match arguments.get("since").and_then(|v| v.as_i64()) {
                Some(since) => format!("/v1/inbox?since={since}"),
                None => "/v1/inbox".to_string(),
            };
            let body = http::get(base_url, &path)?;
            Ok(json!({
                "content": [{"type": "text", "text": body}]
            }))
        }
        "broadcast_observation" => {
            let summary_text = arguments
                .get("summary_text")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: summary_text")?;
            let priority = arguments
                .get("priority")
                .and_then(|v| v.as_str())
                .unwrap_or("normal");

            send_intent(base_url, "signal", "broadcast", summary_text, priority)
        }
        "recommend_to_leader" => {
            let summary_text = arguments
                .get("summary_text")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: summary_text")?;
            let priority = arguments
                .get("priority")
                .and_then(|v| v.as_str())
                .unwrap_or("normal");
            let disposition = arguments
                .get("disposition")
                .and_then(|v| v.as_str())
                .unwrap_or("promote");

            // Map disposition to kind: evaluation for judgment, proposal for shaping
            let kind = match disposition {
                "reject" | "refine" => "evaluation",
                _ => "proposal",
            };

            send_intent(base_url, kind, "leader", summary_text, priority)
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn send_intent(
    base_url: &str,
    kind: &str,
    audience: &str,
    summary_text: &str,
    priority: &str,
) -> Result<Value, String> {
    let payload = json!({
        "kind": kind,
        "audience": audience,
        "payload": { "summary_text": summary_text },
        "priority": priority,
    });

    let idem_key = http::generate_idempotency_key();
    let body = http::post(
        base_url,
        "/v1/intents",
        &payload.to_string(),
        Some(&idem_key),
    )?;
    Ok(json!({
        "content": [{"type": "text", "text": body}]
    }))
}
