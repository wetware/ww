# Image Layout

Each wetware image follows a minimal FHS convention:

```
<image>/
  boot/
    kernel.wasm        # PID0 component, required in the final effective FHS
  bin/
    <name>.wasm        # named application component
    status.wasm        # status component used by the shipped Rust PID0
  svc/                 # optional application content; not composed automatically
```

The Host selects PID0 from `boot/kernel.wasm` in the effective application root.
An overlay can omit that path and inherit the installed baseline's kernel.
The shipped Rust PID0 consumes `bin/status.wasm` from the same root. An
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

## Check an effective image

`ww image check [SOURCE[:TARGET] ...]` composes and statically validates the same
effective image that boot preparation uses. It reports the effective root,
selected kernel file, and exact-byte runtime identities without executing PID0.

```sh
ww image check
ww image check ./kernel.wasm:/boot/kernel.wasm ./config:/etc/myapp
ww image check /ipfs/<directory-cid> --json
```

The installed baseline remains the lowest layer. A valid image without a kernel
can inherit the baseline kernel. Invalid heads and invalid selected kernels
fail without substitution. An authoritative Stem check observes one head;
continuous following remains a `ww run` behavior.

Checks can retrieve content and write snapshots, Kubo blocks, pins, and compiled
cache artifacts. Kubo pins remain after checking because the backend does not
distinguish independent owners. Passing does not prove runtime readiness or
application behavior. See the [command reference](cli.md#ww-image-check) for
options, output, cancellation, and retention limits.

## Rust PID0 boot flow

The shipped Rust PID0 reads `bin/status.wasm`, loads the component, registers
`/status`, passes a narrow `Membrane` with `peerId` and `Stat`, and commits kernel readiness. The Rust PID0 does not
evaluate an init directory.

## Mount sources

Mounts can be local paths or IPFS paths. Multiple mounts are merged
as layers (later mounts override earlier ones):

| Form | Example |
|------|---------|
| Local directory | `./std/status` |
| Local file at target | `./kernel.wasm:/boot/kernel.wasm` |
| Local directory at target | `./config:/etc/myapp` |
| IPFS path | `/ipfs/QmAbc123...` |
| IPNS path | `/ipns/k51qzi5uqu5...` |
| Layered | `ww run /ipfs/<directory-cid> ./my-overlay` |

[Wetware Composer v1](composer-v1.md) merges ordinary UnixFS directory DAGs.
It preserves unchanged subtree CIDs and exact UTF-8 names. HAMT directories and
inputs outside the documented profile limits fail explicitly.

Targeted mounts use `source:/guest/path`. Files require explicit targets; root
mounts require directories. Targets must be absolute and canonical. Directory
collisions merge recursively, while other collisions replace the earlier entry.
Strict target ancestors retain metadata and cannot traverse symlinks.

Local sources require explicit `.`, `./`, `/`, or `~/` syntax. Bare values must
be complete CIDs. Local sources are [captured once](local-snapshots.md), verified,
and retained before deployment. Empty directories survive capture; symlinks and
special files fail. Root snapshots exclude `target`, `.git`, `.gitignore`, and
`Cargo.lock`; targeted directory snapshots include those names. The `ww push`
importer described above remains separate.

An `/ipns/...` mount resolves once and freezes the selected immutable CID.
It is distinct from `--ipns-stem`, which follows signed changes and replaces
the active deployment generation. Each generation applies the installed baseline,
authoritative head, namespace images, and original ordered CLI operations. CLI operations
keep highest precedence, including when their source paths change or disappear.

Bare `ww run` captures the installed `~/.ww/fhs` baseline, independently of CWD.
`--fhs-root` or `WW_FHS_ROOT` selects another baseline directory. Explicit mounts
remain higher-priority operations even when they name `~/.ww/fhs` themselves.
The Host selects PID0 exclusively from `/boot/kernel.wasm` in the effective root.
`--kernel` and `WW_KERNEL` are rejected.

A valid image that omits the kernel can inherit the installed path through
ordinary composition. A selected symlink, wrong type, malformed or incompatible
component, oversized file, or corrupt block fails preparation. Missing or
unavailable selected content never triggers an alternate-kernel fallback.
The effective root CID, source-file CID, runtime CID, and authority epoch remain
distinct identities. The persistent Host PeerId does not depend on kernel bytes.

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
    boot/kernel.wasm     # PID0 selected through effective-root composition
    bin/status.wasm      # status component used by the shipped Rust PID0
```

The generated service captures `~/.ww/fhs` as its lowest-priority baseline and
preserves explicit image arguments as CLI overlays. The service reads
`~/.ww/etc/ns` through a separate host-only
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
4. Deployment terminates PID0 while it composes the baseline, head, and frozen
   layers and validates the kernel selected from the resulting root.
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
