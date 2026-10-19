# Committed Cargo lockfiles

Repository builds, tests, and release builds use `--locked`. Cargo must use the
committed dependency graph or fail when resolution requires a lockfile change.
The development `Containerfile` inherits this policy through `make std echo host`.
Artifact-only container builds do not resolve Rust dependencies.

Run the consistency check before a build:

```sh
bash scripts/check_cargo_lockfiles.sh
```

The check discovers tracked `Cargo.lock` paths and resolves each sibling
`Cargo.toml` with `cargo metadata --locked`. Every current owner declares its own
workspace. Full metadata is required: `--no-deps` can accept stale lockfiles.
Metadata does not compile crates or execute build scripts, but can download
registry or Git dependencies. CI runs this check before Rust build jobs restore
compiler caches. A cache hit cannot skip the check.

## Lockfile ownership

The root `Cargo.lock` governs the host workspace, including Chess, Discovery,
`std/system`, and the crates listed in the root `Cargo.toml`.
These standalone workspaces each own a separate sibling `Cargo.lock`:

- `crates/wagi-guest`
- `examples/counter`, `examples/echo`, `examples/oracle`, `examples/snap-hello-rs`
- `fuzz`
- `std/kernel`, `std/status`
- `tests/fixtures/authority-probe`, `tests/fixtures/native-p3`
- `tests/fixtures/routing-key-probe`, `tests/fixtures/stream-lifecycle-probe`
- `tests/fixtures/guest-session-cancel/parent`
- `tests/fixtures/guest-session-cancel/child`

Path dependencies use the consuming workspace's lockfile. For example, Kernel
consumes `std/system` under `std/kernel/Cargo.lock`, while Chess uses the root
lockfile. The cancellation parent and child have independent lockfiles.
A new tracked lockfile must have its owning manifest beside it.

## Intentional dependency updates

1. Edit dependency requirements where needed.
2. Run `cargo update` for the affected workspace. Use `-p <package>` for a
   targeted update. For a standalone workspace, pass its manifest explicitly,
   for example `cargo update --manifest-path std/kernel/Cargo.toml`.
3. Review and commit the manifest and lockfile changes together.
4. Run the consistency check and normal locked builds/tests.

A shared path dependency change can require updates to several consuming
workspaces. The consistency check identifies those workspaces. Do not add
`--locked` to an intentional update command.

## Tool boundaries and review invariant

New repository Cargo build/check/test/clippy/run/metadata commands must use
`--locked` before any `--` separator. Make wrappers inherit the policy of the
actual Cargo invocation. Formatting and cleaning do not resolve dependencies.
Review wrappers and programmatically assembled commands explicitly; a text grep
cannot reliably distinguish these domains or enforce tool-specific forwarding.

Native release builds use `cargo build --release --locked`. Cross release
builds use `cross build --release --locked`; an explicit locked metadata check
runs first because cross's own preliminary metadata call does not forward that
flag. Both consume the root lockfile. See cross's
[metadata implementation](https://github.com/cross-rs/cross/blob/526d03ddcd2f70e683074b3ea6f75ed300f33132/src/cargo.rs#L129-L156).

`cargo-fuzz` 0.13.2 does not accept or forward `--locked` for `build` or `run`.
Arguments after `cargo fuzz run ... --` go to libFuzzer, not Cargo. CI therefore
checks `fuzz/Cargo.lock` through the consistency job before running cargo-fuzz.
This is a tool limitation: the subsequent fuzz build itself does not enforce
locked resolution. Recheck support when upgrading cargo-fuzz. The
[cargo-fuzz 0.13.2 build implementation](https://docs.rs/crate/cargo-fuzz/0.13.2/source/src/project.rs)
and [build options](https://docs.rs/crate/cargo-fuzz/0.13.2/source/src/options.rs)
define the supported forwarding.

`cargo install cargo-fuzz --locked` governs the installed tool's dependency
graph, not `fuzz/Cargo.lock`. Installing cross from Git is also external tool
bootstrap. Neither command defines repository dependency policy.

`ww build` builds user projects. A fresh `ww init` project has no lockfile, so
its first build must resolve dependencies. Repository lock policy does not
change this behavior.
