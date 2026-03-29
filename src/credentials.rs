//! OAuth credential discovery and staging for the `claude-personal` provider.
//!
//! This module locates Claude personal-plan OAuth credentials on the host
//! (macOS Keychain or Linux filesystem), stages them into a secure temp
//! directory, and exposes the host path so the runtime can mount them into
//! the guest VM at `/home/sandbox/.claude`.
//!
//! Cleanup is automatic: [`StagedCredentials`] wraps a [`tempfile::TempDir`]
//! whose `Drop` impl removes the directory when the value goes out of scope.

use crate::{Error, Result};
use std::fs;

/// Staged credentials ready to be mounted into the guest VM.
///
/// The underlying temp directory is removed when this value is dropped.
pub(crate) struct StagedCredentials {
    /// Kept alive for its `Drop` impl (auto-cleanup).
    _dir: tempfile::TempDir,
    /// Absolute host path to the staged directory.
    pub(crate) host_path: String,
}

/// Discover OAuth credentials from the host system.
///
/// - **macOS**: reads from the system Keychain via `security find-generic-password`.
/// - **Linux**: reads `~/.claude/.credentials.json`.
///
/// Returns the raw JSON string on success, or a user-friendly error directing
/// the user to run `claude auth login`.
pub(crate) fn discover_oauth_credentials() -> Result<String> {
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
fn discover_macos() -> Result<String> {
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
    Ok(json)
}

#[cfg(target_os = "linux")]
fn discover_linux() -> Result<String> {
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
    Ok(json)
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
pub(crate) fn stage_credentials(creds_json: &str) -> Result<StagedCredentials> {
    let dir = tempfile::Builder::new()
        .prefix("voidbox-claude-creds-")
        .tempdir()
        .map_err(|e| Error::Config(format!("failed to create temp dir for credentials: {e}")))?;

    let creds_path = dir.path().join(".credentials.json");
    fs::write(&creds_path, creds_json)
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
        let staged = stage_credentials(json).unwrap();

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

    #[test]
    fn test_stage_credentials_cleanup_on_drop() {
        let json = r#"{"claudeAiOauth":{"accessToken":"tok"}}"#;
        let path;
        {
            let staged = stage_credentials(json).unwrap();
            path = staged.host_path.clone();
            assert!(Path::new(&path).exists());
        }
        // After drop, the temp dir should be gone.
        assert!(!Path::new(&path).exists());
    }
}
