#![no_main]

use libfuzzer_sys::fuzz_target;

mod support;

fuzz_target!(|data: &[u8]| {
    let parsed = ipfs::parse_kubo_mfs_ls_response(data);
    support::check_mfs_ls_response(data, parsed.as_deref().ok());
});
