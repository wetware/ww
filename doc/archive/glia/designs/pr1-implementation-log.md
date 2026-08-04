# PR-1 implementation log — checkpoint 1 (glia core complete)

## Glia-core type/signature diff (summary)

`crates/glia`: 5 files, +1487/−666.

- `lib.rs`: `Val` loses `Recur`/`Effect`/`Resume`; `Val::Cap(CapHandle)` with private fields + accessors (`name/schema_cid/id/inner/effect_target`); opaque `CapId` (private mint, `next_cap_id` privatized); `Fault{kind,payload}` + `FaultKind{Language,Runtime}`; opaque `NativeSignal` (`throw`/`fault` public ctors, `resume` crate-private; `From<Val|String|&str|GliaError>` → throw); `NativeFnImpl`/`AsyncNativeFnImpl` err type = `NativeSignal`.
- `effect.rs`: `EffectTarget::Cap{name,schema_cid,id:CapId}`; new `EffectRequest{target,data}` + `effect_type()`; `make_resume_fn` emits `NativeSignal` (second resume = catchable throw).
- `error.rs`: `tag::INVALID_RECUR` + `GliaError::InvalidRecur` ("recur not in tail position"); `unwrap_thrown(&EvalError)` shim; control arms dropped from `val_type_name`.
- `eval.rs`: crate-private `Flow{Value,Recur}` + `Control{Fault(Box),Unhandled(Box<EffectRequest>),Resume}`; internal `throw(hs,payload)` performs `:glia.exception` via `perform_dispatch`; `settle_native` at every native/Dispatch chokepoint; tail-position discipline (`into_value(ctx)` at all non-tail sites — compile-enforced); loop/fn rebinds on `Flow::Recur`; `with-effect-handler` machine on `Control`; `Dispatch::call` err = `NativeSignal`; public boundary `EvalError{Fault,Unhandled}` with peeled Display; `eval_toplevel*` → `EvalError`; `eval`/`eval_expr` → `pub(crate)`; sync helpers keep `Result<T, Val>` payload lanes.
- `expr.rs`: control arms removed from free-var walk and `analyze` Const arm.

Tests: 682/682 pass, including new contract groups (catchability of arity/type/div-zero/native errors; structural abortiveness; try-resume over builtin errors; language-fault bypass of `try` for all three non-tail-recur pathologies; tail recur; depth-limit catchable; boundary display strings; missing-key Nil pins).

## Compiler errors remaining by crate

- crates/glia: 0 (682 tests green)
- std/caps: 8 (call sigs ×2, unwrap_thrown ×2, call_resume, async handler blocks ×3) — masks downstream counts
- std/kernel, std/shell, ww(cli): blocked behind caps; migration next

## Deviations from the approved contract (none requiring approval)

1. `oneshot::Receiver::Output` kept `Result<Val, Val>`; the abandon→`Fault::runtime` conversion happens at its single await site (`perform_dispatch`). Observable semantics identical to the contract line item.
2. `Control`'s `Fault`/`Unhandled` payloads are `Box`ed and the non-tail value-demand is inlined (no wrapper future): required to keep evaluator stack depth at parity with master (pre-fix, deep prelude-macro tests overflowed the 2MiB test stack; post-fix they pass at master's budget). Internal only.
3. Added `#[derive(PartialEq)]` to `EvalError`/`Fault`/`EffectRequest`/`EffectTarget` (test ergonomics; `EffectTarget::matches` untouched).

Watch item (not a deviation): evaluator stack usage is slightly above master even after mitigation; re-verify under wasm (kernel) target checks in final verification.

---

# Checkpoint 2 — DONE (all crates migrated, all checks green)

Changed files (11, +2110/−945): crates/glia/{lib,effect,error,expr,eval}.rs; std/kernel/{lib,attenuate}.rs; std/caps/{lib,mcp_adapter}.rs; std/shell/lib.rs; src/cli/shell.rs. TODOS.md untouched. Nothing committed.

Verification: workspace tests 1367/0 (incl. glia 682, ww bin/integration, examples); kernel 91/0; caps 34/0; clippy 0 warnings introduced (std/shell's 9 are pre-existing on master); fmt clean; wasm32-wasip2 checks clean for kernel/shell/status/glia; zero references to Val::Recur/Effect/Resume anywhere (code or comments).

Post-checkpoint-1 additions (all within contract intent):
- `NativeSignal::thrown()` read-only accessor (embedder tests/wrappers inspect throw payloads; grants no construction/forgery ability).
- `NativeSignal::map_throw()` (kernel membrane gate rewrites denial payloads without inspecting or forging control signals; replaces the old stringify-everything `map_membrane_denial` pass-through).
- Stack-depth mitigation retained (boxed Control payloads + inlined non-tail value demands); deep-macro tests pass at master's 2MiB budget.
- `EvalError` is `#[allow(clippy::result_large_err)]` (cold boundary type).
