//! Userspace virtio-vsock: header parsing and the host-side connection state
//! machine that routes guest TX packets.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    void_box::fuzz::vsock_packet(data);
});
