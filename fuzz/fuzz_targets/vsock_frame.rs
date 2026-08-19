//! Control-channel frame decoding: `Message` framing plus the multiplex
//! request-id prefix. Runs on any platform.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    void_box::fuzz::vsock_frame(data);
});
