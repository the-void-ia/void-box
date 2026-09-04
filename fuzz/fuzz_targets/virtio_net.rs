//! virtio-net over its MMIO registers and guest memory: the TX walk that
//! assembles a frame from a guest chain and the RX walk that scatters an inbound
//! frame into the buffers the guest posted.
#![no_main]

// `void_box::devices` is Linux-only, so this parser does not exist elsewhere.
// Failing the build with a reason beats either a confusing type error or a
// target that silently fuzzes nothing.
#[cfg(not(target_os = "linux"))]
compile_error!("this parser is Linux-only; fuzz it on Linux");

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The work count is for the replay gate; here any input is valid.
    let _ = void_box::fuzz::virtio_net(data);
});
