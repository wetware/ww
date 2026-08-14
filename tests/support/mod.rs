pub mod atom;
pub mod terminal;

// Parity row 29a is intentionally a documented baseline gap in PR-3b. The
// legacy Glia kernel runs `std::system::run_with_session`, whose
// transport driver logs RPC/bootstrap death without returning that outcome to
// `std/kernel-glia::run_impl`; the WASI command therefore exits 0 accidentally.
// There is no external operation that closes pid0's private bootstrap stream
// without also killing the host, and adding a production seam or test flag is
// forbidden for this baseline. PR-4 must implement row 29b and prove transport
// death outside clean shutdown exits nonzero.
