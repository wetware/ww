# Installation and recovery

Provision the release public keyring independently before running the installer.
See [release authentication](release-authentication.md) for the trust format,
key rotation, and the production activation hold. The installer requires
OpenSSL 3.x or 4.x and a running Kubo node:

```sh
sh scripts/install.sh --release <CID>
sh scripts/install.sh --release <CID> --trusted-release-keys /trusted/path/release-keys
```

The installer authenticates the exact checksum manifest with Ed25519 before
trusting any checksum. Missing signatures and unsigned historic releases fail
closed. The default trust path is `/etc/wetware/release-keys`; fetched release
content cannot supply this trust anchor.
Automatic channel selection refuses until trusted channel freshness is integrated.
The shell bootstrap relies on local Kubo for the requested transport CID. The
native `--release` selector additionally verifies the descriptor link and transport
root independently. Manifest authentication alone does not establish that binding.

The installer downloads the Host, `fhs/boot/kernel.wasm`, and
`fhs/bin/status.wasm` to a temporary directory. It requires an exact SHA-256
checksum entry for each file. The candidate Host validates both components
before changing installed files. Kernel validation uses the production kernel
contract. Status validation uses the ordinary Cell contract.

The shell hands the authenticated package to `ww install`. Installation writes
`~/.ww/bin/ww`, `~/.ww/fhs/boot/kernel.wasm`, `~/.ww/fhs/bin/status.wasm`, and a
private ownership receipt. It creates `~/.ww/identity` only for a fresh
installation. Existing valid identity remains unchanged; malformed identity or
missing identity in a recorded installation causes refusal.

Installation does not register a daemon, create `etc/ns/ww`, import a standard
namespace, publish a name, or contact Kubo after the package is staged. Existing
namespace and service configuration remains unchanged. Private identity, keys,
IPNS records, watermarks, caches, logs, and application data stay outside the FHS.

The native bootstrap handoff reauthenticates the staged manifest with the
independent keyring before parsing or installing component payloads.
`ww install --release <CID>` authenticates an immutable release through the
shared release descriptor and installs its selected platform Host and FHS.
`--trusted-release-keys` defaults to `/etc/wetware/release-keys`.
`--check` validates without installing; `--json` prints one versioned result.

## Separate software and service operations

| Command | Effect |
|---|---|
| `ww install` | Create or repair the same recorded Host/FHS package and identity. No service registration or publication. |
| `ww image update --release <CID>` | Authenticate and update only the owned baseline FHS. No Host replacement or restart. |
| `ww upgrade --release <CID>` | Authenticate native selection and enter the #739 engine. Native replacement remains blocked by the operational gates below. |
| `ww upgrade --rollback` | Request retained native recovery through the same engine and admission gates. |
| `ww uninstall` | Remove owned software and daemon registration; preserve user state and recovery evidence. |
| `ww daemon install [RUN OPTIONS] [MOUNT...]` | Register a disabled service using existing identity and startup paths. No image import or identity generation. |
| `ww daemon uninstall` | Stop/unload, disable, verify, back up configuration, then remove owned registration. |

`perform` and all four former subcommands are removed without aliases. There is
no top-level `update`. Deployment following remains `run --stem` or
`run --ipns-stem`; publishers retain authority to advance those sources.

Register a service separately, with the same options as `run`:

```sh
ww daemon install --http-listen 127.0.0.1:2080
```

A fresh registration is disabled and stopped. An identical owned registration
preserves manager state. A different existing definition causes refusal; remove
registration explicitly before reconfiguring. Custom identity must already
exist. Generate a new key at an unused path with `ww keygen --output PATH`.
The service stores absolute local paths and explicit environment-derived options.
There is no daemon-only default HTTP listener.

On Linux, activate explicitly:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ww
ww healthcheck --ready
```

On macOS, use the logged-in GUI domain:

```sh
launchctl enable "gui/$(id -u)/io.wetware.ww"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/io.wetware.ww.plist"
ww healthcheck --ready
```

Registration requires that GUI domain; there is no implicit headless fallback.
The plist and persistent launchd override both disable fresh registration.
Service-manager acceptance does not prove Host readiness.

## Existing files and recovery

`install` refuses a different existing native artifact and directs the operator
to `upgrade`. It refuses a different FHS package and directs the operator to
`image update`. Re-running a newer shell installer cannot bypass these checks.
Same-artifact repair can restore missing owned software; unknown collisions,
symlinks, and altered owned files cause refusal. Unknown FHS files remain intact.

Version-2 `installation.json` names executable and FHS ownership separately.
It retains legacy namespace/service hashes as provenance. Complete version-1
receipts migrate under the installation lock, with `installation-v1.json` kept
as a backup. Incomplete version-1 receipts require the exact recorded package
and original identity; missing legacy artifacts must be restored before migration.
No migration rewrites namespace or service bytes.

Fresh installation uses atomic file replacement plus an incomplete receipt.
The complete installation is not one atomic filesystem operation. Rerun the
same package to recover an interrupted installation. Managed startup rejects
incomplete ownership and pending FHS updates.

`image update` runs with the installed Host so production static validation uses
that Host's engine. It retains both complete baseline generations under
`.fhs-recovery/` and records `image-update.json` before replacing either file.
Startup holds `~/.ww-installation.lock` through managed baseline capture. A
running Host keeps its previously captured baseline.

After interruption, repeat `ww image update`. Recovery finishes the recorded
target when retained bytes verify, otherwise restores the retained previous
baseline. Recovery never selects another release in the same invocation. If
neither generation is available, startup stays closed and recovery evidence
remains. Unknown changed files cause refusal. Namespace configuration and signed
state are never part of FHS rollback. `image-update-retained.json` identifies the
retained transaction after completion.

The result includes `previous_fhs` and selected `fhs` kernel/status BLAKE3 hashes.
If recovery restores the previous generation without a recorded release CID,
`release` is null; the exact selected hashes still identify the restored bytes.

The result names `~/.ww/fhs` and reports activation as false. Namespace or CLI
overlays can shadow changed baseline paths. Existing namespace configuration,
including legacy `etc/ns/ww` publication targets, remains unchanged. A custom
`WW_FHS_ROOT` or service baseline remains an external packaging responsibility.
Activate the managed baseline with a separate start or restart when intended.

All software and registration mutations share `~/.ww-installation.lock`.
Competing operations refuse. Keep the lock inode in place; the lock can remain
after a refused preflight. Pending Host recovery blocks FHS, install, and removal
operations; pending FHS recovery blocks native upgrade and managed startup.

`uninstall` never recursively removes `~/.ww`. It verifies software ownership,
removes owned service registration with checked lifecycle results, and deletes
only the three software paths. Service removal retains an exact configuration
backup. Stop/unload failure preserves registration. The shell uninstaller only
delegates to the installed `ww uninstall`; it has no separate purge behavior.

For a pending Host transaction whose installed executable cannot launch, use:

```sh
sh scripts/install.sh --recover
sh scripts/install.sh --recover --release <RECORDED_RELEASE_CID>
```

Recovery requires an existing `host-upgrade.json`. The default locator is the
untrusted `installed-release.json`; supply the exact recorded release explicitly
when that locator is unavailable. The shell authenticates and stages only the
Host, then invokes the native recovery-only entry point. The native entry point
holds the installation lock, reauthenticates the typed release, verifies its own
executable bytes, and matches the recorded generation before calling `Engine::recover`.
It never falls through to fresh installation or new-release selection.

The shell uses the configured Kubo CLI for staging. Native reauthentication uses
`--ipfs-url`, `IPFS_API`, or `http://localhost:5001`, in that order. Configure both
endpoints when using a nondefault Kubo. The recorded release must contain a Host
with this recovery entry point. Missing, incompatible, or unrelated artifacts
cause refusal. Recovery preserves the native operational gates below.

## Native upgrade integration gates

`upgrade` now consumes authenticated descriptor selection and the #739 engine.
A new `install --release` stores an untrusted release-CID locator; every upgrade
reauthenticates that release and verifies the installed bytes. Legacy and local
bootstrap installations without that locator cannot substitute ownership hashes
for release authentication.

Native replacement remains unavailable: `NativeService` refuses candidate
compatibility and live restart admission. No CLI flag bypasses those checks.
Exact authenticated no-ops and permitted recorded recovery can complete without
replacement. Automatic channel discovery also remains unavailable until an
independently trusted raw IPNS channel identity is provisioned; explicit
`--release CID` does not lower a channel freshness floor.

Retained signed evidence, atomic native-provenance transition, effective-deployment
compatibility, child-side startup admission, exact running artifact/readiness,
and measured restart deadlines remain integration gates. See
[native Host replacement](native-upgrades.md). The installer is not an upgrade
fallback, and CLI implementation does not authorize a production rollout.

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

Run `ww image check` to validate installed/configured image composition and
PID0 static compatibility before startup. Use the service's `--fhs-root`,
`--namespace-root`, and `--ipfs-url` values when the service overrides defaults.
The check does not install or restart services. It can import content and retain
Kubo pins; see [image check](cli.md#ww-image-check). A passing check does not
replace runtime readiness checks.

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
replacements.
