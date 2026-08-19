//! Split-virtqueue reader: descriptor-chain walking over guest memory, with
//! guest-chosen queue geometry.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    void_box::fuzz::virtqueue(data);
});
