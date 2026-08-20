//! 9P over its MMIO registers and guest memory — the queue programming and
//! descriptor walk beneath the message parser.
//!
//! Fresh root per input, like `nine_p`: see that target for why.
#![no_main]

// `void_box::devices` is Linux-only, so this parser does not exist elsewhere.
// Failing the build with a reason beats either a confusing type error or a
// target that silently fuzzes nothing.
#[cfg(not(target_os = "linux"))]
compile_error!("this parser is Linux-only; fuzz it on Linux");

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let root = tempfile::Builder::new()
        .prefix("void-box-fuzz-9pt-")
        .tempdir()
        .expect("create the 9P transport fuzz root");
    // The work count is for the replay gate; here any input is valid.
    let _ = void_box::fuzz::nine_p_transport(root.path(), data);
});
