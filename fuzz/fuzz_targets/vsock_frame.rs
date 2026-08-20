//! Control-channel frame decoding: `Message` framing plus the multiplex
//! request-id prefix. Runs on any platform.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The work count is for the replay gate; here any input is valid.
    let _ = void_box::fuzz::vsock_frame(data);
});
