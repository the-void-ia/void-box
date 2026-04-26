//! OAuth credential discovery and staging for personal-auth providers.
//!
//! This module locates per-agent credentials on the host (macOS Keychain or
//! Linux filesystem), stages them into a secure temp directory, and exposes
//! the host path so the runtime can mount them into the guest VM at the
//! agent's expected `$HOME` config directory.
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

use crate::{Error, Result};
use secrecy::{ExposeSecret, SecretString};
use std::fs;

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
