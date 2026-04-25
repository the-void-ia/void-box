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
fn claude_override_differing_from_manifest_without_sha_is_rejected() {
    // 99.99.99 differs from the pinned manifest version, so it's a
    // genuine override and the SHA env var is required.
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
        err.contains("CLAUDE_CODE_VERSION=99.99.99 differs from the manifest pin"),
        "stderr did not name the drift: {err}"
    );
    assert!(
        err.contains("R-B5c.1 forbids unverified overrides"),
        "stderr did not cite R-B5c.1: {err}"
    );
}

#[test]
fn codex_override_differing_from_manifest_without_sha_is_rejected() {
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
        err.contains("CODEX_VERSION=99.99.99 differs from the manifest pin"),
        "stderr did not name the drift: {err}"
    );
    assert!(
        err.contains("R-B5c.1 forbids unverified overrides"),
        "stderr did not cite R-B5c.1: {err}"
    );
}

#[test]
fn claude_version_matching_manifest_pin_is_accepted_without_sha() {
    // Setting CLAUDE_CODE_VERSION to the manifest's currently-pinned
    // version is not really an override — the resolver should treat it
    // as a no-op and use the manifest's pinned SHA. This is the path CI
    // workflows take when they (legacy) export CLAUDE_CODE_VERSION
    // without a SHA.
    //
    // We stub the fetcher to return success without doing network I/O.
    // The assertion is that the resolver gets past the SHA-required
    // guard and reports provenance=manifest.
    let pinned_version = pinned_manifest_field("claude-code", "linux", "x86_64", "version");
    let out = run_bash(&format!(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=false
        # Stub the fetcher: pretend the download succeeded.
        _agent_fetch_and_verify() {{ touch "$3"; chmod +x "$3"; return 0; }}
        # Stub `file -L ... ELF executable` check by replacing the file
        # check with a no-op echo. The function only cares about the grep
        # match, so prepend a wrapper.
        file() {{ echo "ELF 64-bit LSB executable"; }}
        CLAUDE_CODE_VERSION={pinned_version} resolve_claude_binary r-b5c1-test
        "#,
    ));
    assert!(
        out.status.success(),
        "resolve_claude_binary must accept VERSION matching manifest pin without SHA, stderr={}",
        stderr(&out)
    );
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        combined.contains("[provenance=manifest]"),
        "expected manifest provenance for same-version case: {combined}"
    );
    assert!(
        !combined.contains("R-B5c.1 forbids"),
        "must not complain about missing SHA when VERSION equals manifest pin: {combined}"
    );
}

#[test]
fn codex_version_matching_manifest_pin_is_accepted_without_sha() {
    let pinned_version = pinned_manifest_field("codex", "linux", "x86_64", "version");
    let out = run_bash(&format!(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=false
        # Stubs: succeed both the fetch+verify and the post-extract bin
        # discovery. _codex_fetch_verify_extract sets CODEX_BIN as a side
        # effect, so we replace it directly.
        _codex_fetch_verify_extract() {{
            local cached="$ROOT_DIR/target/codex-download/codex-$2-$3"
            mkdir -p "$(dirname "$cached")"
            touch "$cached"; chmod +x "$cached"
            CODEX_BIN="$cached"
            return 0
        }}
        file() {{ echo "ELF 64-bit LSB executable"; }}
        CODEX_VERSION={pinned_version} resolve_codex_binary r-b5c1-test
        "#,
    ));
    assert!(
        out.status.success(),
        "resolve_codex_binary must accept VERSION matching manifest pin without SHA, stderr={}",
        stderr(&out)
    );
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        combined.contains("[provenance=manifest]"),
        "expected manifest provenance for same-version case: {combined}"
    );
}

#[test]
fn claude_version_matching_manifest_with_disagreeing_sha_is_rejected() {
    // If the user supplies VERSION=manifest-pin AND a SHA that disagrees
    // with the manifest's SHA, that's a typo or attempted attack — fail
    // loudly rather than silently using the manifest SHA.
    let pinned_version = pinned_manifest_field("claude-code", "linux", "x86_64", "version");
    let out = run_bash(&format!(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        ROOT_DIR="$PWD"
        GUEST_ARCH=x86_64
        IS_CROSS_BUILD=false
        CLAUDE_CODE_VERSION={pinned_version} CLAUDE_CODE_SHA256=deadbeef \
            resolve_claude_binary r-b5c1-test
        "#,
    ));
    assert!(
        !out.status.success(),
        "must reject SHA env var that disagrees with manifest"
    );
    let err = stderr(&out);
    assert!(
        err.contains("CLAUDE_CODE_SHA256 disagrees with the manifest SHA"),
        "stderr did not name the disagreement: {err}"
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
fn find_extracted_executable_prefers_named_binary_over_sort_earlier_decoy() {
    // Regression guard: a tarball extraction may contain a
    // lexicographically-earlier executable that is NOT the binary we
    // want (e.g. `aaa-uninstall`). Without a preferred name, the
    // sort-deterministic fallback (Pass 2) would pick the decoy. With a
    // preferred name, Pass 1 must filter to the name and win. A
    // regression where Pass 1 silently falls through to Pass 2 would
    // flip this test to returning the decoy.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile_path("r-b5c1-find-pref");
    fs::create_dir_all(&dir).unwrap();
    let decoy = dir.join("aaa-decoy");
    let target = dir.join("codex");
    fs::write(&decoy, b"#!/bin/sh\n").unwrap();
    fs::write(&target, b"#!/bin/sh\n").unwrap();
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
        "preferred name must win over lexicographically earlier non-matching decoy"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn find_extracted_executable_is_deterministic_without_preferred_name() {
    // Without a preferred name, the helper must pick the lexicographically
    // first executable under LC_ALL=C sort. This pins the behavior so a
    // future refactor that relies on `find`'s raw (filesystem-defined)
    // order fails in CI rather than silently choosing different binaries
    // on different runners.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile_path("r-b5c1-find-sort");
    fs::create_dir_all(&dir).unwrap();
    // `alpha` sorts before `zeta` under LC_ALL=C.
    let first = dir.join("alpha");
    let second = dir.join("zeta");
    fs::write(&first, b"#!/bin/sh\n").unwrap();
    fs::write(&second, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&first, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&second, fs::Permissions::from_mode(0o755)).unwrap();

    let script = format!(
        r#"
        source scripts/lib/agent_rootfs_common.sh
        find_extracted_executable "{}"
        "#,
        dir.display()
    );
    let out = run_bash(&script);
    assert!(out.status.success(), "stderr={}", stderr(&out));
    let picked = stdout(&out).trim().to_string();
    assert_eq!(
        picked,
        first.to_string_lossy(),
        "no-preferred-name fallback must pick the LC_ALL=C sort-first executable"
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

/// Read a single field (`version`, `url`, or `sha256`) from the in-tree
/// manifest by shelling out to the same awk parser the build scripts use.
/// Used by tests that need to assert behavior against the currently-pinned
/// values without hard-coding them.
fn pinned_manifest_field(agent: &str, platform: &str, arch: &str, field: &str) -> String {
    let line_n = match field {
        "version" => "1p",
        "url" => "2p",
        "sha256" => "3p",
        other => panic!("unknown manifest field: {other}"),
    };
    let script = format!(
        r#"
        source scripts/lib/agent_manifest.sh
        agent_manifest_require {agent} {platform} {arch} | sed -n '{line_n}'
        "#
    );
    let out = run_bash(&script);
    assert!(
        out.status.success(),
        "failed to read manifest field {field} for [{agent}.{platform}.{arch}]: stderr={}",
        stderr(&out)
    );
    stdout(&out).trim().to_string()
}
