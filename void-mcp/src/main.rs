//! void-mcp: MCP stdio server for void-box sidecar messaging.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout with Content-Length header framing
//! (the same wire format as LSP). Claude Code spawns this as a subprocess
//! and discovers tools automatically via the MCP initialize handshake.

mod http;
mod jsonrpc;
mod tools;

use std::env;
use std::io::{self, BufRead, Read, Write};
use std::process;

use jsonrpc::{Request, Response, INVALID_PARAMS, METHOD_NOT_FOUND};
use serde_json::json;

fn main() {
    let base_url = match env::var("VOID_SIDECAR_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("error: VOID_SIDECAR_URL not set");
            process::exit(1);
        }
    };

    let stdin = io::stdin();
    let mut reader = io::BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    while let Some(content_length) = read_content_length(&mut reader) {
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            break;
        }

        let request: Request = match serde_json::from_slice(&body) {
            Ok(req) => req,
            Err(e) => {
                let resp = Response::error(None, -32700, format!("Parse error: {e}"));
                write_response(&mut writer, &resp);
                continue;
            }
        };

        let response = handle_request(&base_url, &request);

        // Notifications (no id) get no response
        if let Some(resp) = response {
            write_response(&mut writer, &resp);
        }
    }
}

fn handle_request(base_url: &str, req: &Request) -> Option<Response> {
    // Notifications have no id and expect no response
    req.id.as_ref()?;

    let id = req.id.clone();

    let resp = match req.method.as_str() {
        "initialize" => Response::success(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "void-mcp",
                    "version": "0.1.0"
                }
            }),
        ),
        "tools/list" => Response::success(id, tools::tool_list()),
        "tools/call" => {
            let params = req.params.as_ref();
            let name = params.and_then(|p| p.get("name")).and_then(|v| v.as_str());
            let name = match name {
                Some(n) => n,
                None => {
                    return Some(Response::error(
                        id,
                        INVALID_PARAMS,
                        "missing 'name' in tools/call params",
                    ))
                }
            };
            let arguments = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(json!({}));

            match tools::handle_call(base_url, name, &arguments) {
                Ok(result) => Response::success(id, result),
                Err(e) => Response::success(
                    id,
                    json!({
                        "content": [{"type": "text", "text": format!("Error: {e}")}],
                        "isError": true
                    }),
                ),
            }
        }
        _ => Response::error(
            id,
            METHOD_NOT_FOUND,
            format!("Method not found: {}", req.method),
        ),
    };

    Some(resp)
}

fn read_content_length(reader: &mut impl BufRead) -> Option<usize> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return None, // EOF
            Ok(_) => {}
            Err(_) => return None,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // End of headers
            return content_length;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            if let Ok(len) = value.trim().parse::<usize>() {
                content_length = Some(len);
            }
        }
    }
}

fn write_response(writer: &mut impl Write, response: &Response) {
    let body = serde_json::to_string(response).expect("failed to serialize response");
    let _ = write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::Cursor;

    fn make_request(method: &str, id: Option<Value>, params: Option<Value>) -> Request {
        Request {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }

    fn frame_message(json_str: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", json_str.len(), json_str).into_bytes()
    }

    #[test]
    fn initialize_returns_capabilities() {
        let req = make_request("initialize", Some(json!(1)), None);
        let resp = handle_request("http://127.0.0.1:9999", &req).unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert!(result["capabilities"]["tools"].is_object());
        assert_eq!(result["serverInfo"]["name"], "void-mcp");
        assert_eq!(result["serverInfo"]["version"], "0.1.0");
    }

    #[test]
    fn tools_list_returns_three_tools() {
        let req = make_request("tools/list", Some(json!(2)), None);
        let resp = handle_request("http://127.0.0.1:9999", &req).unwrap();
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"get_context"));
        assert!(names.contains(&"read_inbox"));
        assert!(names.contains(&"send_message"));
    }

    #[test]
    fn unknown_method_returns_error() {
        let req = make_request("bogus/method", Some(json!(3)), None);
        let resp = handle_request("http://127.0.0.1:9999", &req).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.as_ref().unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn tools_call_missing_name_returns_error() {
        let req = make_request("tools/call", Some(json!(4)), Some(json!({})));
        let resp = handle_request("http://127.0.0.1:9999", &req).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.as_ref().unwrap().code, INVALID_PARAMS);
    }

    #[test]
    fn notification_returns_no_response() {
        // Notifications have no id
        let req = make_request("notifications/initialized", None, None);
        let resp = handle_request("http://127.0.0.1:9999", &req);
        assert!(resp.is_none());
    }

    #[test]
    fn read_content_length_parses_header() {
        let input = b"Content-Length: 42\r\n\r\n";
        let mut reader = Cursor::new(input);
        assert_eq!(read_content_length(&mut reader), Some(42));
    }

    #[test]
    fn read_content_length_returns_none_on_eof() {
        let input = b"";
        let mut reader = Cursor::new(input);
        assert_eq!(read_content_length(&mut reader), None);
    }

    #[test]
    fn write_response_uses_content_length_framing() {
        let resp = Response::success(Some(json!(1)), json!({"ok": true}));
        let mut buf = Vec::new();
        write_response(&mut buf, &resp);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.starts_with("Content-Length: "));
        assert!(output.contains("\r\n\r\n"));
        // Verify the body after the separator is valid JSON
        let body = output.split("\r\n\r\n").nth(1).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
    }

    #[test]
    fn full_round_trip() {
        let req_json = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        })
        .to_string();

        let input = frame_message(&req_json);
        let mut reader = io::BufReader::new(Cursor::new(input));

        let len = read_content_length(&mut reader).unwrap();
        assert_eq!(len, req_json.len());

        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).unwrap();

        let request: Request = serde_json::from_slice(&body).unwrap();
        let response = handle_request("http://127.0.0.1:9999", &request).unwrap();
        assert!(response.result.is_some());
        assert_eq!(response.result.unwrap()["serverInfo"]["name"], "void-mcp");
    }
}
