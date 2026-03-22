use std::path::Path;

/// Errors from CLI backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    Local(String),
    #[error("daemon unreachable at {url}: {detail}")]
    DaemonUnreachable { url: String, detail: String },
    #[error("daemon error: {0}")]
    DaemonError(String),
}

/// Result of a `run` operation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunResult {
    pub name: String,
    pub kind: String,
    pub success: bool,
    pub output: String,
    pub stages: usize,
    pub total_cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl std::fmt::Display for RunResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "name: {}", self.name)?;
        writeln!(f, "kind: {}", self.kind)?;
        writeln!(f, "success: {}", self.success)?;
        writeln!(f, "stages: {}", self.stages)?;
        writeln!(f, "cost_usd: {:.6}", self.total_cost_usd)?;
        writeln!(
            f,
            "tokens: {} in / {} out",
            self.input_tokens, self.output_tokens
        )?;
        write!(f, "output:\n{}", self.output)
    }
}

/// In-process execution: runs specs via `run_file` directly.
pub struct LocalBackend;

impl LocalBackend {
    pub async fn run(
        file: &Path,
        input: Option<String>,
    ) -> std::result::Result<RunResult, BackendError> {
        let report = void_box::runtime::run_file(file, input, None, None, None, None, None)
            .await
            .map_err(|e| BackendError::Local(e.to_string()))?;

        Ok(RunResult {
            name: report.name,
            kind: report.kind,
            success: report.success,
            output: report.output,
            stages: report.stages,
            total_cost_usd: report.total_cost_usd,
            input_tokens: report.input_tokens,
            output_tokens: report.output_tokens,
        })
    }
}

/// HTTP client for the daemon: all remote CLI access goes through here.
pub struct RemoteBackend {
    pub daemon_url: String,
    client: reqwest::Client,
}

impl RemoteBackend {
    pub fn new(daemon_url: String) -> Self {
        Self {
            daemon_url,
            client: reqwest::Client::new(),
        }
    }

    fn base_url(&self) -> &str {
        self.daemon_url.trim_end_matches('/')
    }

    async fn get_text(&self, url: &str) -> Result<String, BackendError> {
        self.client
            .get(url)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable {
                url: url.to_string(),
                detail: e.to_string(),
            })?
            .text()
            .await
            .map_err(|e| BackendError::DaemonError(e.to_string()))
    }

    /// Parse daemon JSON response bodies; empty body → `null`.
    fn parse_json_body(body: &str) -> Result<serde_json::Value, BackendError> {
        let t = body.trim();
        if t.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(t).map_err(|e| BackendError::DaemonError(format!("invalid JSON: {e}")))
    }

    /// `GET /v1/runs/{run_id}/events` — run log stream.
    pub async fn logs(&self, run_id: &str) -> Result<serde_json::Value, BackendError> {
        let url = format!("{}/v1/runs/{}/events", self.base_url(), run_id);
        let body = self.get_text(&url).await?;
        Self::parse_json_body(&body)
    }

    /// `GET /v1/runs/{run_id}` — run status.
    pub async fn status(&self, run_id: &str) -> Result<serde_json::Value, BackendError> {
        let url = format!("{}/v1/runs/{}", self.base_url(), run_id);
        let body = self.get_text(&url).await?;
        Self::parse_json_body(&body)
    }

    /// `POST /v1/runs` — start a remote run; returns `run_id`.
    pub async fn create_run(
        &self,
        file: &str,
        input: Option<String>,
    ) -> Result<String, BackendError> {
        let url = format!("{}/v1/runs", self.base_url());
        let body = serde_json::json!({ "file": file, "input": input }).to_string();
        let text = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable {
                url: url.clone(),
                detail: e.to_string(),
            })?
            .text()
            .await
            .map_err(|e| BackendError::DaemonError(e.to_string()))?;

        let value = serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| BackendError::DaemonError(format!("invalid JSON from daemon: {e}")))?;
        let run_id = value
            .get("run_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| BackendError::DaemonError("missing run_id in daemon response".into()))?;
        Ok(run_id.to_string())
    }

    /// `POST /v1/sessions/{session_id}/messages` — append a chat message for the TUI session.
    pub async fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<(), BackendError> {
        let url = format!("{}/v1/sessions/{}/messages", self.base_url(), session_id);
        let body = serde_json::json!({ "role": role, "content": content }).to_string();
        self.client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable {
                url: url.clone(),
                detail: e.to_string(),
            })?;
        Ok(())
    }

    /// `GET /v1/sessions/{session_id}/messages` — session message history.
    pub async fn get_messages(&self, session_id: &str) -> Result<serde_json::Value, BackendError> {
        let url = format!("{}/v1/sessions/{}/messages", self.base_url(), session_id);
        let body = self.get_text(&url).await?;
        Self::parse_json_body(&body)
    }

    /// `POST /v1/runs/{run_id}/cancel` — cancel a run.
    pub async fn cancel_run(&self, run_id: &str) -> Result<serde_json::Value, BackendError> {
        let url = format!("{}/v1/runs/{}/cancel", self.base_url(), run_id);
        let text = self
            .client
            .post(&url)
            .body("{}")
            .send()
            .await
            .map_err(|e| BackendError::DaemonUnreachable {
                url: url.clone(),
                detail: e.to_string(),
            })?
            .text()
            .await
            .map_err(|e| BackendError::DaemonError(e.to_string()))?;
        Self::parse_json_body(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    // -----------------------------
    // RunResult display
    // -----------------------------

    #[test]
    fn run_result_display_format() {
        let r = RunResult {
            name: "test".into(),
            kind: "agent".into(),
            success: true,
            output: "hello".into(),
            stages: 2,
            total_cost_usd: 0.123456,
            input_tokens: 10,
            output_tokens: 20,
        };

        let s = format!("{r}");
        assert!(s.contains("name: test"));
        assert!(s.contains("kind: agent"));
        assert!(s.contains("success: true"));
        assert!(s.contains("stages: 2"));
        assert!(s.contains("cost_usd: 0.123456"));
        assert!(s.contains("tokens: 10 in / 20 out"));
        assert!(s.contains("output:\nhello"));
    }

    // -----------------------------
    // base_url normalization
    // -----------------------------

    #[test]
    fn base_url_trims_trailing_slash() {
        let b = RemoteBackend::new("http://localhost:1234/".into());
        assert_eq!(b.base_url(), "http://localhost:1234");
    }

    // -----------------------------
    // parse_json_body
    // -----------------------------

    #[test]
    fn parse_json_body_empty_returns_null() {
        let v = RemoteBackend::parse_json_body("").unwrap();
        assert_eq!(v, serde_json::Value::Null);
    }

    #[test]
    fn parse_json_body_valid_json() {
        let v = RemoteBackend::parse_json_body(r#"{"a":1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn parse_json_body_invalid_json_errors() {
        let err = RemoteBackend::parse_json_body("not-json").unwrap_err();
        match err {
            BackendError::DaemonError(msg) => {
                assert!(msg.contains("invalid JSON"));
            }
            _ => panic!("expected DaemonError"),
        }
    }

    // -----------------------------
    // HTTP: status
    // -----------------------------

    #[tokio::test]
    async fn status_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(GET).path("/v1/runs/run-1");
            then.status(200).body(r#"{"status":"ok"}"#);
        });

        let backend = RemoteBackend::new(server.base_url());
        let result = backend.status("run-1").await.unwrap();

        assert_eq!(result["status"], "ok");
        mock.assert();
    }

    #[tokio::test]
    async fn status_empty_body_returns_null() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/v1/runs/run-1");
            then.status(200).body("");
        });

        let backend = RemoteBackend::new(server.base_url());
        let result = backend.status("run-1").await.unwrap();

        assert_eq!(result, serde_json::Value::Null);
    }

    // -----------------------------
    // HTTP: logs
    // -----------------------------

    #[tokio::test]
    async fn logs_success() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/v1/runs/run-1/events");
            then.status(200).body(r#"{"events":[]}"#);
        });

        let backend = RemoteBackend::new(server.base_url());
        let result = backend.logs("run-1").await.unwrap();

        assert!(result["events"].is_array());
    }

    // -----------------------------
    // create_run
    // -----------------------------

    #[tokio::test]
    async fn create_run_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/runs")
                .header("content-type", "application/json");
            then.status(200).body(r#"{"run_id":"abc123"}"#);
        });

        let backend = RemoteBackend::new(server.base_url());
        let run_id = backend.create_run("file.yaml", None).await.unwrap();

        assert_eq!(run_id, "abc123");
        mock.assert();
    }

    #[tokio::test]
    async fn create_run_missing_run_id_errors() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(POST).path("/v1/runs");
            then.status(200).body(r#"{}"#);
        });

        let backend = RemoteBackend::new(server.base_url());
        let err = backend.create_run("file.yaml", None).await.unwrap_err();

        match err {
            BackendError::DaemonError(msg) => {
                assert!(msg.contains("missing run_id"));
            }
            _ => panic!("expected DaemonError"),
        }
    }

    #[tokio::test]
    async fn create_run_invalid_json_errors() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(POST).path("/v1/runs");
            then.status(200).body("not-json");
        });

        let backend = RemoteBackend::new(server.base_url());
        let err = backend.create_run("file.yaml", None).await.unwrap_err();

        match err {
            BackendError::DaemonError(msg) => {
                assert!(msg.contains("invalid JSON"));
            }
            _ => panic!("expected DaemonError"),
        }
    }

    // -----------------------------
    // network failure
    // -----------------------------

    #[tokio::test]
    async fn unreachable_daemon_returns_error() {
        let backend = RemoteBackend::new("http://127.0.0.1:59999".into());

        let err = backend.status("run-1").await.unwrap_err();

        match err {
            BackendError::DaemonUnreachable { .. } => {}
            _ => panic!("expected DaemonUnreachable"),
        }
    }

    // -----------------------------
    // cancel_run
    // -----------------------------

    #[tokio::test]
    async fn cancel_run_success() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(POST).path("/v1/runs/run-1/cancel");
            then.status(200).body(r#"{"ok":true}"#);
        });

        let backend = RemoteBackend::new(server.base_url());
        let result = backend.cancel_run("run-1").await.unwrap();

        assert_eq!(result["ok"], true);
    }

    // -----------------------------
    // append_message
    // -----------------------------

    #[tokio::test]
    async fn append_message_success() {
        let server = MockServer::start();

        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/sessions/s1/messages");
            then.status(200);
        });

        let backend = RemoteBackend::new(server.base_url());
        backend.append_message("s1", "user", "hello").await.unwrap();

        mock.assert();
    }

    // -----------------------------
    // get_messages
    // -----------------------------

    #[tokio::test]
    async fn get_messages_success() {
        let server = MockServer::start();

        server.mock(|when, then| {
            when.method(GET).path("/v1/sessions/s1/messages");
            then.status(200).body(r#"{"messages":[]}"#);
        });

        let backend = RemoteBackend::new(server.base_url());
        let result = backend.get_messages("s1").await.unwrap();

        assert!(result["messages"].is_array());
    }
}
