# Image Layout

Each wetware image follows a minimal FHS convention:

```
<image>/
  bin/
    <name>.wasm        # named application component
    status.wasm        # status component used by the shipped Rust PID0
  svc/                 # optional application content; not composed automatically
```

The Host selects PID0 independently from the effective application root. The
shipped Rust PID0 consumes only `bin/status.wasm` from that root. An
application-specific composition selects its own component paths. The shipped
Rust PID0 does not automatically load components produced by `ww build`.

## Build and publication

The image lifecycle is:

```text
source project -> ww build -> canonical image tree -> ww push -> published content root
```

For a generated project, run these commands from the repository root after
installing the [native P3 toolchain](cli.md#native-wasi-p3-toolchain) and starting
Kubo:

```sh
cd examples
ww init greeter
ww build greeter
ww push greeter
```

The scaffold uses `../../std/system` and `../../capnp`. Build compiles and
validates the project's single WASM artifact, then writes `greeter/bin/greeter.wasm`.
The resolved project directory name determines the filename. Building from inside
`greeter` with `ww build` produces the same path. No `boot/` directory, `main.wasm`
alias, image manifest, or image-root CID is generated.

The project directory is the image tree. `Cargo.toml`, `build.rs`, source files,
the Cap'n Proto schema, and any supporting content remain ordinary files there.
`target/` holds intermediate build output.

Push imports files using the existing local-image importer. The importer excludes
entries named `target`, `.git`, `.gitignore`, and `Cargo.lock` at every depth.
Other regular files, including project source and configuration, are included.
Empty directories and special files are omitted. Symlinks are followed; Unix
metadata is not preserved. Keep private files outside the image tree.

Push requires at least one `bin/*.wasm` file and accepts multiple named components.
It does not modify the input tree or generate executable aliases. The `CID:` line
reports Kubo's wrapped UnixFS directory-root CID, which identifies the imported
paths and file content. Kubo's default import behavior pins this root. Repeated
imports of unchanged content under the same Kubo settings yield the same root.

The directory-root CID differs from the identity of a contained executable.
`Executor.bytecodeCid()` identifies only the loaded WASM bytes using
CIDv1/raw/BLAKE3-256. Kubo's file CID uses its own import encoding. A deployment
can compose this directory with other layers into a different effective root.
Push neither creates a deployment nor updates Atom or IPNS.

`ww run /ipfs/<root-cid>` mounts the published tree. Component loading remains
the responsibility of the selected PID0 or application composition. Publication
alone does not make the shipped Rust PID0 launch `bin/greeter.wasm`.

## Rust PID0 boot flow

The shipped Rust PID0 reads `bin/status.wasm`, loads the component, registers
`/status`, passes a narrow `Membrane` with `peerId` and `Stat`, and commits kernel readiness. The Rust PID0 does not
evaluate an init directory.

## Mount sources

Mounts can be local paths or IPFS paths. Multiple mounts are merged
as layers (later mounts override earlier ones):

| Form | Example |
|------|---------|
| Local path | `std/status` |
| IPFS path | `/ipfs/QmAbc123...` |
| IPNS path | `/ipns/k51qzi5uqu5...` |
| Layered | `ww run /ipfs/QmBase my-overlay` |

[Wetware Composer v1](composer-v1.md) merges ordinary UnixFS directory DAGs.
It preserves unchanged subtree CIDs and exact UTF-8 names. HAMT directories and
inputs outside the documented profile limits fail explicitly.

Targeted mounts (`source:/guest/path`) are not accepted by backend virtual mode.
An `/ipns/...` mount is an image layer that Kubo resolves during composition.
It is distinct from `--ipns-stem`, which follows signed changes and replaces
the active deployment generation.

## Installed host layout

`ww perform install` and `ww perform update` separate private host state from
the local image root:

```
~/.ww/
  identity              # private node identity
  etc/ns/               # host-only namespace configuration
  logs/                  # host logs
  run/                   # host runtime state
  ipns/
    follow/*.record      # durable signed follower ordering floors
    publish/*.record     # durable signed records for the default publisher
  fhs/                   # publishable local image root
    bin/status.wasm      # status component used by the shipped Rust PID0
```

The generated service recursively imports only `~/.ww/fhs` and explicit image
arguments. The service reads `~/.ww/etc/ns` through a separate host-only
configuration argument. The service does not mount `~/.ww` as an image.

Put intentional local image content under `~/.ww/fhs`, or pass an explicit
image path to `ww daemon install`. Do not put private host state in an image
root because Kubo can store, pin, and provide every imported file.

## Stem coordination

The `--stem` flag connects to an Atom contract on an EVM chain.
The contract holds a monotonic head pointer (an IPFS CID). When the
head is updated:

1. The Atom Source reads the chain tip.
2. It reads `Atom.head()` at `tip - confirmation_depth`.
3. Deployment advances its host-local epoch and publishes `root: None`.
4. Deployment terminates PID0 while it prepares the head plus frozen layers.
5. Deployment waits for teardown, swaps `CidTree`, publishes the rooted epoch,
   and starts the replacement.

Contract events are not required for correctness. The boot head and later
updates use the same finalized-depth rule. Atom's contract sequence remains
private to the Source and does not become `Epoch.seq`.

This provides a coordination primitive across trust boundaries:
multiple independent nodes watching the same contract will
synchronize their agent lifecycle to the same on-chain state.

`--ipns-stem <name>` selects an IPNS record instead of an Atom contract. The
two flags are mutually exclusive. Wetware fetches the raw signed record through
Kubo HTTP Routing V1 and validates it locally. A valid new `/ipfs/<cid>` value
uses the same revoke, prepare, swap, and replacement path described above.

The signed record's EOL is authoritative. Transport failures before EOL keep
the current image active. EOL without a valid replacement emits
`InvalidHead`, removes the rooted epoch, and terminates PID0. An expired
persisted value does not seed boot, although its signed ordering floor remains
durable. See [deployment.md](deployment.md#ipns-stem) for ordering and recovery
semantics.
