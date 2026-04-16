use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub fn parse_host_port(base_url: &str) -> Result<(String, u16), String> {
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

pub fn get(base_url: &str, path: &str) -> Result<String, String> {
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

    parse_body(&response)
}

pub fn post(
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

    parse_body(&response)
}

pub fn generate_idempotency_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", nanos)
}

fn parse_body(raw: &str) -> Result<String, String> {
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
    fn parse_body_200() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}";
        assert_eq!(parse_body(raw).unwrap(), "{\"ok\":true}");
    }

    #[test]
    fn parse_body_404() {
        let raw = "HTTP/1.1 404 Not Found\r\n\r\nnot found";
        assert!(parse_body(raw).unwrap_err().contains("404"));
    }

    #[test]
    fn parse_body_malformed() {
        assert!(parse_body("garbage").is_err());
    }

    #[test]
    fn idempotency_key_format() {
        let key = generate_idempotency_key();
        assert_eq!(key.len(), 32);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
