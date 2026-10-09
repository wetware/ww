# Guest RPC session cancellation

This composed parent/child fixture uses the real `std/system::RpcSession`,
`run`, and `serve` implementations with the production P3 transport linker.
The parent drops a generated asynchronous child import, invoking actual
Component Model subtask cancellation while its Store and sibling task survive.

Install the pinned P3 toolchain described in
[the native P3 fixture documentation](../native-p3/README.md), then build and run:

```sh
WASI_SDK_PATH=/path/to/wasi-sdk-34.0 \
WASM_TOOLS=/path/to/wasm-tools \
make test-guest-session-cancel
```

The script requires SDK 34.0, nightly-2026-08-30 with rust-src, and wasm-tools
1.258.0 as documented by `scripts/build_wasip3_component.sh`. Missing artifacts,
failed normal controls, or any cancellation assertion fail the invoked lane.
`make guest-session-cancel-probe` builds
`target/guest-session-cancel/composed.wasm`; set
`WW_RPC_SESSION_P3_FIXTURE` to that artifact to run the ignored `p3_guest_session_`
cell tests separately.

Four scenarios cover an unpolled raw session, driven `run`, driven `serve`, and
partial extraction of the public RpcSystem/client fields. Both normal and
cancelled cases retain real ordinary responses, a response pipeline, a promised
capability, calls on that promised capability, and the bootstrap import. The
native peer retains the exported capability and leaves its response pending;
a later ordered RPC reply establishes readiness. Inspection after cancellation
must find all retained work already terminal and all tracked owners destroyed.

The host starts a second actual RPC session with its result handle and release
gate held outside the victim's event loop. It completes another RPC after victim
cancellation, then enters a fresh real RPC session in the same Store. Each new
session receives a fresh fixture-only grant after the previous grant's endpoint
was consumed. Production's single-open-per-grant contract is unchanged.

Observer events prove readiness, synchronous destruction/terminality before
cancellation returns, no cancelled victim/application repoll, late-waker safety,
sibling progress, and Store reuse. Guest observations use component globals;
ordering uses component futures and RPC barriers, with timeouts only as failure
watchdogs. No cleanup surrogate or guest driver worker is used.
