//! 9P2000.L request handling against a host directory.
//!
//! The root is a fresh `TempDir` per input, matching what `tests/fuzz_corpus.rs`
//! gives each replayed artifact. Sharing one root across a run would be faster,
//! but a crash that depended on a file an earlier input created would then not
//! reproduce from its own artifact — it would pass the merge gate while the bug
//! stayed live. Reproducibility is the property the artifact exists for.
#![no_main]

// `void_box::devices` is Linux-only, so this parser does not exist elsewhere.
// Failing the build with a reason beats either a confusing type error or a
// target that silently fuzzes nothing.
#[cfg(not(target_os = "linux"))]
compile_error!("this parser is Linux-only; fuzz it on Linux");

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let root = tempfile::Builder::new()
        .prefix("void-box-fuzz-9p-")
        .tempdir()
        .expect("create the 9P fuzz root");
    void_box::fuzz::nine_p(root.path(), data);
});
