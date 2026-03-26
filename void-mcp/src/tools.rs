use serde_json::{json, Value};

use crate::http;

pub fn tool_list() -> Value {
    json!({
        "tools": [
            {
                "name": "get_context",
                "description": "Get the execution context (identity, role, run metadata) from the sidecar.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "read_inbox",
                "description": "Read messages from the sidecar inbox. Optionally pass 'since' to only get messages after that sequence number.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "since": {
                            "type": "integer",
                            "description": "Only return messages with sequence number greater than this value."
                        }
                    },
                    "required": []
                }
            },
            {
                "name": "send_message",
                "description": "Send a message (intent) through the sidecar to other agents or the leader.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["proposal", "signal", "evaluation"],
                            "description": "The type of intent to send."
                        },
                        "audience": {
                            "type": "string",
                            "enum": ["broadcast", "leader"],
                            "description": "Who should receive this message."
                        },
                        "summary_text": {
                            "type": "string",
                            "description": "The message content."
                        },
                        "priority": {
                            "type": "string",
                            "enum": ["high", "normal", "low"],
                            "description": "Message priority (defaults to normal)."
                        }
                    },
                    "required": ["kind", "audience", "summary_text"]
                }
            }
        ]
    })
}

pub fn handle_call(base_url: &str, name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "get_context" => {
            let body = http::get(base_url, "/v1/context")?;
            Ok(json!({
                "content": [{"type": "text", "text": body}]
            }))
        }
        "read_inbox" => {
            let path = match arguments.get("since").and_then(|v| v.as_i64()) {
                Some(since) => format!("/v1/inbox?since={since}"),
                None => "/v1/inbox".to_string(),
            };
            let body = http::get(base_url, &path)?;
            Ok(json!({
                "content": [{"type": "text", "text": body}]
            }))
        }
        "send_message" => {
            let kind = arguments
                .get("kind")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: kind")?;
            let audience = arguments
                .get("audience")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: audience")?;
            let summary_text = arguments
                .get("summary_text")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: summary_text")?;
            let priority = arguments
                .get("priority")
                .and_then(|v| v.as_str())
                .unwrap_or("normal");

            // Validate enums
            match kind {
                "proposal" | "signal" | "evaluation" => {}
                _ => return Err(format!("invalid kind: {kind}")),
            }
            match audience {
                "broadcast" | "leader" => {}
                _ => return Err(format!("invalid audience: {audience}")),
            }
            match priority {
                "high" | "normal" | "low" => {}
                _ => return Err(format!("invalid priority: {priority}")),
            }

            let payload = serde_json::json!({
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
        _ => Err(format!("unknown tool: {name}")),
    }
}
