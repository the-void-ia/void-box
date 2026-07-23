//! Host-side custody of per-agent credentials, in two forms.
//!
//! **Staging into the guest** (the mount path). Locates credentials on the host
//! (macOS Keychain or Linux filesystem), stages them into a secure temp
//! directory, and exposes the host path so the runtime can mount them into the
//! guest VM at the agent's expected `$HOME` config directory:
//!
//! - **Claude personal**: `~/.claude/.credentials.json` (Linux) or macOS
//!   Keychain `Claude Code-credentials` entry. Mounted into the guest at
//!   `/home/sandbox/.claude`.
//! - **Codex**: `~/.codex/auth.json` (both Linux and macOS — codex stores
//!   plain JSON, no Keychain integration). Mounted into the guest at
//!   `/home/sandbox/.codex`. Supports both `auth_mode: "chatgpt"` (cached
//!   OAuth bearer from `codex login`) and `auth_mode: "api_key"`.
//!
//! Cleanup is automatic: [`StagedCredentials`] wraps a [`tempfile::TempDir`]
//! whose `Drop` impl removes the directory when the value goes out of scope.
//!
//! **Host-side custody without staging** ([`ClaudeOAuthStore`]). When the
//! credential proxy is active, the durable Claude OAuth refresh token must stay
//! on the host and never reach the guest. The store holds it in host memory,
//! refreshes it against Anthropic's token endpoint to mint short-lived access
//! tokens the proxy injects, and writes the rotated refresh token back to the
//! host credential file. The guest gets a placeholder, not the secret.

use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::CONTENT_TYPE;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{Error, Result};

/// Staged credentials ready to be mounted into the guest VM.
///
/// The underlying temp directory is removed when this value is dropped.
pub struct StagedCredentials {
    /// Kept alive for its `Drop` impl (auto-cleanup).
    _dir: tempfile::TempDir,
    /// Absolute host path to the staged directory.
    pub host_path: String,
}

/// Discover OAuth credentials from the host system.
///
/// - **macOS**: reads from the system Keychain via `security find-generic-password`.
/// - **Linux**: reads `~/.claude/.credentials.json`.
///
/// Returns the raw JSON wrapped in [`SecretString`] on success, or a
/// user-friendly error directing the user to run `claude auth login`.
pub fn discover_oauth_credentials() -> Result<SecretString> {
    #[cfg(target_os = "macos")]
    {
        discover_macos()
    }
    #[cfg(target_os = "linux")]
    {
        discover_linux()
    }
}

#[cfg(target_os = "macos")]
fn discover_macos() -> Result<SecretString> {
    let user = std::env::var("USER").map_err(|_| {
        Error::Config("cannot determine current user (USER env var not set)".into())
    })?;

    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-a",
            &user,
            "-w",
        ])
        .output()
        .map_err(|e| Error::Config(format!("failed to run 'security' command: {e}")))?;

    if !output.status.success() {
        return Err(Error::Config(
            "Claude personal plan not authenticated \u{2014} \
             run 'claude auth login' first, then retry."
                .into(),
        ));
    }

    let json = String::from_utf8(output.stdout)
        .map_err(|_| Error::Config("credentials contain invalid UTF-8".into()))?
        .trim()
        .to_string();

    validate_credentials_json(&json)?;
    Ok(SecretString::from(json))
}

#[cfg(target_os = "linux")]
fn discover_linux() -> Result<SecretString> {
    let home = std::env::var("HOME").map_err(|_| {
        Error::Config("HOME not set; cannot locate ~/.claude/.credentials.json".into())
    })?;
    let path = std::path::Path::new(&home).join(".claude/.credentials.json");

    let json = fs::read_to_string(&path).map_err(|_| {
        Error::Config(format!(
            "Claude personal plan not authenticated \u{2014} \
             credentials not found at {}. Run 'claude auth login' first, then retry.",
            path.display()
        ))
    })?;

    validate_credentials_json(&json)?;
    Ok(SecretString::from(json))
}

/// Light validation: parse as JSON and check for the expected top-level key.
fn validate_credentials_json(json: &str) -> Result<()> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|_| Error::Config("credentials file is not valid JSON".into()))?;
    if v.get("claudeAiOauth").is_none() {
        return Err(Error::Config(
            "credentials file missing 'claudeAiOauth' key \u{2014} \
             re-run 'claude auth login' to refresh."
                .into(),
        ));
    }
    Ok(())
}

/// Stage credentials into a secure temp directory.
///
/// Creates a temp directory with a `.credentials.json` file (0600 permissions).
/// The returned [`StagedCredentials`] holds the directory alive; dropping it
/// removes the staged files.
pub fn stage_credentials(creds_json: &SecretString) -> Result<StagedCredentials> {
    let dir = tempfile::Builder::new()
        .prefix("voidbox-claude-creds-")
        .tempdir()
        .map_err(|e| Error::Config(format!("failed to create temp dir for credentials: {e}")))?;

    let creds_path = dir.path().join(".credentials.json");
    // expose: writing the OAuth JSON to the staged 0600 file.
    fs::write(&creds_path, creds_json.expose_secret())
        .map_err(|e| Error::Config(format!("failed to write credentials file: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&creds_path, fs::Permissions::from_mode(0o600))
            .map_err(|e| Error::Config(format!("failed to set credentials permissions: {e}")))?;
    }

    let host_path = dir.path().to_string_lossy().into_owned();
    Ok(StagedCredentials {
        _dir: dir,
        host_path,
    })
}

// ---------------------------------------------------------------------------
// Codex credentials
// ---------------------------------------------------------------------------

/// Discover Codex credentials from the host.
///
/// Codex stores credentials as plain JSON at `~/.codex/auth.json` on both
/// Linux and macOS — there is no Keychain integration. The file contains
/// the OAuth tokens (when authenticated via `codex login`) and/or a stored
/// API key. Returns the raw JSON wrapped in [`SecretString`] on success.
pub fn discover_codex_credentials() -> Result<SecretString> {
    let home = std::env::var("HOME")
        .map_err(|_| Error::Config("HOME not set; cannot locate ~/.codex/auth.json".into()))?;
    let path = std::path::Path::new(&home).join(".codex/auth.json");

    let json = fs::read_to_string(&path).map_err(|_| {
        Error::Config(format!(
            "Codex credentials not found at {} \u{2014} run 'codex login' first, then retry. \
             (Or set OPENAI_API_KEY for API-key auth without the host file mount.)",
            path.display()
        ))
    })?;

    validate_codex_credentials_json(&json)?;
    Ok(SecretString::from(json))
}

/// Light validation: parse as JSON and check for the expected `auth_mode` key.
fn validate_codex_credentials_json(json: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|_| Error::Config("codex auth.json is not valid JSON".into()))?;
    if value.get("auth_mode").is_none() {
        return Err(Error::Config(
            "codex auth.json missing 'auth_mode' key \u{2014} \
             re-run 'codex login' to refresh."
                .into(),
        ));
    }
    Ok(())
}

/// Stage codex credentials into a secure temp directory.
///
/// Creates a temp directory containing `auth.json` (0600 permissions). The
/// returned [`StagedCredentials`] holds the directory alive; dropping it
/// removes the staged file. Mount the temp dir at `/home/sandbox/.codex`
/// in the guest.
pub fn stage_codex_credentials(creds_json: &SecretString) -> Result<StagedCredentials> {
    let dir = tempfile::Builder::new()
        .prefix("voidbox-codex-creds-")
        .tempdir()
        .map_err(|e| Error::Config(format!("failed to create temp dir for codex creds: {e}")))?;

    let auth_path = dir.path().join("auth.json");
    // expose: writing the OAuth JSON to the staged 0600 file.
    fs::write(&auth_path, creds_json.expose_secret())
        .map_err(|e| Error::Config(format!("failed to write codex auth.json: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600)).map_err(|e| {
            Error::Config(format!("failed to set codex auth.json permissions: {e}"))
        })?;
    }

    let host_path = dir.path().to_string_lossy().into_owned();
    Ok(StagedCredentials {
        _dir: dir,
        host_path,
    })
}

// ---------------------------------------------------------------------------
// Claude OAuth credential store (host-side refresh / mint / rotation)
// ---------------------------------------------------------------------------

/// Anthropic OAuth token endpoint and the claude-code public OAuth client id.
///
/// These are the values the bundled claude-code uses for its own refresh flow.
/// They are provider-controlled and load-bearing, so they must be re-verified in
/// the V2 OAuth-acceptance validation (RFC-0002 rollout) and on every claude-code
/// version bump before this path is relied on against a real subscription.
const ANTHROPIC_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Mint a fresh access token this far before the stored expiry, so a request in
/// flight never races the expiry boundary.
const ACCESS_TOKEN_SKEW: Duration = Duration::from_secs(300);

/// Floor on the spacing between refresh attempts. A single-use refresh token must
/// not be spent in a tight loop, so when a refresh fails or a token is already
/// expired, callers fail closed until this interval elapses rather than hammering
/// the token endpoint. The store is the sole rotation owner.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Overall deadline for one refresh round-trip. Without it, a token endpoint that
/// accepts the connection but never responds would block the refresh (and every
/// caller waiting behind the state lock) forever — a hang, not the fail-closed a
/// dead endpoint must produce.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Connect deadline for the refresh round-trip (subset of [`REFRESH_TIMEOUT`]).
const REFRESH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on a minted access token's accepted lifetime. Caps a bogus or
/// hostile `expires_in` so the `SystemTime` addition cannot overflow and panic;
/// real access tokens live hours, so anything longer is treated as this maximum.
const MAX_ACCESS_TOKEN_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);

/// Basename of the sibling advisory-lock file that serializes the read-refresh-
/// write cycle across concurrent void-box runs sharing one credential file.
///
/// It coordinates void-box runs with each other only. It cannot coordinate with
/// the operator's own `claude-code`, which does not take this lock — a concurrent
/// refresh by that client remains an inherent, unguarded race on the shared
/// single-use refresh token.
const CREDENTIALS_LOCK_NAME: &str = ".voidbox-claude-credentials.lock";

/// Host-side custodian of a Claude personal-subscription OAuth credential.
///
/// Holds the durable refresh token in host memory only — never the guest —
/// refreshes it against Anthropic's token endpoint to mint short-lived access
/// tokens, and is the rotation owner across void-box runs: refreshes are
/// serialized within the process (the state mutex, coalescing concurrent
/// requests) and across processes (an advisory `flock` held over the whole
/// read-refresh-write cycle, so two runs sharing one credential file never
/// double-spend the single-use refresh token), and the rotated token is written
/// back atomically so subsequent runs stay valid. This does not extend to the
/// operator's own `claude-code`, which does not take the lock (see
/// [`CREDENTIALS_LOCK_NAME`]).
///
/// The injection proxy asks it for a currently-valid access token per request via
/// [`access_token`](ClaudeOAuthStore::access_token); the durable refresh token
/// never leaves this process. Secrets are held in [`SecretString`] (zeroized on
/// drop). `mlock` + `PR_SET_DUMPABLE=0` land with the out-of-process proxy
/// hardening, alongside the process split the M0 proxy also defers.
pub struct ClaudeOAuthStore {
    /// Host path of the durable credential file; the write-back target.
    creds_path: PathBuf,
    /// Token endpoint (the pinned Anthropic URL on the real path; overridable in
    /// tests via [`with_token_endpoint`](ClaudeOAuthStore::with_token_endpoint)).
    token_url: String,
    /// HTTP client for the token endpoint. On the real path it resolves through the
    /// SSRF guard and ignores any ambient `HTTPS_PROXY`, mirroring the proxy's
    /// upstream client; tests swap in a client pointed at a loopback mock.
    http: reqwest::Client,
    /// Serialized token state. A `tokio` mutex so a refresh (which awaits a
    /// network round-trip) holds the lock across the await, forcing concurrent
    /// callers to wait for and reuse the one refresh rather than each spending the
    /// single-use refresh token.
    state: Mutex<TokenState>,
}

/// The mutable half of a [`ClaudeOAuthStore`].
struct TokenState {
    /// Short-lived minted access token presented upstream as a Bearer.
    access_token: SecretString,
    /// Durable refresh token — the secret that must never reach the guest.
    refresh_token: SecretString,
    /// Absolute expiry of `access_token`.
    expires_at: SystemTime,
    /// Full credential-file JSON, preserved so write-back rewrites only the three
    /// token fields and never drops fields the client relies on.
    document: Value,
    /// When the last refresh was attempted, for the rate-cap.
    last_refresh_attempt: Option<Instant>,
}

/// One successful refresh grant's result.
struct RefreshedTokens {
    access_token: SecretString,
    /// Present only when the endpoint rotated the refresh token.
    refresh_token: Option<SecretString>,
    expires_in: Duration,
}

/// The subset of the OAuth token-endpoint response this path consumes.
#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

impl ClaudeOAuthStore {
    /// Build a store from the host's discovered Claude OAuth credential.
    ///
    /// Reads via [`discover_oauth_credentials`] (macOS Keychain or the Linux
    /// credential file) and targets the Linux credential file for write-back. The
    /// credential proxy runs only on Linux/KVM today (macOS/VZ is deferred to
    /// M1b), so a Keychain-sourced credential is never refreshed in practice.
    pub fn from_host() -> Result<Self> {
        let json = discover_oauth_credentials()?;
        let home = std::env::var("HOME").map_err(|_| {
            Error::Config("HOME not set; cannot locate ~/.claude/.credentials.json".into())
        })?;
        let creds_path = Path::new(&home).join(".claude/.credentials.json");
        Self::from_json(&json, creds_path)
    }

    /// Build a store from an explicit credential JSON and write-back path.
    /// Exposed so tests avoid touching the host's real credential file.
    pub fn from_json(creds_json: &SecretString, creds_path: PathBuf) -> Result<Self> {
        let state = TokenState::parse(creds_json.expose_secret())?;
        Ok(Self {
            creds_path,
            token_url: ANTHROPIC_TOKEN_URL.to_string(),
            http: build_token_client()?,
            state: Mutex::new(state),
        })
    }

    /// Point the store at a different token endpoint. A test/override seam
    /// (mirroring [`ProxyHandle::new`](crate::proxy::ProxyHandle::new)): the real
    /// path uses the pinned Anthropic URL, tests target a loopback mock.
    pub fn with_token_endpoint(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    /// Replace the HTTP client so a loopback mock is reachable without the SSRF
    /// guard rejecting it. A test/override seam; the real path uses the SSRF-guarded
    /// client from [`build_token_client`].
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http = client;
        self
    }

    /// Return a currently-valid access token, refreshing if the cached one has
    /// expired or is within [`ACCESS_TOKEN_SKEW`] of expiry. Serialized:
    /// concurrent callers wait on and reuse a single refresh.
    ///
    /// Fails closed (`Err`) rather than returning a stale token — the proxy turns
    /// that into a `502` so the agent never sends an unauthenticated upstream call.
    pub async fn access_token(&self) -> Result<SecretString> {
        let mut state = self.state.lock().await;
        let now = SystemTime::now();
        if state.valid_at(now + ACCESS_TOKEN_SKEW) {
            return Ok(state.access_token.clone());
        }
        // A refresh is due. Apply the rate-cap before spending the refresh token.
        if let Some(last) = state.last_refresh_attempt {
            if last.elapsed() < MIN_REFRESH_INTERVAL {
                // Within the skew window the current token is still usable, so keep
                // it rather than refresh again; if it is already expired, fail
                // closed instead of looping on the single-use refresh token.
                if state.valid_at(now) {
                    return Ok(state.access_token.clone());
                }
                return Err(Error::Network(
                    "Claude OAuth access token expired; refresh rate-capped, failing closed".into(),
                ));
            }
        }
        state.last_refresh_attempt = Some(Instant::now());
        self.refresh(&mut state).await?;
        Ok(state.access_token.clone())
    }

    /// Snapshot the durable refresh token for the no-credential-in-guest audit — the host-side
    /// "no durable secret in the guest" check. The value already lives in host
    /// memory; the audit only searches the staged env/files for it and drops it.
    pub async fn durable_secret_snapshot(&self) -> SecretString {
        self.state.lock().await.refresh_token.clone()
    }

    /// Best-effort prime so the first proxied request does not pay a refresh
    /// round-trip. Overlap this with VM boot and ignore failures — a real failure
    /// surfaces again (fail-closed) on the request itself.
    pub async fn warm_up(&self) {
        if let Err(e) = self.access_token().await {
            tracing::debug!("Claude OAuth warm-up did not mint a token: {e}");
        }
    }

    /// Refresh against the token endpoint and persist the rotated token. The
    /// caller holds the in-process state mutex; this additionally holds the
    /// cross-process `flock` across the whole read-refresh-write cycle, so a peer
    /// run that already rotated the token on disk is adopted rather than
    /// double-spending our cached (now-stale) refresh token.
    async fn refresh(&self, state: &mut TokenState) -> Result<()> {
        // Take the cross-process lock and read the current on-disk credential
        // under it. `spawn_blocking` keeps the blocking `flock` acquire off the
        // runtime; the guard owns the lock file, so it travels with us across the
        // await below and the lock stays held until the write completes.
        let path = self.creds_path.clone();
        let (guard, on_disk) = tokio::task::spawn_blocking(move || {
            let guard = FlockGuard::acquire(&path)?;
            Ok::<_, Error>((guard, read_on_disk(&path)))
        })
        .await
        .map_err(|e| Error::Config(format!("credential lock task join failed: {e}")))??;

        // If a peer run refreshed since we loaded, adopt its result: a still-valid
        // on-disk token means skip our refresh entirely (no double-spend);
        // otherwise adopt the on-disk refresh token + document, which may already
        // be one rotation ahead of the copy we hold.
        if let Some(disk) = on_disk {
            if disk.valid_at(SystemTime::now() + ACCESS_TOKEN_SKEW) {
                let attempt = state.last_refresh_attempt;
                *state = disk;
                state.last_refresh_attempt = attempt;
                return Ok(());
            }
            state.refresh_token = disk.refresh_token;
            state.document = disk.document;
        }

        let refresh_token = state.refresh_token.expose_secret().to_string();
        let refreshed = self.request_refresh(&refresh_token).await?;
        state.apply(refreshed);

        // Write back and release the lock, both on the blocking pool.
        let path = self.creds_path.clone();
        let document = state.document.clone();
        tokio::task::spawn_blocking(move || write_document(&path, &document, guard))
            .await
            .map_err(|e| Error::Config(format!("credential write-back task join failed: {e}")))??;
        Ok(())
    }

    /// POST the refresh grant and parse the minted tokens. Never logs the request
    /// or response body — either could echo a token.
    async fn request_refresh(&self, refresh_token: &str) -> Result<RefreshedTokens> {
        let request = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
        });
        let body = serde_json::to_vec(&request)
            .map_err(|e| Error::Config(format!("serialize OAuth refresh request: {e}")))?;

        let response = self
            .http
            .post(&self.token_url)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Network(format!("OAuth refresh request failed: {e}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| Error::Network(format!("reading OAuth refresh response: {e}")))?;
        if !status.is_success() {
            return Err(Error::Network(format!(
                "OAuth refresh rejected with HTTP {status}"
            )));
        }

        let parsed: RefreshResponse = serde_json::from_slice(&bytes).map_err(|_| {
            Error::Network("OAuth refresh response was not the expected JSON".into())
        })?;
        Ok(RefreshedTokens {
            access_token: SecretString::from(parsed.access_token),
            refresh_token: parsed.refresh_token.map(SecretString::from),
            expires_in: Duration::from_secs(parsed.expires_in),
        })
    }
}

impl TokenState {
    /// Parse the `claudeAiOauth` block, keeping the whole document for write-back.
    fn parse(json: &str) -> Result<Self> {
        let document: Value = serde_json::from_str(json)
            .map_err(|_| Error::Config("credentials file is not valid JSON".into()))?;
        let oauth = document
            .get("claudeAiOauth")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                Error::Config("credentials file missing 'claudeAiOauth' object".into())
            })?;
        let field = |name: &str| -> Result<&str> {
            oauth
                .get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::Config(format!("credentials missing '{name}'")))
        };
        let access_token = field("accessToken")?.to_string();
        let refresh_token = field("refreshToken")?.to_string();
        let expires_at_ms = oauth
            .get("expiresAt")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Config("credentials missing 'expiresAt'".into()))?;
        Ok(Self {
            access_token: SecretString::from(access_token),
            refresh_token: SecretString::from(refresh_token),
            expires_at: unix_ms_to_system_time(expires_at_ms),
            document,
            last_refresh_attempt: None,
        })
    }

    /// Whether the access token is still valid at `instant`.
    fn valid_at(&self, instant: SystemTime) -> bool {
        self.expires_at > instant
    }

    /// Fold a refresh result into the in-memory state and the preserved document.
    fn apply(&mut self, refreshed: RefreshedTokens) {
        self.access_token = refreshed.access_token;
        if let Some(rotated) = refreshed.refresh_token {
            self.refresh_token = rotated;
        }
        // Cap the lifetime so a bogus/hostile `expires_in` cannot overflow the
        // `SystemTime` addition (which panics); clamp to "now" if it somehow still
        // overflows, treating that as already-expired.
        let lifetime = refreshed.expires_in.min(MAX_ACCESS_TOKEN_LIFETIME);
        self.expires_at = SystemTime::now()
            .checked_add(lifetime)
            .unwrap_or_else(SystemTime::now);
        if let Some(oauth) = self
            .document
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
        {
            oauth.insert(
                "accessToken".into(),
                Value::String(self.access_token.expose_secret().to_string()),
            );
            oauth.insert(
                "refreshToken".into(),
                Value::String(self.refresh_token.expose_secret().to_string()),
            );
            oauth.insert(
                "expiresAt".into(),
                Value::Number(system_time_to_unix_ms(self.expires_at).into()),
            );
        }
    }
}

/// Build the token-endpoint client: no redirects (a credential must
/// not chase a redirect), no ambient proxy, and SSRF-guarded resolution so a
/// rebound token-endpoint name cannot steer the refresh at an internal target.
///
/// `SsrfGuardResolver` is a shared network primitive intentionally reused from
/// `proxy` (it also guards the proxy's upstream client), so this module depends on
/// `proxy` here while `proxy::injector` depends back on [`ClaudeOAuthStore`]. If
/// that mutual dependency grows, promote the resolver to a neutral shared module.
fn build_token_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(REFRESH_TIMEOUT)
        .connect_timeout(REFRESH_CONNECT_TIMEOUT)
        .dns_resolver(Arc::new(crate::proxy::ssrf::SsrfGuardResolver))
        .build()
        .map_err(|e| Error::Network(format!("OAuth token client build failed: {e}")))
}

/// Convert epoch-milliseconds to [`SystemTime`], clamping a negative value to the
/// epoch (treated as already-expired).
fn unix_ms_to_system_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH
    }
}

/// Convert [`SystemTime`] to epoch-milliseconds, clamping pre-epoch times to 0.
fn system_time_to_unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Read and parse the current on-disk credential, tolerating a missing or
/// malformed file (returns `None`) so a fresh mint proceeds from the in-memory
/// token rather than erroring.
fn read_on_disk(path: &Path) -> Option<TokenState> {
    let text = fs::read_to_string(path).ok()?;
    TokenState::parse(&text).ok()
}

/// Atomically write `document` to `path` (temp file + `rename`), then release the
/// held lock. `guard` must be the lock acquired for `path`; it is dropped here,
/// after the durable rename, so the lock spans the whole read-refresh-write cycle.
///
/// A non-atomic or raced write to a single-use refresh token risks account
/// lockout. The temp-file swap means a concurrent reader never observes a
/// half-written credential.
fn write_document(path: &Path, document: &Value, guard: FlockGuard) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Config("credential path has no parent directory".into()))?;

    let serialized = serde_json::to_vec_pretty(document)
        .map_err(|e| Error::Config(format!("serialize credential document: {e}")))?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".credentials-")
        .tempfile_in(dir)
        .map_err(|e| Error::Config(format!("credential temp file failed: {e}")))?;
    tmp.write_all(&serialized)?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600))?;
    }
    tmp.persist(path)
        .map_err(|e| Error::Config(format!("atomic credential rename failed: {e}")))?;

    // Fsync the directory so the rename itself is durable across a host crash:
    // the rename is atomic regardless, but without this the rename can be lost on
    // some filesystems even though the temp file's data was synced.
    if let Ok(dir_handle) = fs::File::open(dir) {
        let _ = dir_handle.sync_all();
    }

    drop(guard);
    Ok(())
}

/// Locked atomic write for a caller that does not already hold the lock: acquire,
/// write, release. The refresh path holds the lock across the whole cycle and
/// calls [`write_document`] directly, so this convenience is only used by tests.
#[cfg(test)]
fn persist_credentials(path: &Path, document: &Value) -> Result<()> {
    let guard = FlockGuard::acquire(path)?;
    write_document(path, document, guard)
}

/// RAII advisory-lock guard over the sibling lock file of a credential path, via
/// `flock(2)`. Owns the lock file, so the lock is held for the guard's lifetime
/// and released on drop — the guard can therefore be moved across an await and
/// held for the whole read-refresh-write cycle.
struct FlockGuard {
    file: fs::File,
}

impl FlockGuard {
    /// Acquire the exclusive advisory lock for the credential file at `creds_path`
    /// (blocking until available). The lock lives in a sibling file so the
    /// credential `rename` never swaps out the inode the lock is held on.
    fn acquire(creds_path: &Path) -> Result<Self> {
        let dir = creds_path
            .parent()
            .ok_or_else(|| Error::Config("credential path has no parent directory".into()))?;
        fs::create_dir_all(dir)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join(CREDENTIALS_LOCK_NAME))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(Self { file })
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        // Release explicitly; closing the file on drop would also release it.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_validate_valid_credentials() {
        let json =
            r#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"ref","expiresAt":123}}"#;
        assert!(validate_credentials_json(json).is_ok());
    }

    #[test]
    fn test_validate_missing_key() {
        let json = r#"{"someOtherKey": true}"#;
        let err = validate_credentials_json(json).unwrap_err();
        assert!(err.to_string().contains("claudeAiOauth"));
    }

    #[test]
    fn test_validate_invalid_json() {
        let err = validate_credentials_json("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn test_stage_credentials() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok"}}"#;
        let staged = stage_credentials(&SecretString::from(json)).unwrap();

        let creds_path = Path::new(&staged.host_path).join(".credentials.json");
        assert!(creds_path.exists());

        let content = fs::read_to_string(&creds_path).unwrap();
        assert_eq!(content, json);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::metadata(&creds_path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    // ---- Codex credential tests ----

    #[test]
    fn test_validate_codex_chatgpt_mode() {
        let json = r#"{"auth_mode":"chatgpt","tokens":{"id_token":"x","access_token":"y","refresh_token":"z","account_id":"a"},"OPENAI_API_KEY":null}"#;
        assert!(validate_codex_credentials_json(json).is_ok());
    }

    #[test]
    fn test_validate_codex_api_key_mode() {
        let json = r#"{"auth_mode":"api_key","OPENAI_API_KEY":"sk-...","tokens":null}"#;
        assert!(validate_codex_credentials_json(json).is_ok());
    }

    #[test]
    fn test_validate_codex_missing_auth_mode() {
        let json = r#"{"OPENAI_API_KEY":"sk-..."}"#;
        let err = validate_codex_credentials_json(json).unwrap_err();
        assert!(err.to_string().contains("auth_mode"));
    }

    #[test]
    fn test_validate_codex_invalid_json() {
        let err = validate_codex_credentials_json("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn test_stage_codex_credentials() {
        let json = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"tok"}}"#;
        let staged = stage_codex_credentials(&SecretString::from(json)).unwrap();

        let auth_path = Path::new(&staged.host_path).join("auth.json");
        assert!(auth_path.exists());

        let content = fs::read_to_string(&auth_path).unwrap();
        assert_eq!(content, json);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::metadata(&auth_path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn test_stage_codex_credentials_cleanup_on_drop() {
        let json = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"tok"}}"#;
        let path;
        {
            let staged = stage_codex_credentials(&SecretString::from(json)).unwrap();
            path = staged.host_path.clone();
            assert!(Path::new(&path).exists());
        }
        assert!(!Path::new(&path).exists());
    }

    #[test]
    fn test_stage_credentials_cleanup_on_drop() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok"}}"#;
        let path;
        {
            let staged = stage_credentials(&SecretString::from(json)).unwrap();
            path = staged.host_path.clone();
            assert!(Path::new(&path).exists());
        }
        // After drop, the temp dir should be gone.
        assert!(!Path::new(&path).exists());
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Epoch-milliseconds `delta_secs` from now (negative = in the past).
    fn ms_from_now(delta_secs: i64) -> i64 {
        let now = system_time_to_unix_ms(SystemTime::now());
        now + delta_secs * 1000
    }

    /// Build a `claudeAiOauth` credential JSON with the given tokens and expiry,
    /// plus an unrelated top-level key and nested field to prove write-back
    /// preserves everything it does not own.
    fn creds_json(access: &str, refresh: &str, expires_at_ms: i64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at_ms,
                "subscriptionType": "max",
                "scopes": ["user:inference"],
            },
            "someOtherTopLevelKey": {"keep": true},
        })
        .to_string()
    }

    /// A plain reqwest client (no SSRF guard) so a loopback mock is reachable.
    fn loopback_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("loopback client")
    }

    /// Spawn a one-shot-per-connection HTTP mock that returns `status_line` +
    /// `json_body` for every request. Returns its URL and a hit counter.
    fn spawn_mock_token_endpoint(status_line: &str, json_body: &str) -> (String, Arc<AtomicUsize>) {
        let response = format!(
            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json_body}",
            json_body.len()
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_thread = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                hits_thread.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf); // drain request; contents unused
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });
        (format!("http://{addr}/v1/oauth/token"), hits)
    }

    fn read_oauth_field(path: &Path, field: &str) -> String {
        let doc: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        doc["claudeAiOauth"][field].as_str().unwrap().to_string()
    }

    #[test]
    fn parse_extracts_fields() {
        let state = TokenState::parse(&creds_json("acc", "ref", ms_from_now(3600))).unwrap();
        assert_eq!(state.access_token.expose_secret(), "acc");
        assert_eq!(state.refresh_token.expose_secret(), "ref");
        assert!(state.valid_at(SystemTime::now()));
    }

    /// `TokenState` holds the preserved document (which contains plaintext
    /// tokens), so it deliberately has no `Debug`; extract the error by match
    /// rather than `unwrap_err`, which would require `Ok: Debug`.
    fn parse_err(json: &str) -> Error {
        match TokenState::parse(json) {
            Err(e) => e,
            Ok(_) => panic!("expected a parse error"),
        }
    }

    #[test]
    fn parse_rejects_missing_oauth() {
        assert!(parse_err(r#"{"nope": 1}"#)
            .to_string()
            .contains("claudeAiOauth"));
    }

    #[test]
    fn parse_rejects_missing_refresh_token() {
        let json = r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":123}}"#;
        assert!(parse_err(json).to_string().contains("refreshToken"));
    }

    #[tokio::test]
    async fn valid_token_is_returned_without_refresh() {
        // Far-future expiry → no refresh; a bogus endpoint proves it isn't hit.
        let dir = tempfile::tempdir().unwrap();
        let store = ClaudeOAuthStore::from_json(
            &SecretString::from(creds_json("live-access", "ref", ms_from_now(3600))),
            dir.path().join(".credentials.json"),
        )
        .unwrap()
        .with_token_endpoint("http://127.0.0.1:1/never")
        .with_http_client(loopback_client());
        let tok = store.access_token().await.unwrap();
        assert_eq!(tok.expose_secret(), "live-access");
    }

    #[test]
    fn persist_is_atomic_0600_and_preserves_unowned_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let doc: Value =
            serde_json::from_str(&creds_json("acc", "ref", ms_from_now(3600))).unwrap();
        persist_credentials(&path, &doc).unwrap();

        // Unowned fields survive.
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written["someOtherTopLevelKey"]["keep"],
            serde_json::json!(true)
        );
        assert_eq!(written["claudeAiOauth"]["subscriptionType"], "max");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn refresh_mints_and_writes_back_rotated_token() {
        let (url, hits) = spawn_mock_token_endpoint(
            "HTTP/1.1 200 OK",
            r#"{"access_token":"fresh-access","refresh_token":"rotated-refresh","expires_in":3600}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let store = ClaudeOAuthStore::from_json(
            &SecretString::from(creds_json("stale-access", "old-refresh", ms_from_now(-60))),
            path.clone(),
        )
        .unwrap()
        .with_token_endpoint(url)
        .with_http_client(loopback_client());

        let tok = store.access_token().await.unwrap();
        assert_eq!(tok.expose_secret(), "fresh-access");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // The rotated refresh token is written back to the host file, and unowned
        // fields are preserved.
        assert_eq!(read_oauth_field(&path, "refreshToken"), "rotated-refresh");
        assert_eq!(read_oauth_field(&path, "accessToken"), "fresh-access");
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written["someOtherTopLevelKey"]["keep"],
            serde_json::json!(true)
        );

        // A subsequent call reuses the freshly minted token — no second refresh.
        let again = store.access_token().await.unwrap();
        assert_eq!(again.expose_secret(), "fresh-access");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_without_rotation_keeps_old_refresh_token() {
        let (url, _hits) = spawn_mock_token_endpoint(
            "HTTP/1.1 200 OK",
            r#"{"access_token":"fresh-access","expires_in":3600}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let store = ClaudeOAuthStore::from_json(
            &SecretString::from(creds_json("stale", "keep-refresh", ms_from_now(-60))),
            path.clone(),
        )
        .unwrap()
        .with_token_endpoint(url)
        .with_http_client(loopback_client());

        assert_eq!(
            store.access_token().await.unwrap().expose_secret(),
            "fresh-access"
        );
        assert_eq!(read_oauth_field(&path, "refreshToken"), "keep-refresh");
    }

    #[tokio::test]
    async fn refresh_rejection_fails_closed_and_rate_caps() {
        let (url, hits) =
            spawn_mock_token_endpoint("HTTP/1.1 400 Bad Request", r#"{"error":"invalid_grant"}"#);
        let dir = tempfile::tempdir().unwrap();
        let store = ClaudeOAuthStore::from_json(
            &SecretString::from(creds_json("stale", "dead-refresh", ms_from_now(-60))),
            dir.path().join(".credentials.json"),
        )
        .unwrap()
        .with_token_endpoint(url)
        .with_http_client(loopback_client());

        assert!(store.access_token().await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Rate-cap: an immediate retry on the still-expired token fails closed
        // without spending another refresh round-trip.
        assert!(store.access_token().await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn adopts_valid_on_disk_token_from_a_peer_without_refreshing() {
        // Our in-memory copy is expired, but a peer run already wrote a still-valid
        // token to the shared file. `access_token` must adopt it under the lock and
        // never hit the token endpoint — an unreachable endpoint proves no network
        // call happens (a refused connect would error out and fail the unwrap).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        fs::write(
            &path,
            creds_json("peer-fresh-access", "peer-refresh", ms_from_now(3600)),
        )
        .unwrap();

        let store = ClaudeOAuthStore::from_json(
            &SecretString::from(creds_json(
                "our-stale-access",
                "our-refresh",
                ms_from_now(-60),
            )),
            path.clone(),
        )
        .unwrap()
        .with_token_endpoint("http://127.0.0.1:1/never")
        .with_http_client(loopback_client());

        let tok = store.access_token().await.unwrap();
        assert_eq!(tok.expose_secret(), "peer-fresh-access");
    }
}
