# Installation and recovery

Run the current shell installer with a running Kubo node:

```sh
sh scripts/install.sh                    # resolves releases.wetware.run
sh scripts/install.sh --version <CID>    # immutable release
```

The installer downloads the Host, `fhs/boot/kernel.wasm`, and
`fhs/bin/status.wasm` to a temporary directory. It requires an exact SHA-256
checksum entry for each file. The candidate Host validates both components
before changing installed files. Kernel validation uses the production kernel
contract. Status validation uses the ordinary Cell contract.

The Host indexes the existing flat standard namespace (`bin/main.wasm` and
`bin/status.wasm`) in the configured Kubo node before installing files. The
namespace uses the explicit package bytes and contains no `boot/kernel.wasm`.
Set `IPFS_API` for a non-default Kubo API and `IPFS_ROUTING_API` for its Routing
V1 gateway. Startup still requires the gateway Routing V1 API.

The installer writes the executable to `~/.ww/bin/ww` and the payload to
`~/.ww/fhs`. Identity, installation metadata, namespace configuration, IPNS
records, watermarks, caches and logs remain outside the FHS. The installer
preserves existing identity. A malformed or unreadable identity causes refusal.

Installation generates a service definition and prints its activation command.
It does not activate an OS service. On macOS:

```sh
launchctl load "$HOME/Library/LaunchAgents/io.wetware.ww.plist"
```

On Linux:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ww
```

Check `http://127.0.0.1:2080/status` after activation. Installation success
means files and service configuration were installed; it does not prove that
the service reached readiness on the operator's machine.

## Existing files

Private `~/.ww/installation.json` records hashes for five managed files:
executable, kernel, status, namespace configuration and service definition.
The installer replaces a file only when its bytes match recorded ownership.
A directly executed regular `~/.ww/bin/ww` is retained. Symlinks at managed
paths or their installation ancestors cause refusal.

Unknown FHS files remain untouched. A collision at a managed path or a custom
service definition causes an actionable refusal. Back up identity, IPNS state,
credentials and custom configuration before manual reinstallation. Historical
installation migration and configuration translation are unsupported.

Each file replacement uses a temporary file in its destination directory and
an atomic rename. The complete installation is **not atomic**. The metadata
records an incomplete update while files are replaced. Rerun the same shell
installer and immutable release CID to recover. Preserve the original identity;
the installer refuses to replace a missing identity in a recorded installation.
There is no automatic rollback or crash-consistent multi-file transaction.
Stop the existing service before replacing its package, then activate it after
successful installation. Do not run multiple installers concurrently.

`ww perform update` revalidates the owned installed payload and refreshes the
managed installation. Explicit installed payloads take precedence over embedded
installation seeds. `ww perform upgrade` fails before changing files and directs
the operator to the current installer.

## Build and container contract

`make fhs` builds both native P3 components and stages `target/fhs` through one
validator. The producer remaps checkout, Cargo cache and pinned Rust sysroot
paths so release WASM does not embed builder-specific absolute paths.
`make package-check` verifies exact producer bytes, invalid payload
refusal, and build-script embedding rules. Release artifacts include the same
FHS next to each Host executable. Standard and std-only releases use Hosts from
the same workflow revision.

`make container-build` builds the canonical FHS before building the Host image.
For a direct `docker build -f Containerfile .` or `podman build`, first run
`make fhs`. The source recipe validates and packages these producer outputs;
it does not independently rebuild WASM. CI supplies the same validated outputs
to every distribution. Independent compiler runs across native platforms are
not assumed to produce identical bytes.

All three Containerfiles place the canonical FHS at
`/usr/share/wetware/kernel`, outside writable `HOME=/home/ww`. Runtime UID/GID
is `1000:1000`. An empty named HOME volume receives the image's owned home
structure. A bind-mounted HOME must supply writable directories with that
ownership. A read-only root also requires writable `/tmp` and HOME. All three
images set `WW_FHS_ROOT=/usr/share/wetware/kernel` and default to `run`.
The configured FHS is the lowest composition layer, including after Stem updates.

## Installed startup

Bare `ww run` captures `~/.ww/fhs` once and never imports the invocation CWD.
Use `--fhs-root <directory>` or `WW_FHS_ROOT` for another baseline directory.
Explicit `ww run .` captures the requested directory as a higher-priority overlay.
Generated services use installed-state startup and retain their explicit
`--namespace-root` and install-time `--ipfs-url`, including nondefault endpoints.

Every accepted epoch composes baseline, Stem head, namespace images, and CLI
operations in that order. The Host verifies and statically validates
`/boot/kernel.wasm` from the final effective root before launch. A valid image
without a kernel can inherit the baseline kernel. Invalid selected content
cannot trigger an embedded or previous-kernel fallback.

Runtime preparation does not modify the installation. Local edits take effect
only on a later Host invocation. Private identity and IPNS state remain outside
the captured FHS. The persistent Host PeerId remains unchanged across kernel
replacements. `--kernel` and `WW_KERNEL` cause an actionable error; replace them
with an explicit targeted mount. Historical selector migration is unsupported.
