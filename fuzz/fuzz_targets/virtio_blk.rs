//! virtio-blk over its MMIO registers and guest memory: the request handler
//! that turns a guest's header, data, and status descriptors into disk reads.
//!
//! Fresh scratch directory per input, in which the harness creates the backing
//! disk. The disk is read-only to the device and rewritten by every call, so
//! sharing one directory would be safe; a fresh one per input costs little and
//! matches what `tests/fuzz_corpus.rs` gives each replayed artifact.
#![no_main]

// `void_box::devices` is Linux-only, so this parser does not exist elsewhere.
// Failing the build with a reason beats either a confusing type error or a
// target that silently fuzzes nothing.
#[cfg(not(target_os = "linux"))]
compile_error!("this parser is Linux-only; fuzz it on Linux");

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let root = tempfile::Builder::new()
        .prefix("void-box-fuzz-blk-")
        .tempdir()
        .expect("create the virtio-blk fuzz root");
    // The work count is for the replay gate; here any input is valid.
    let _ = void_box::fuzz::virtio_blk(root.path(), data);
});
