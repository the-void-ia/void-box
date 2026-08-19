//! Replay the committed fuzz corpus through the harnesses on stable Rust.
//!
//! `cargo fuzz` finds new inputs; this test is what stops a found input from
//! coming back. libFuzzer needs a nightly toolchain and sanitizer
//! instrumentation, so it runs in a workflow of its own, on demand — which
//! means nothing it discovers would be checked on a pull request. Replaying
//! every seed and every crash artifact here puts that check inside the ordinary
//! `cargo test` gate, on the toolchain the project actually ships.
//!
//! Workflow for a crash: `cargo fuzz` writes the input under
//! `fuzz/artifacts/<target>/`, `git add` it, fix the parser, and this test then
//! fails for anyone who reintroduces the bug. The corpus doubles as
//! documentation — each seed's filename names the shape it covers.
//!
//! A crashing input must never be committed before its fix, since it would red
//! the gate for every unrelated change.

use std::fs;
use std::path::{Path, PathBuf};

/// Corpus directories are per target; `nine_p` additionally needs a writable
/// root, so the dispatch is by name rather than by a function pointer table.
const TARGETS: &[&str] = &["vsock_frame", "vsock_packet", "virtqueue", "nine_p"];

/// Run one corpus input through the harness that owns it.
///
/// `root` is a scratch directory the 9P harness may modify. Each input gets its
/// own, so a replay failure names one file rather than one file plus whatever
/// the previous input left behind.
fn replay(target: &str, data: &[u8], root: &Path) {
    match target {
        "vsock_frame" => void_box::fuzz::vsock_frame(data),
        #[cfg(target_os = "linux")]
        "vsock_packet" => void_box::fuzz::vsock_packet(data),
        #[cfg(target_os = "linux")]
        "virtqueue" => void_box::fuzz::virtqueue(data),
        #[cfg(target_os = "linux")]
        "nine_p" => void_box::fuzz::nine_p(root, data),
        // The device harnesses are Linux-only because `void_box::devices` is.
        // Their corpus still travels with the repo and still replays on Linux.
        #[cfg(not(target_os = "linux"))]
        "vsock_packet" | "virtqueue" | "nine_p" => {
            let _ = (data, root);
        }
        other => panic!("no harness registered for fuzz target {other}"),
    }
}

/// Every input file under `fuzz/corpus/<target>/` and `fuzz/artifacts/<target>/`.
fn inputs_for(target: &str) -> Vec<PathBuf> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz");
    let mut files = Vec::new();
    for kind in ["corpus", "artifacts"] {
        let dir = base.join(kind).join(target);
        let Ok(entries) = fs::read_dir(&dir) else {
            // `artifacts/` exists only once a target has crashed at least once.
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
}

/// Every committed input replays without panicking, and the harness invariants
/// hold on all of them.
#[test]
fn fuzz_corpus_replays_clean() {
    let scratch = tempfile::tempdir().expect("scratch dir for the 9P corpus");
    let mut replayed = 0usize;

    for target in TARGETS {
        for path in inputs_for(target) {
            let data = fs::read(&path)
                .unwrap_or_else(|err| panic!("read corpus input {}: {err}", path.display()));
            let root = scratch.path().join(format!("{target}-{replayed}"));
            fs::create_dir_all(&root).expect("create the per-input scratch root");
            // A panic here prints the harness's own assertion; name the input
            // too, since the whole point is to hand back a reproducer.
            eprintln!("replaying {}", path.display());
            replay(target, &data, &root);
            replayed += 1;
        }
    }

    assert!(
        replayed > 0,
        "no fuzz corpus inputs found under fuzz/corpus — the replay gate is covering nothing"
    );
}

/// Every target named in `fuzz/Cargo.toml` has a harness here, and a seed
/// corpus.
///
/// Without this, adding a `[[bin]]` to the fuzz crate and forgetting to
/// register it leaves a target libFuzzer explores while the pull-request gate
/// never replays a single one of its inputs — the silent-coverage-loss failure
/// this whole file exists to prevent.
#[test]
fn every_fuzz_target_is_replayed() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz");
    let manifest = fs::read_to_string(base.join("Cargo.toml")).expect("read fuzz/Cargo.toml");

    let declared: Vec<String> = manifest
        .lines()
        .filter_map(|line| line.trim().strip_prefix("name = "))
        .map(|value| value.trim().trim_matches('"').to_string())
        // The `[package]` name shares the key; the targets are the rest.
        .filter(|name| name != "void-box-fuzz")
        .collect();

    assert!(
        !declared.is_empty(),
        "parsed no [[bin]] targets out of fuzz/Cargo.toml"
    );
    for name in &declared {
        assert!(
            TARGETS.contains(&name.as_str()),
            "fuzz target {name} is declared in fuzz/Cargo.toml but not replayed by this test"
        );
        assert!(
            !inputs_for(name).is_empty(),
            "fuzz target {name} has no seed corpus under fuzz/corpus/{name}"
        );
    }
    for target in TARGETS {
        assert!(
            declared.iter().any(|name| name == target),
            "this test replays {target}, but fuzz/Cargo.toml declares no such target"
        );
    }
}
