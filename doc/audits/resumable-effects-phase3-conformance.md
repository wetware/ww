# Resumable-effects Phase 3 conformance audit

## Executive summary

This audit covers local Glia interposition only; capability authority remains enforced at the Cap'n Proto hook membrane layer.
Of 15 enumerated invocation surfaces, 5 are **CONFORMANT**, 6 are **VIOLATION**, and 4 are proposed **ACCEPTED-EXCEPTION** cases.
The most important finding is that bare `(load "path")` is a filesystem/IPFS read that reaches host dispatch directly, whereas `(perform :load "path")` is interposable.
That bypass is present in the kernel, remote shell, local CLI shell, and MCP evaluator, and is used extensively by shipped examples and documentation.
The kernel's unknown-command fallback is a second, more consequential bypass: a bare list head reads `PATH` and files, loads and spawns a WASM process, reads its stdout, and waits for it without a `perform`.
`println` writes stdout directly from the evaluator and is likewise not interposable.
All capability operations reached through `(perform ...)` (including the handlers' internal RPC calls) are conformant.
Subject to the accepted REPL/diagnostic exceptions below, the invariant does **not** substantially hold for filesystem/process/stdout operations today.

## Surface inventory

| Surface / operation | Verdict | Rationale |
| --- | --- | --- |
| Keyword `(perform :load path)` | **CONFORMANT** | `perform_dispatch` walks the active handler stack; the standard wrapper supplies the default filesystem implementation. |
| Capability `(perform cap :method ...)`, including `host`, `runtime`, `routing`, `import`, executor/process/http and grafted extras | **CONFORMANT** | The evaluator's `perform` path checks a dynamically installed handler before a carried cap handler/RPC adapter. |
| Kernel host/runtime/routing handlers | **CONFORMANT** | Their network, process, and routing RPCs execute only after the corresponding cap-targeted `perform` reaches `make_*_handler`. |
| Import handler's module read | **CONFORMANT** | `(perform import "...")` reaches `make_import_handler`; its `eval_load_async` call is implementation behind that performed effect. |
| Kernel bare `(load path)` | **VIOLATION** | Generic evaluator dispatch invokes `eval_load`, which calls `std::fs::read`, without walking the handler stack. |
| Kernel bare unknown command `(cmd args...)` | **VIOLATION** | Dispatch fallback reads `PATH` and candidate WASM files, then loads/spawns/waits on a process and reads stdout directly. |
| Evaluator `(println ...)` | **VIOLATION** | A synchronous builtin calls `std::println!` directly; no effect is performed. |
| Remote shell bare `(load path)` | **VIOLATION** | `ShellDispatch` routes it to `caps::eval_load_async` without `perform`. |
| Local CLI shell bare `(load path)` | **VIOLATION** | `LocalShellDispatch` routes it to `eval_load_async`; its backend reads host files or calls IPFS directly. |
| MCP Glia evaluator bare `(load path)` | **VIOLATION** | `McpDispatch` has the same direct `eval_load_async` table entry. |
| Kernel `(cd path)` | **ACCEPTED-EXCEPTION (proposed)** | It mutates only evaluator session state and does no filesystem operation; retaining it as shell-local state is defensible. |
| Kernel `(exit)` | **ACCEPTED-EXCEPTION (proposed)** | It terminates the interactive kernel process directly; this is a REPL lifecycle convenience rather than program capability work. |
| Remote/local shell `(exit)` | **ACCEPTED-EXCEPTION (proposed)** | It returns an `:exit` sentinel to the serving loop, a defensible interactive-session control operation. |
| `(help)` in kernel/shell/MCP dispatch tables | **ACCEPTED-EXCEPTION (proposed)** | It only returns static diagnostic text and has no outside-world side effect. |
| Standard `wrap_with_handlers` wrappers | **CONFORMANT** | Kernel and shared-caps wrappers install `:load` plus the applicable cap handlers around each evaluated user form. |

## Violations

### Bare filesystem/IPFS loading

**Call path.** In either interpreter, an unresolved list head reaches `Dispatch::call` at `crates/glia/src/eval.rs:2366` (analyzed forms) or `:2864` (Val/raw-body re-evaluation); `apply` has equivalent paths at `:2417` and `:2801`.  Kernel dispatch selects `load` at `std/kernel/src/lib.rs:498-501`, then `eval_load` performs `std::fs::read` at `:526-551`.  The remote shell, local shell, and MCP evaluator instead select `caps::eval_load_async` (`std/shell/src/lib.rs:83-94`, `src/cli/shell.rs:670-690`, and `std/mcp/src/lib.rs:498-504`), which ultimately reads a filesystem backend at `std/caps/src/lib.rs:115-127`.  The local shell backend also makes an IPFS capability call directly for `/ipfs` or `/ipns` paths (`src/cli/shell.rs:730-740`).

**Interposition evidence.** The following form evaluates the bare dispatch call, not the installed `:load` handler. With an existing readable path it returns bytes; with a missing path it returns a `load:` error. In neither case does it throw `intercepted`:

```clojure
(with-effect-handler :load
  (fn [path resume] (throw "intercepted"))
  (load "/path/to/file"))
```

The converse is covered by the kernel test `test_perform_load_resolves_through_effect_handler` (`std/kernel/src/lib.rs:3206-3230`): `(perform :load path)` reaches the standard handler installed by `wrap_with_handlers` (`:1394-1422`). The wrapper's default handler then calls bare `load` only *after* the effect has been intercepted, which is conformant for that performed entry point.

**Repository evidence.** Bare `load` is documented and used by shipped code, including `README.md:29`, `std/status/etc/init.d/05-status.glia:14`, and the example registration scripts under `examples/*/glia/`. Examples also instruct interactive users to load Glia scripts directly. The current `doc/capabilities.md:123` explicitly presents `(load "path")` as the content-read form.

**Fix shape.** Make the public `(load path)` spelling perform `:load` (or make it an evaluator special form that does so), and give the default `:load` handler a private/raw filesystem primitive so it does not recurse. Keep the raw primitive out of normal Glia name resolution. Apply the same routing rule to all kernel, shell, local-shell, and MCP dispatch tables; this preserves the familiar spelling while making mocking/auditing possible.

### Kernel unknown-command fallback

**Call path.** The same four evaluator generic-dispatch paths named above call `KernelDispatch::call` (`std/kernel/src/lib.rs:576-588`). A table miss enters `eval_path_lookup` at `:1220`. It reads `PATH` (`:1230`), reads `<dir>/<cmd>.wasm` or `<dir>/<cmd>/main.wasm` (`:1232-1239`), calls `runtime.load`, `executor.spawn`, reads process stdout (`:1240-1296`), and waits for exit (`:1298-1316`). None of these operations is preceded by `perform`.

**Interposition evidence.** This shows that the handler stack is not consulted even on a failed lookup (and therefore cannot observe the environment/filesystem probes):

```clojure
(with-effect-handler runtime
  (fn [data resume] (throw "intercepted"))
  (definitely-no-such-command))
```

It returns `definitely-no-such-command: command not found`, rather than `intercepted`. With a test image containing `/bin/demo.wasm`, replace the final form with `(demo)`; it spawns the executable while the same handler remains unfired.

**Fix shape.** Replace implicit path execution with an explicit `(perform :command cmd args...)`/`:host-command` effect whose default installed handler owns PATH resolution and process orchestration. Alternatively remove the fallback and require an explicit capability-oriented command API. In either shape, do not make the handler stack an authority boundary.

### `println` stdout builtin

**Call path.** Both interpreter paths call `eval_builtin` before generic dispatch (`crates/glia/src/eval.rs:2360-2366` and `:2857-2864`). Its `println` arm writes with `std::println!` at `:1222-1235` and returns `nil`.

**Interposition evidence.** `audit-output` is printed and the form returns `nil`; the `:stdout` handler is never invoked:

```clojure
(with-effect-handler :stdout
  (fn [data resume] (throw "intercepted"))
  (println "audit-output"))
```

This is not limited to the REPL: `std/lib/ww/test.glia:83-92` uses `println` for test results, so a test program cannot locally suppress, capture, or audit its own output through the established handler mechanism.

**Fix shape.** Route public `println` through a `:stdout` effect and install a default output handler in every evaluator embedding. A private raw printer can remain available to bootstrap/diagnostic code; if maintainers prefer direct REPL output, document it as an accepted exception instead.

### Shared impact of evaluator fallback paths

The dispatch sites at `crates/glia/src/eval.rs:2366`, `:2417`, `:2801`, and `:2864` are not independently outside-world operations; they are the common bypass mechanism. They are reachable respectively through analyzed unresolved calls, analyzed `apply`, Val/raw-body `apply`, and Val/raw-body unresolved calls. Consequently every violation above is also reachable through the appropriate `(apply 'load [path])` or `(apply 'cmd [args])` form where the embedding exposes that dispatch target. Any fix must cover all four paths, rather than only the main analyzed-call path.

## Accepted exceptions proposed for maintainer decision

`help` only constructs static text. `cd` only assigns `Session.cwd` (`std/kernel/src/lib.rs:554-562`) and performs no I/O. Kernel `exit` calls `std::process::exit` directly (`:505-510`), while the shell variants return an `:exit` sentinel (`std/shell/src/lib.rs:89-93`; `src/cli/shell.rs:683-690`). These are reasonable interactive-control and diagnostics exceptions, provided they remain clearly scoped to evaluator/REPL lifecycle rather than reusable application operations.

## Method

I read the authority-layering design and capability documentation first, then enumerated both Glia evaluator dispatch paths, all `eval_builtin` arms, kernel/caps handler factories, pre-installed handler wrappers, kernel command dispatch, remote shell, local shell, and MCP evaluator dispatch. I searched shipped Glia, examples, docs, and CLI templates for bare and performed spellings. I traced each candidate to its filesystem, environment, stdout, process, IPFS, or Cap'n Proto call and used handler forms above as repeatable interposition probes. Existing tests were run with `cargo test -p glia --lib eval::tests -- --nocapture` (365 passed), `cargo test -p caps --lib -- --nocapture` (31 passed), and `cargo test --manifest-path std/kernel/Cargo.toml --lib test_perform_load_resolves_through_effect_handler -- --nocapture` (1 passed).

## Out-of-scope observations

`cd` currently has no observed consumer beyond assigning `Session.cwd`: the kernel's `eval_load` resolves relative paths against `/` and `eval_path_lookup` uses `PATH`, neither reading that field. This appears inconsistent with the help text's "Change working directory" description, but is a behavior issue rather than a Phase 3 interposition question and was not changed.
