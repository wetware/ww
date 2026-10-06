#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = cell::image::fuzz_composer_dagpb_decode(data);
});
