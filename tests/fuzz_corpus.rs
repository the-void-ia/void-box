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
const TARGETS: &[&str] = &[
    "vsock_frame",
    "vsock_packet",
    "virtqueue",
    "nine_p",
    "nine_p_transport",
];

/// Run one corpus input through the harness that owns it, returning the units of
/// work the harness reported — or `None` when the target compiles out on this
/// platform.
///
/// `root` is a scratch directory the 9P harness may modify. Each input gets its
/// own, so a replay failure names one file rather than one file plus whatever
/// the previous input left behind.
fn replay(target: &str, data: &[u8], root: &Path) -> Option<usize> {
    match target {
        "vsock_frame" => Some(void_box::fuzz::vsock_frame(data)),
        #[cfg(target_os = "linux")]
        "vsock_packet" => Some(void_box::fuzz::vsock_packet(data)),
        #[cfg(target_os = "linux")]
        "virtqueue" => Some(void_box::fuzz::virtqueue(data)),
        #[cfg(target_os = "linux")]
        "nine_p" => Some(void_box::fuzz::nine_p(root, data)),
        #[cfg(target_os = "linux")]
        "nine_p_transport" => Some(void_box::fuzz::nine_p_transport(root, data)),
        // The device harnesses are Linux-only because `void_box::devices` is.
        // Their corpus still travels with the repo and still replays on Linux.
        #[cfg(not(target_os = "linux"))]
        "vsock_packet" | "virtqueue" | "nine_p" | "nine_p_transport" => {
            let _ = (data, root);
            None
        }
        other => panic!("no harness registered for fuzz target {other}"),
    }
}

/// Units of work a seed must produce to count as reaching its parser.
///
/// A harness that reaches nothing returns cleanly, so panic-freedom alone lets a
/// target go inert without the gate noticing — the seed still replays, it simply
/// stops exercising the code it is named for. Holding each seed to a floor is
/// what makes that visible.
///
/// `nine_p_transport` carries the high floor because reaching the queue walker
/// takes a whole bring-up sequence: queue size, three ring addresses, ready, and
/// notify. Its seeds each carry ten register writes, so eight leaves margin for
/// a seed to be trimmed while still failing an input that programs nothing.
fn work_floor(target: &str) -> usize {
    match target {
        "nine_p_transport" => 8,
        _ => 1,
    }
}

/// Where a replayed input came from. Seeds are written to cover a shape and are
/// held to [`work_floor`]; artifacts are inputs a fuzz run crashed on, and one
/// legitimately does almost nothing before reaching the bug it captures.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum InputKind {
    Seed,
    Artifact,
}

/// Every input file under `fuzz/corpus/<target>/` and `fuzz/artifacts/<target>/`.
fn inputs_for(target: &str) -> Vec<(InputKind, PathBuf)> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz");
    let mut files = Vec::new();
    for (kind, dir_name) in [
        (InputKind::Seed, "corpus"),
        (InputKind::Artifact, "artifacts"),
    ] {
        let dir = base.join(dir_name).join(target);
        let Ok(entries) = fs::read_dir(&dir) else {
            // `artifacts/` exists only once a target has crashed at least once.
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().is_file() {
                files.push((kind, entry.path()));
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

    let mut skipped: Vec<&str> = Vec::new();
    let mut inert: Vec<String> = Vec::new();
    for target in TARGETS {
        let mut ran_any = false;
        let floor = work_floor(target);
        for (kind, path) in inputs_for(target) {
            let data = fs::read(&path)
                .unwrap_or_else(|err| panic!("read corpus input {}: {err}", path.display()));
            let root = scratch.path().join(format!("{target}-{replayed}"));
            fs::create_dir_all(&root).expect("create the per-input scratch root");
            // A panic here prints the harness's own assertion; name the input
            // too, since the whole point is to hand back a reproducer.
            eprintln!("replaying {}", path.display());
            let Some(work) = replay(target, &data, &root) else {
                continue;
            };
            replayed += 1;
            ran_any = true;
            // Only seeds. A crash artifact earns its place by reaching a bug,
            // which it may do before performing any countable work.
            if kind == InputKind::Seed && work < floor {
                inert.push(format!(
                    "{} did {work} units of work, below the {floor} its target requires",
                    path.display()
                ));
            }
        }
        if !ran_any {
            skipped.push(target);
        }
    }

    // Count harnesses actually executed, not files read. Four of the five
    // targets compile out on non-Linux, and a count of files would report a
    // healthy number there while exercising one parser — the "covering nothing"
    // guard would never fire on the platform where it matters most.
    if !skipped.is_empty() {
        eprintln!(
            "SKIP: {} not replayed on this platform (void_box::devices is Linux-only): {}",
            skipped.len(),
            skipped.join(", ")
        );
    }
    assert!(
        replayed > 0,
        "no fuzz harness ran — the replay gate is covering nothing"
    );
    // A seed below the floor means its harness no longer reaches the parser the
    // seed was written for. The input still replays and still panics on nothing,
    // so this is the only signal that the target has gone inert.
    assert!(
        inert.is_empty(),
        "these seeds no longer reach their parser:\n  {}",
        inert.join("\n  ")
    );
    #[cfg(target_os = "linux")]
    assert!(
        skipped.is_empty(),
        "every target must replay on Linux, but these did not: {}",
        skipped.join(", ")
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

    // Not a TOML parser: match `name` followed by `=` with any spacing, so a
    // target declared as `name="x"` is not silently skipped — that would leave
    // the very gap this test exists to close.
    let declared: Vec<String> = manifest
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("name"))
        .filter_map(|rest| rest.trim_start().strip_prefix('='))
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
            inputs_for(name)
                .iter()
                .any(|(kind, _)| *kind == InputKind::Seed),
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
