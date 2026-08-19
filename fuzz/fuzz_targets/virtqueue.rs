//! Split-virtqueue reader: descriptor-chain walking over guest memory, with
//! guest-chosen queue geometry.
#![no_main]

// `void_box::devices` is Linux-only, so this parser does not exist elsewhere.
// Failing the build with a reason beats either a confusing type error or a
// target that silently fuzzes nothing.
#[cfg(not(target_os = "linux"))]
compile_error!("this parser is Linux-only; fuzz it on Linux");

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    void_box::fuzz::virtqueue(data);
});
