# Installation and recovery

The canonical installation command is:

```sh
curl -fsSL https://wetware.run/install.sh | sh
```

The public redirect still serves the explicit-CID installer pending review and
deployment approval. Use `sh scripts/install.sh` from this revision to run the
candidate locally. Both paths require a configured Kubo CLI and running daemon,
Python 3, and OpenSSL 3.x or 4.x. Use a Kubo version that supports
`ipfs name get` and `ipfs name inspect --verify`, such as 0.43.1. On macOS, check `openssl version` if the
system provides LibreSSL. The R1 Linux verifier requires glibc 2.39 on ARM64 and glibc 2.38 on x86-64.

The installer fetches the signed record for the fixed `ww-release` IPNS
identity with `ipfs name get`. Local Kubo inspects the signature and validity
against that identity. Only a direct immutable release CID is eligible.
To select a known CID during a discovery outage, run:

```sh
sh scripts/install.sh --release "$RELEASE_CID" --trusted-release-keys /trusted/path/release-keys
```

For automatic bootstrap, the installer uses the keyring selected by
`--trusted-release-keys`, then a provisioned `/etc/wetware/release-keys`.
Only when neither is configured does it stage the embedded R1 public key in
its temporary directory. A configured keyring that fails verification stops
installation. Explicit CID, recovery, and manifest-only modes require a
provisioned keyring. See [release authentication](release-authentication.md).

A first-time installer can receive an older, still-valid IPNS record. A valid
signature and expiry check cannot prove that the record selects the globally
latest release. Shell installation creates no IPNS or release sequence floor.
If `~/.ww/release-consumer` exists, use native discovery to enforce its floors.

The shell authenticates the manifest, binds it to the requested transport root,
and verifies SHA-256 for the staged Host, kernel, and status files. Automatic
installation also checks the signed descriptor binding, descriptor block, and
channel against the selected root. The HTTPS
installer pins the SHA-256 of an independently verified R1 verifier for each
platform. For a subsequent release, the installer retrieves the verifier from
the immutable R1 root. R1 must therefore remain available. The pinned verifier
runs `ww install --release`, authenticates the typed descriptor, selects the
compatible Host and FHS, and checks the reconstructed bytes before installation.
The installer runs the installed Host with `--version` as a final check. The success summary shows the installation location and the full release CID.

The shell hands the authenticated package to `ww install`. Installation writes
`~/.ww/bin/ww`, `~/.ww/fhs/boot/kernel.wasm`, `~/.ww/fhs/bin/status.wasm`, and a
private ownership receipt. It creates `~/.ww/identity` only for a fresh
installation. Existing valid identity remains unchanged; malformed identity or
missing identity in a recorded installation causes refusal.

Installation does not register a daemon, create `etc/ns/ww`, import a standard
namespace, or publish a name. Existing namespace and service configuration
remains unchanged. Private identity, keys,
IPNS records, watermarks, caches, logs, and application data stay outside the FHS.

`ww install --release <CID>` authenticates an immutable release through the
shared release descriptor and installs its selected platform Host and FHS.
`--trusted-release-keys` defaults to `/etc/wetware/release-keys`.
`--check` validates without installing; `--json` prints one versioned result.

The shell stages bytes with the configured `ipfs` CLI. Native installation and
recovery use the Kubo HTTP endpoint selected by `--ipfs-url`, then `IPFS_API`,
then `http://localhost:5001`. The shell's `--ipfs-url` is forwarded to those
native calls; it does not reconfigure the shell's `ipfs` CLI. Configure both
paths when using a nondefault Kubo, for example:

```sh
# First configure the ipfs CLI to reach the intended Kubo daemon.
sh scripts/install.sh --release <CID> --ipfs-url http://127.0.0.1:55001
```

## Find `ww` on PATH

After a successful installation, `scripts/install.sh` checks whether `~/.ww/bin` is
on `PATH`. If the directory is missing, an interactive installation can offer
to update a startup file for your shell. The prompt reads from the controlling
terminal, including when the script arrives through a pipe. Press Enter to
decline. The prompt requires a foreground job started by a shell with job
control. A 15-second timeout or end of input also declines. Set
`WW_NONINTERACTIVE=1` to suppress the prompt in automation.

| Shell | Startup file for future sessions |
|---|---|
| Zsh | `.zshrc` under `ZDOTDIR`, or under your home directory by default. |
| Bash | On macOS, the first existing login file among `~/.bash_profile`, `~/.bash_login`, and `~/.profile`, or a new `~/.bash_profile` if none exists. On Linux, `~/.bashrc` for interactive shells. |
| Fish | `fish/conf.d/ww-path.fish` under `XDG_CONFIG_HOME`, which defaults to `~/.config`. |
| Ksh | An existing `.kshrc`, `kshrc`, or `.mkshrc` selected by a literal absolute `ENV` path under your home directory. Paths with shell syntax require manual setup. |
| POSIX sh, Dash, BusyBox Ash | Manual. The startup file depends on how the shell starts and how the system configures it. |
| Csh, Tcsh | Csh uses `~/.cshrc`. Tcsh uses `~/.tcshrc` when it exists and `~/.cshrc` otherwise. |
| Nushell | Manual. Run `$nu.config-path` to locate the active configuration file. |

The installer shows the proposed destination before asking for consent.
Automatic edits require a destination under your home directory. If the installer
declines a destination, use the printed command in your shell. After approving an edit, use `~/.ww/bin/ww run` in your
current shell or open a new terminal and use `ww run`. For an unrecognized shell, run `~/.ww/bin/ww` directly.

## Separate software and service operations

| Command | Effect |
|---|---|
| `ww install --release <CID>` | Create the authenticated Host/FHS package and identity, or repair the exact recorded package. No service registration or publication. |
| `ww image update --release <CID>` | Authenticate and update only the owned baseline FHS. No Host replacement or restart. |
| `ww upgrade --release <CID>` | Authenticate native selection through `UpgradeCoordinator`; changed operations require an explicit timeout and supported managed service. |
| `ww upgrade` | Resolve the configured release IPNS identity, authenticate selection and persist freshness floors, then use the same native coordinator. |
| `ww upgrade --rollback` | Restore the authenticated retained native generation through the same coordinator. |
| `ww uninstall` | Remove owned software and daemon registration; preserve user state and recovery evidence. |
| `ww daemon install [RUN OPTIONS] [MOUNT...]` | Register a disabled service using existing identity and startup paths. No image import or identity generation. |
| `ww daemon uninstall` | Stop/unload, disable, verify, back up configuration, then remove owned registration. |

`perform` and all four former subcommands are removed without aliases. There is
no top-level `update`. Deployment following remains `run --stem` or
`run --ipns-stem`; publishers retain authority to advance those sources.

For native upgrade discovery, independently configure the `ww-release` IPNS identity with
`WW_RELEASE_IPNS` or `--release-ipns <NAME>`. The native command requires this configuration.
The signing keyring remains independently provisioned through
`--trusted-release-keys` (default `/etc/wetware/release-keys`). Raw signed records
come from `--ipns-routing-url` / `IPFS_ROUTING_API`; Kubo must expose Routing V1.
Release content comes from `--ipfs-url` / `IPFS_API`.

`ww upgrade --check` authenticates and inspects without replacing the Host or
starting services. Channel checks can advance the existing `Consumer` watermark
in the private `~/.ww/release-consumer` directory. Both IPNS and signed-release
sequence floors survive process restarts, expiry, and artifact-download failures.
Discovery accepts authenticated observations under those floors; it cannot prove
the globally latest release. There is no background polling or automatic upgrade.

`--release-cid <CID>` aliases `--release <CID>` for deliberate immutable selection,
including historical releases. Explicit selection bypasses discovery without
opening or lowering the channel watermark. Recovery and baseline repair continue
to require explicit immutable selection. Changing the configured IPNS identity
selects a different trust identity with its own watermark; do not use identity
changes or watermark deletion to recover an old release. Use explicit CID selection.

Register a service separately, then use the native service manager to start it.
Installation and registration do not start the Host or enable login activation.
For Linux:

```sh
ww daemon install
systemctl --user start ww.service
ww healthcheck --ready
# Optional login activation:
systemctl --user enable ww.service
```

Generated Linux units use the user manager's resolved home as their working
directory. Custom settings remain unchanged. Automatic native replacement requires
supported GNU ARM64 systemd configuration and a fixed loopback `/readyz` endpoint.
No stopped startup admission or private readiness protocol is required.

On macOS, use the logged-in GUI domain:

```sh
ww daemon install
launchctl enable "gui/$(id -u)/io.wetware.ww"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/io.wetware.ww.plist"
ww healthcheck --ready
```

Registration requires that GUI domain; there is no implicit headless fallback.
macOS automatic native replacement remains unsupported. Service-manager
acceptance alone does not prove Host readiness.

## Existing files and recovery

`install` refuses a different existing native artifact and directs the operator
to `upgrade`. It refuses a different FHS package and directs the operator to
`image update`. Re-running a newer shell installer cannot bypass these checks.
Same-artifact repair can restore missing owned software; unknown collisions,
symlinks, and altered owned files cause refusal. Unknown FHS files remain intact.
Select the recorded package with `ww install --release <RECORDED_ROOT>` or rerun
its signed shell bootstrap. Exact-byte reinstall preserves accepted native
provenance. Existing installations without native proof need explicit baseline
repair before a changed upgrade.

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
Startup holds a shared `~/.ww-fhs.lock` guard through local baseline capture.
FHS writers acquire `MutationLock` before the exclusive FHS guard. A running Host
keeps its previously captured baseline without holding either lock.

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
sh scripts/install.sh --recover --timeout-secs N
sh scripts/install.sh --recover --release <RECORDED_RELEASE_CID> --timeout-secs N
```

Recovery requires an existing `host-upgrade.json`. The default locator is the
untrusted `installed-release.json`; supply the exact recorded release explicitly
when that locator is unavailable. The shell binds the authenticated manifest to
the requested root and verifies the Host bytes before invoking the native
recovery-only entry point. The native entry point
holds the installation lock, reauthenticates the typed release, verifies its own
executable bytes, and matches the recorded generation before calling `UpgradeCoordinator::recover`.
It never falls through to fresh installation or new-release selection.

Native recovery supports recorded managed-service transactions. The native
recovery command does not accept a standalone `run` invocation:

```sh
/path/to/authenticated/ww upgrade --recover-only --release <RECOVERY_EXECUTABLE_CID> \
  --timeout-secs N
```

The CID authenticates the executable running recovery; the journal selects the
restored generation. Use the installation's HOME and independent trust keys.
See the [explicit native recovery form](native-upgrades.md#recovery-and-explicit-repair).

Shell staging and native HTTP reauthentication use the separate endpoint scopes
described above. The recorded release must contain a Host
with this recovery entry point. Missing, incompatible, or unrelated artifacts
cause refusal. Recovery preserves the native operational gates below.

## Native replacement and recovery gates

Ordinary startup does not read native-upgrade journals or require admission.
A damaged journal blocks native replacement and remains available for explicit
repair. FHS capture takes a short shared guard; FHS writers take `MutationLock`
then the exclusive FHS guard. The capture worker releases its guard before
network publication and Host execution.

Changed native upgrade, rollback, recovery, and repair require a positive
`--timeout-secs N`. `UpgradeCoordinator` retains independent signed release proofs,
exact staged and rollback bytes, durable before/after records, and journal-last
cleanup. Automatic replacement supports managed GNU ARM64 Linux/systemd only.
A running service requires a fixed loopback `/readyz` endpoint. Unsupported
configuration refuses before mutation; custom configuration remains unchanged.

`ww upgrade --repair-baseline --release INSTALLED_CID --timeout-secs N` authenticates
current bytes and repairs damaged metadata without replacing the executable or
restarting the service. Missing historical rollback requires explicit
`--acknowledge-missing-rollback`. Repair preserves damaged evidence and never
invents signed provenance. Known valid transactions use ordinary recovery.

Automatic channel discovery still requires independently trusted channel
freshness. Explicit historic selection does not lower a freshness floor. See
[native replacement and recovery](native-upgrades.md) for trust, durability,
service observations, repair, and qualification limits. No operation authorizes
production activation or publication.

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
