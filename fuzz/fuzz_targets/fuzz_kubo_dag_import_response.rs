#![no_main]

use std::sync::LazyLock;

use cid::Cid;
use libfuzzer_sys::fuzz_target;

static EXPECTED_ROOT: LazyLock<Cid> = LazyLock::new(|| {
    "QmUNLLsPACCz1vLxQVkXqqLX5R1X345qqfHbsf67hvA3Nn"
        .parse()
        .expect("fixed fuzz root must be a valid CID")
});

const EXPECT_ACCEPT: &[u8] = b"expect:accept\n";
const EXPECT_REJECT: &[u8] = b"expect:reject\n";

fuzz_target!(|data: &[u8]| {
    let (expected, response) = if let Some(response) = data.strip_prefix(EXPECT_ACCEPT) {
        (Some(true), response)
    } else if let Some(response) = data.strip_prefix(EXPECT_REJECT) {
        (Some(false), response)
    } else {
        (None, data)
    };
    let result = ipfs::parse_kubo_dag_import_response(response, &EXPECTED_ROOT);

    // Persisted CI seeds carry an explicit semantic expectation. Normal fuzz
    // runs leave this mode disabled so mutations continue to probe arbitrary
    // parser input for crashes without treating every changed seed as a bug.
    if matches!(std::env::var("WW_FUZZ_SEMANTIC_REPLAY").as_deref(), Ok("1")) {
        match expected {
            Some(true) => assert!(result.is_ok(), "expected acceptance: {result:#?}"),
            Some(false) => assert!(result.is_err(), "expected rejection"),
            None => panic!("semantic replay seed lacks an expectation marker"),
        }
    }
});
