//! Tests for the R-B5c.1 agent-binary pinning manifest.
//!
//! These exercise the shell helpers in `scripts/lib/agent_manifest.sh` and the
//! R-B5c.1 guards in `scripts/lib/agent_rootfs_common.sh` (no VM required).
//! We drive bash as a subprocess and assert on stderr/exit-code behavior, so
//! the threat model claim ("build fails loudly on mismatch / missing
//! manifest / unverified override") is tested end-to-end, not just by
//! inspection.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_bash(script: &str) -> Output {
    Command::new("bash")
        .arg("-c")
        .arg(script)
        .current_dir(repo_root())
        .output()
        .expect("failed to spawn bash")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn manifest_pins_expected_tuples() {
    let manifest = repo_root().join("scripts/agents/manifest.toml");
    let body = fs::read_to_string(&manifest).expect("manifest must exist in tree");
    for tuple in [
        "[claude-code.linux.x86_64]",
        "[claude-code.linux.aarch64]",
        "[codex.linux.x86_64]",
        "[codex.linux.aarch64]",
    ] {
        assert!(
            body.contains(tuple),
            "manifest missing required tuple: {tuple}"
        );
    }
}

#[test]
fn manifest_reader_extracts_fields_in_stable_order() {
    let out = run_bash(
        r#"
        source scripts/lib/agent_manifest.sh
        agent_manifest_require claude-code linux x86_64
        "#,
    );
    assert!(out.status.success(), "stderr={}", stderr(&out));
    let out_stdout = stdout(&out);
    let lines: Vec<&str> = out_stdout.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 lines, got {:?}", lines);
    assert!(!lines[0].is_empty(), "version empty");
    assert!(
        lines[1].starts_with("https://"),
        "url not https: {}",
        lines[1]
    );
    assert_eq!(
        lines[2].len(),
        64,
        "sha256 should be 64 hex chars, got: {}",
        lines[2]
    );
}

#[test]
fn manifest_reader_fails_on_missing_entry() {
    let out = run_bash(
        r#"
        source scripts/lib/agent_manifest.sh
        agent_manifest_require claude-code windows x86_64
        "#,
    );
    assert!(!out.status.success(), "expected failure for unknown tuple");
    assert!(
        stderr(&out).contains("manifest entry [claude-code.windows.x86_64] missing"),
        "stderr did not name the missing tuple: {}",
        stderr(&out)
    );
}

#[test]
fn manifest_reader_fails_on_missing_file() {
    let out = run_bash(
        r#"
        source scripts/lib/agent_manifest.sh
        # Shadow the path resolver so the helper looks at a nonexistent file.
        agent_manifest_path() { printf '%s\n' "/tmp/does-not-exist-$$-manifest.toml"; }
        agent_manifest_require claude-code linux x86_64
        "#,
    );
    assert!(
        !out.status.success(),
        "expected failure when manifest is missing"
    );
    assert!(
        stderr(&out).contains("manifest not found"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn manifest_verify_rejects_wrong_hash() {
    // Write a file whose real SHA-256 differs from the expected hash below.
    let tmp = tempfile_path("r-b5c1-sha-mismatch.bin");
    fs::write(&tmp, b"payload").unwrap();
    let script = format!(
        r#"
        source scripts/lib/agent_manifest.sh
        agent_manifest_verify "{}" deadbeef label
        "#,
        tmp.display()
    );
    let out = run_bash(&script);
    assert!(!out.status.success(), "verify should have rejected");
    let err = stderr(&out);
    assert!(err.contains("SHA-256 mismatch"), "stderr: {err}");
    assert!(err.contains("expected: deadbeef"), "stderr: {err}");
    let _ = fs::remove_file(&tmp);
}

#[test]
fn manifest_verify_accepts_matching_hash() {
    let tmp = tempfile_path("r-b5c1-sha-match.bin");
    fs::write(&tmp, b"hello world").unwrap();
    // sha256("hello world") — computed once, stable.
    let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    let script = format!(
        r#"
        source scripts/lib/agent_manifest.sh
        agent_manifest_verify "{}" {} label
        "#,
        tmp.display(),
        expected
    );
    let out = run_bash(&script);
    assert!(
        out.status.success(),
        "expected verify to succeed, stderr={}",
        stderr(&out)
    );
    let _ = fs::remove_file(&tmp);
}

#[test]
fn claude_override_without_sha_is_rejected() {
    let out = run_bash(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=false
        CLAUDE_CODE_VERSION=99.99.99 resolve_claude_binary r-b5c1-test
        "#,
    );
    assert!(
        !out.status.success(),
        "resolve_claude_binary must fail when the override has no SHA"
    );
    let err = stderr(&out);
    assert!(
        err.contains("CLAUDE_CODE_VERSION=99.99.99 is set without a matching CLAUDE_CODE_SHA256"),
        "stderr did not name the missing SHA env: {err}"
    );
    assert!(
        err.contains("R-B5c.1 forbids unverified overrides"),
        "stderr did not cite R-B5c.1: {err}"
    );
}

#[test]
fn codex_override_without_sha_is_rejected() {
    let out = run_bash(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=false
        CODEX_VERSION=99.99.99 resolve_codex_binary r-b5c1-test
        "#,
    );
    assert!(
        !out.status.success(),
        "resolve_codex_binary must fail when the override has no SHA"
    );
    let err = stderr(&out);
    assert!(
        err.contains("CODEX_VERSION=99.99.99 is set without a matching CODEX_SHA256"),
        "stderr did not name the missing SHA env: {err}"
    );
    assert!(
        err.contains("R-B5c.1 forbids unverified overrides"),
        "stderr did not cite R-B5c.1: {err}"
    );
}

#[test]
fn resolve_claude_fails_if_manifest_missing() {
    // Resolver must not silently fall back to an unpinned download.
    let out = run_bash(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=true
        agent_manifest_path() { printf '%s\n' "/tmp/does-not-exist-$$-manifest.toml"; }
        resolve_claude_binary r-b5c1-test
        "#,
    );
    assert!(!out.status.success(), "must fail when manifest is absent");
    assert!(
        stderr(&out).contains("manifest not found"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn parser_strips_inline_trailing_comments() {
    let scratch = tempfile_path("r-b5c1-inline-comment.toml");
    fs::write(
        &scratch,
        r#"[claude-code.linux.x86_64]
version = "9.9.9" # annotated for CVE-2099-0000
url = "https://example.com/{version}/bin" # stable URL
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
"#,
    )
    .unwrap();
    let script = format!(
        r#"
        source scripts/lib/agent_manifest.sh
        agent_manifest_path() {{ printf '%s\n' "{}"; }}
        agent_manifest_require claude-code linux x86_64
        "#,
        scratch.display()
    );
    let out = run_bash(&script);
    assert!(out.status.success(), "stderr={}", stderr(&out));
    let out_stdout = stdout(&out);
    let lines: Vec<&str> = out_stdout.lines().collect();
    assert_eq!(
        lines,
        [
            "9.9.9",
            "https://example.com/{version}/bin",
            "0000000000000000000000000000000000000000000000000000000000000000"
        ]
    );
    let _ = fs::remove_file(&scratch);
}

#[test]
fn parser_rejects_single_quoted_values() {
    let scratch = tempfile_path("r-b5c1-singlequoted.toml");
    fs::write(
        &scratch,
        "[claude-code.linux.x86_64]\nversion = '1.0.0'\nurl = \"x\"\nsha256 = \"y\"\n",
    )
    .unwrap();
    let script = format!(
        r#"
        source scripts/lib/agent_manifest.sh
        agent_manifest_path() {{ printf '%s\n' "{}"; }}
        agent_manifest_require claude-code linux x86_64
        "#,
        scratch.display()
    );
    let out = run_bash(&script);
    assert!(
        !out.status.success(),
        "parser must reject single-quoted value"
    );
    assert!(
        stderr(&out).contains("single-quoted values are not accepted"),
        "stderr: {}",
        stderr(&out)
    );
    let _ = fs::remove_file(&scratch);
}

#[test]
fn parser_rejects_trailing_junk_after_value() {
    let scratch = tempfile_path("r-b5c1-trailing-junk.toml");
    fs::write(
        &scratch,
        "[claude-code.linux.x86_64]\nversion = \"1.0.0\" unexpected\nurl = \"x\"\nsha256 = \"y\"\n",
    )
    .unwrap();
    let script = format!(
        r#"
        source scripts/lib/agent_manifest.sh
        agent_manifest_path() {{ printf '%s\n' "{}"; }}
        agent_manifest_require claude-code linux x86_64
        "#,
        scratch.display()
    );
    let out = run_bash(&script);
    assert!(!out.status.success(), "parser must reject trailing junk");
    assert!(
        stderr(&out).contains("unexpected content after quoted value"),
        "stderr: {}",
        stderr(&out)
    );
    let _ = fs::remove_file(&scratch);
}

#[test]
fn claude_ambiguous_env_combo_is_rejected() {
    let out = run_bash(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=false
        CLAUDE_BIN=/usr/bin/true CLAUDE_CODE_VERSION=9.9.9 CLAUDE_CODE_SHA256=deadbeef \
            resolve_claude_binary r-b5c1-test
        "#,
    );
    assert!(
        !out.status.success(),
        "setting both CLAUDE_BIN and CLAUDE_CODE_VERSION must be refused"
    );
    assert!(
        stderr(&out).contains("ambiguous resolution"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn codex_ambiguous_env_combo_is_rejected() {
    let out = run_bash(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=false
        CODEX_BIN=/usr/bin/true CODEX_VERSION=9.9.9 CODEX_SHA256=deadbeef \
            resolve_codex_binary r-b5c1-test
        "#,
    );
    assert!(
        !out.status.success(),
        "setting both CODEX_BIN and CODEX_VERSION must be refused"
    );
    assert!(
        stderr(&out).contains("ambiguous resolution"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn override_drift_from_manifest_warns() {
    // Override CLAUDE_CODE_VERSION to something the manifest does not pin.
    // The resolver will still fail at fetch-time (the URL probably 404s, or
    // the SHA won't match), but before it fails it must print the drift
    // warning — and we must NOT see "R-B5c.1 forbids unverified overrides"
    // because the SHA env var IS set.
    let out = run_bash(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=true
        # Stub the fetcher so we don't hit the network. Return 1 like a
        # verification failure so the function returns nonzero normally.
        _agent_fetch_and_verify() { return 1; }
        CLAUDE_CODE_VERSION=0.0.0-dev-override \
        CLAUDE_CODE_SHA256=deadbeef \
            resolve_claude_binary r-b5c1-test
        "#,
    );
    let err = stderr(&out);
    assert!(
        err.contains("differs from manifest pin"),
        "stderr should warn on drift but didn't: {err}"
    );
    assert!(
        !err.contains("R-B5c.1 forbids unverified overrides"),
        "stderr must not complain about missing SHA when SHA is set: {err}"
    );
}

#[test]
fn find_extracted_executable_prefers_named_binary() {
    // A tarball extraction often contains multiple executables (e.g.
    // `codex` + `codex-migrate` + a README script). Deterministic
    // selection under `LC_ALL=C sort` is fine but the explicit-name path
    // is stronger: pass `codex` and it should win even if another
    // sortable-first file exists alongside.
    let dir = tempfile_path("r-b5c1-find");
    fs::create_dir_all(&dir).unwrap();
    let decoy = dir.join("aaa-decoy");
    let target = dir.join("codex");
    fs::write(&decoy, b"#!/bin/sh\n").unwrap();
    fs::write(&target, b"#!/bin/sh\n").unwrap();
    // Both need the exec bit for the helper to consider them.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    let script = format!(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        find_extracted_executable "{}" codex
        "#,
        dir.display()
    );
    let out = run_bash(&script);
    assert!(out.status.success(), "stderr={}", stderr(&out));
    let picked = stdout(&out).trim().to_string();
    assert_eq!(
        picked,
        target.to_string_lossy(),
        "preferred name should win over lexicographically earlier decoy"
    );

    let _ = fs::remove_dir_all(&dir);
}

fn tempfile_path(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir: PathBuf = std::env::temp_dir();
    dir.join(format!(
        "{}-{}-{}",
        std::process::id(),
        seq,
        Path::new(name).display()
    ))
}
