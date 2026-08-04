#![no_main]
use libfuzzer_sys::fuzz_target;
mod common;

fuzz_target!(|data: &[u8]| {
    common::run_sequence(data, false);
});

