//! 9P2000.L request handling against a host directory.
//!
//! The root is a fresh `TempDir` per process rather than per input: creating one
//! per iteration would dominate the run, and the harness's read-write pass needs
//! somewhere it may freely create, rename, and unlink. State therefore
//! accumulates across inputs within a run, which is also what a long-lived mount
//! looks like to the guest. `TempDir::keep` hands the path to the OS rather than
//! deleting it, because libFuzzer aborts the process on a crash and a destructor
//! would not run anyway; the enclosing temp directory is the OS's to reclaim.
#![no_main]

// `void_box::devices` is Linux-only, so this parser does not exist elsewhere.
// Failing the build with a reason beats either a confusing type error or a
// target that silently fuzzes nothing.
#[cfg(not(target_os = "linux"))]
compile_error!("this parser is Linux-only; fuzz it on Linux");

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let root = ROOT.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("void-box-fuzz-9p-")
            .tempdir()
            .expect("create the 9P fuzz root")
            .keep()
    });
    void_box::fuzz::nine_p(root, data);
});
