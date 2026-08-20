//! Userspace virtio-vsock: header parsing and the host-side connection state
//! machine that routes guest TX packets.
#![no_main]

// `void_box::devices` is Linux-only, so this parser does not exist elsewhere.
// Failing the build with a reason beats either a confusing type error or a
// target that silently fuzzes nothing.
#[cfg(not(target_os = "linux"))]
compile_error!("this parser is Linux-only; fuzz it on Linux");

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The work count is for the replay gate; here any input is valid.
    let _ = void_box::fuzz::vsock_packet(data);
});
