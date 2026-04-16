//! void-message: In-guest CLI for void-box sidecar messaging.
//!
//! Wraps the sidecar HTTP API with simple subcommands. Reads
//! VOID_SIDECAR_URL from environment to locate the sidecar.
//!
//! Usage:
//!   void-message context
//!   void-message inbox [--since N]
//!   void-message send --kind KIND --audience AUDIENCE --summary TEXT [--priority PRI]
//!   void-message health

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
        process::exit(1);
    }

    let base_url = match env::var("VOID_SIDECAR_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("error: VOID_SIDECAR_URL not set");
            process::exit(1);
        }
    };

    let result = match args[1].as_str() {
        "context" => cmd_context(&base_url),
        "inbox" => cmd_inbox(&base_url, &args[2..]),
        "send" => cmd_send(&base_url, &args[2..]),
        "health" => cmd_health(&base_url),
        "--help" | "-h" | "help" => {
            usage();
            Ok(())
        }
        other => {
            eprintln!("error: unknown command '{other}'");
            usage();
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn usage() {
    eprintln!("Usage: void-message <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  context                          Show execution identity");
    eprintln!("  inbox [--since N]                Read inbox messages");
    eprintln!("  send --kind K --audience A --summary S [--priority P]");
    eprintln!("                                   Send an intent");
    eprintln!("  health                           Check sidecar health");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  VOID_SIDECAR_URL  Sidecar base URL (set automatically)");
}

fn cmd_context(base_url: &str) -> Result<(), String> {
    let body = http_get(base_url, "/v1/context")?;
    println!("{body}");
    Ok(())
}

fn cmd_inbox(base_url: &str, args: &[String]) -> Result<(), String> {
    let mut since: Option<u64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--since" => {
                i += 1;
                since = Some(
                    args.get(i)
                        .ok_or("--since requires a value")?
                        .parse()
                        .map_err(|_| "--since must be a number")?,
                );
            }
            other => return Err(format!("unknown option: {other}")),
        }
        i += 1;
    }

    let path = match since {
        Some(v) => format!("/v1/inbox?since={v}"),
        None => "/v1/inbox".to_string(),
    };
    let body = http_get(base_url, &path)?;
    println!("{body}");
    Ok(())
}

fn cmd_send(base_url: &str, args: &[String]) -> Result<(), String> {
    let mut kind: Option<String> = None;
    let mut audience: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut priority = "normal".to_string();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" => {
                i += 1;
                let v = args.get(i).ok_or("--kind requires a value")?;
                match v.as_str() {
                    "proposal" | "signal" | "evaluation" => kind = Some(v.clone()),
                    _ => {
                        return Err(format!(
                            "invalid kind: {v} (use proposal, signal, or evaluation)"
                        ))
                    }
                }
            }
            "--audience" => {
                i += 1;
                let v = args.get(i).ok_or("--audience requires a value")?;
                match v.as_str() {
                    "broadcast" | "leader" => audience = Some(v.clone()),
                    _ => return Err(format!("invalid audience: {v} (use broadcast or leader)")),
                }
            }
            "--summary" => {
                i += 1;
                summary = Some(args.get(i).ok_or("--summary requires a value")?.clone());
            }
            "--priority" => {
                i += 1;
                let v = args.get(i).ok_or("--priority requires a value")?;
                match v.as_str() {
                    "high" | "normal" | "low" => priority = v.clone(),
                    _ => return Err(format!("invalid priority: {v} (use high, normal, or low)")),
                }
            }
            other => return Err(format!("unknown option: {other}")),
        }
        i += 1;
    }

    let kind = kind.ok_or("--kind is required")?;
    let audience = audience.ok_or("--audience is required")?;
    let summary = summary.ok_or("--summary is required")?;

    let payload = serde_json::json!({
        "kind": kind,
        "audience": audience,
        "payload": { "summary_text": summary },
        "priority": priority,
    });

    let idem_key = generate_idempotency_key();
    let body = http_post(
        base_url,
        "/v1/intents",
        &payload.to_string(),
        Some(&idem_key),
    )?;
    println!("{body}");
    Ok(())
}

fn cmd_health(base_url: &str) -> Result<(), String> {
    let body = http_get(base_url, "/v1/health")?;
    println!("{body}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal HTTP client (no external deps, raw TCP)
// ---------------------------------------------------------------------------

fn parse_host_port(base_url: &str) -> Result<(String, u16), String> {
    let url = base_url
        .strip_prefix("http://")
        .ok_or("VOID_SIDECAR_URL must start with http://")?;
    let (host, port_str) = url
        .rsplit_once(':')
        .ok_or("VOID_SIDECAR_URL must include port")?;
    let host = host.trim_end_matches('/');
    let port: u16 = port_str
        .trim_end_matches('/')
        .parse()
        .map_err(|_| "invalid port in VOID_SIDECAR_URL")?;
    Ok((host.to_string(), port))
}

fn http_get(base_url: &str, path: &str) -> Result<String, String> {
    let (host, port) = parse_host_port(base_url)?;
    let addr = format!("{host}:{port}");

    let mut stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("bad address: {e}"))?,
        Duration::from_secs(5),
    )
    .map_err(|e| format!("connect failed: {e}"))?;

    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("set timeout: {e}"))?;

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read: {e}"))?;

    parse_response(&response)
}

fn http_post(
    base_url: &str,
    path: &str,
    body: &str,
    idempotency_key: Option<&str>,
) -> Result<String, String> {
    let (host, port) = parse_host_port(base_url)?;
    let addr = format!("{host}:{port}");

    let mut stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("bad address: {e}"))?,
        Duration::from_secs(5),
    )
    .map_err(|e| format!("connect failed: {e}"))?;

    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("set timeout: {e}"))?;

    let idem_header = idempotency_key
        .map(|k| format!("Idempotency-Key: {k}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n{idem_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("read: {e}"))?;

    parse_response(&response)
}

fn parse_response(raw: &str) -> Result<String, String> {
    let (headers, body) = raw
        .split_once("\r\n\r\n")
        .ok_or("malformed HTTP response")?;

    let status_line = headers.lines().next().unwrap_or("");
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if status_code >= 400 {
        return Err(format!("HTTP {status_code}: {body}"));
    }

    Ok(body.to_string())
}

fn generate_idempotency_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", nanos)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_valid() {
        let (host, port) = parse_host_port("http://192.0.2.10:8090").unwrap();
        assert_eq!(host, "192.0.2.10");
        assert_eq!(port, 8090);
    }

    #[test]
    fn parse_host_port_with_trailing_slash() {
        let (host, port) = parse_host_port("http://192.0.2.10:8090/").unwrap();
        assert_eq!(host, "192.0.2.10");
        assert_eq!(port, 8090);
    }

    #[test]
    fn parse_host_port_localhost() {
        let (host, port) = parse_host_port("http://127.0.0.1:0").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 0);
    }

    #[test]
    fn parse_host_port_missing_scheme() {
        assert!(parse_host_port("192.0.2.10:8090").is_err());
    }

    #[test]
    fn parse_host_port_missing_port() {
        assert!(parse_host_port("http://192.0.2.10").is_err());
    }

    #[test]
    fn parse_response_200() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"ok\"}";
        let body = parse_response(raw).unwrap();
        assert_eq!(body, "{\"status\":\"ok\"}");
    }

    #[test]
    fn parse_response_404() {
        let raw = "HTTP/1.1 404 Not Found\r\n\r\n{\"code\":\"NOT_FOUND\"}";
        let result = parse_response(raw);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("404"));
    }

    #[test]
    fn parse_response_malformed() {
        assert!(parse_response("garbage").is_err());
    }

    #[test]
    fn idempotency_key_not_empty() {
        let key = generate_idempotency_key();
        assert!(!key.is_empty());
        assert_eq!(key.len(), 32); // 128 bits hex
    }

    #[test]
    fn idempotency_keys_are_unique() {
        let k1 = generate_idempotency_key();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let k2 = generate_idempotency_key();
        assert_ne!(k1, k2);
    }
}
