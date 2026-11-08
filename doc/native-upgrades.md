# Native Host startup, replacement, and recovery

`ww upgrade` selects an authenticated native Host and delegates replacement to
the existing `Engine`. The installer, FHS updater, and daemon registration retain
their separate responsibilities. Native replacement does not change the FHS,
identity, namespaces, service arguments, or publication authority.

The production `NativeService` adapter remains disabled until native acceptance
passes. Current component tests do not authorize an installed-CLI upgrade.
macOS also lacks qualified exact running-executable evidence.

## Startup admission

A fresh signed installation creates a durable installation identity, retains the
public signed proof, and enters `RequiredUnbound`. Installation does not authorize
`ww run` or start a service. Admission binds exact normalized startup inputs while
the installation is stopped:

```sh
ww startup admit --timeout-secs 300 service
ww startup admit --timeout-secs 300 run --executor-threads 2
```

These commands require a qualified native adapter. The number above is an
explicit example budget, not a measured or automatic timeout recommendation.
`--check` prepares the inputs and reports the proposed admission without changing
the contract. Preparation can fetch and retain public content in Kubo.

Managed admission reads the actual loaded service definition, drop-ins, argument
vector, working directory, and effective environment. Custom definitions remain
unchanged. Guarded managed invocations currently require explicit positive
executor and compiler worker counts (`--executor-threads` or
`WW_EXECUTOR_THREADS`, and `WW_COMPILE_WORKERS`). The adapter does not infer
service CPU defaults from the admission command's process.

Standalone admission binds the supplied `run` arguments and environment. A
standalone contract cannot authorize a process in the owned `ww.service` cgroup.
An admitted manual process holds a shared instance lease until exit. The operator
must stop that process before replacement, re-admission, or removal.

At startup, the Host captures identity, namespace files, local source bytes,
runtime settings, and publication settings before Host effects. The Host compares
the captured identity with the admitted contract, then consumes that same capture.
Changes to an admitted input require explicit stopped re-admission. A baseline
FHS update does not implicitly grant a new startup contract.

## Independent release trust

The startup state and contract retain the absolute, custody-checked path supplied
through `--trusted-release-keys`. The file contains independently provisioned
public keys. The Host never provisions publisher credentials or signing keys.

Pending launches and new candidate acceptance reread that path. Replacing a path
component with a symlink, changing file custody, or removing the required current
key causes refusal. Rotating public keys at the bound path does not require a new
path binding. Explicit stopped `startup admit --trusted-release-keys FILE` changes
the binding after authenticating the installed generation with the supplied keys.

Exact committed current and previous generations can authenticate with their
retained historical public-key evidence. This exception permits historical
rollback; it does not admit a different candidate signed by a removed key. No
operation lowers the release consumer's freshness floor.

## Durable installation tuple

The accepted native generation binds all of the following:

- Exact executable digest algorithm, digest, and size.
- Native ownership slot in `installation.json`.
- Compact v1 transport-root locator in `installed-release.json`.
- Signed manifest, descriptor, binding, and root-block evidence.
- Startup contract and durable startup disposition.

The compact locator remains untrusted. A source revision, version string,
pathname, ownership digest, or service-manager PID cannot substitute for signed
release evidence or exact executable identity.

| Path under `~/.ww` | Purpose |
| --- | --- |
| `bin/ww` | Installed native executable |
| `bin/.ww-host-upgrade/` | Staged and retained executables, startup state, contract, and public proofs |
| `host-upgrade.json` | One bounded V2 transaction journal, including admission-only operations |
| `host-upgrade-retained.json` | Current and one previous authenticated native generation |
| `installation.json` | Software ownership receipt |
| `installed-release.json` | Compact untrusted release locator |

Metadata records are bounded to 64 KiB. Retained signed proof envelopes have a
separate 8 MiB bound and retain each underlying release-verifier bound.
Writes sync a temporary file, rename the file, and sync its parent directory.
The journal is removed last. This protocol provides logical atomic acceptance;
the files do not change simultaneously.

Initialization records intent before incomplete receipt, marker, identity, and
software writes. The complete receipt is published last. Retrying initialization
preserves the installation identity and private identity bytes. Exact-artifact
reinstallation preserves an admitted contract and accepted provenance, including
when another signed release contains identical executable bytes.

## Leases and pending launches

`~/.ww-installation.lock` excludes installation writers. Stable external instance
and startup locks coordinate replacement with running Hosts. Writers acquire
mutation, instance, and startup leases in that order. A writer publishes Blocked
before stopping an owned service, releases the startup lease, and then obtains
instance exclusion. Do not remove lock files to bypass contention.

Only an originally running managed service receives a pending transaction launch
permit. The permit binds the transaction, contract, exact executable, manager
identity, and recorded service configuration. A busy mutation lock is not launch
authority. A stopped or absent service remains stopped throughout replacement.

The child validates its permit before capture and again before effects. The
pending journal supplies the effective candidate or restored contract while
steady metadata still contains the other generation. A terminal journal admits
only its exact recorded managed generation. Terminal cleanup does not initiate
another restart.

## Process identity and readiness

Linux observation pins the process with a pidfd, identifies its boot/start token,
opens `/proc/PID/exe`, and hashes the complete running executable. The verifier
checks executable and process identity before and after private IPC. The service
manager must report the same process and invocation.

The private readiness protocol binds a fresh challenge, launch nonce, startup
contract, executable, and coherent serving epoch. The Host holds a CLOEXEC socket
open until acknowledgement. The verifier rejects EOF or extra data before
acknowledging. A stale buffered reply after `exec` cannot satisfy that exchange.
Readiness works with the HTTP admin listener disabled.

macOS `SecCode` and mapped-image observations did not establish the required main
executable identity in the adversarial probe. A matching code-directory hash and
an expected secondary mapping were insufficient to prove complete main-image
bytes. macOS native admission remains unavailable.

## Replacement and recovery

```sh
ww upgrade --release CID --timeout-secs 300
ww upgrade --rollback --timeout-secs 300
ww upgrade --release CID --check
```

Changed upgrades, rollback, and recovery require a positive explicit timeout.
Exact-artifact no-ops and read-only checks do not need a restart budget.
The journal records a separate containment budget of at most 30 seconds. After
operation failure, one cleanup deadline bounds the owned-service stop and
reobservation. Unconfirmed containment retains blocked recovery evidence. This
budget is not a restart default. Filesystem syscalls have no hard real-time bound.
Rollback preflight (`--rollback --check`) remains unavailable. Automatic channel
selection still requires independent trusted-channel provisioning; explicit CID
selection does not activate that separate feature.

The engine authenticates both generations, captures effective inputs, retains
rollback material, and preflights both exact executables with the production
composer and linker. Preflight does not activate guests. The engine records stop
intent, confirms quiescence, replaces the executable, and permits only the
recorded child. Live commit requires exact new-process evidence and serving
readiness. Stopped replacement proves that no process started.

Recovery runs before selecting another release. Early recovery preserves the
previous tuple. Recovery after replacement blocks starts, contains the owned
service, verifies previous compatibility, and restores the complete previous
tuple. Missing or conflicting proof retains the blocked journal and recovery
material. Terminal recovery verifies the durable accepted tuple and finishes
cleanup without another restart.

Legacy V1 journals permit early abort. Post-replacement V1 recovery requires an
independently authenticated exact recorded generation; the engine does not infer
a transport root from a descriptor CID or grant guarded startup authority.
Existing unguarded installations remain supported for their existing operations;
changed native upgrade requires explicit migration work and cannot silently
convert ownership metadata into release authority.

Uninstall blocks new starts before stopping an owned service. After instance
exclusion, uninstall records `Uninstalled` and removes owned software. The
installation identity, startup marker, signed proofs, private data, and removal
tombstone remain available for validated retry.

## Qualification limits

Filesystem crash tests establish transaction behavior, not native manager
acceptance. Local source builds establish component behavior, not genuine
release-producer evidence. The acceptance runner invokes actual installed CLI
and manager commands in a separately provisioned disposable account:

```sh
python3 scripts/qualify_native_upgrade.py --platform PLATFORM --home ABS_HOME \
  --service-manager systemd --release-a CID --release-b CID \
  --trusted-release-keys ABS_PUBLIC_KEY_FILE --ipfs-url URL --timeout-secs N \
  --output ABS_EVIDENCE_DIRECTORY
```

The runner requires original producer receipts and a root-provisioned disposable
account marker. A partial matrix returns a nonzero exit. Each supported platform
needs its own changed-upgrade, rollback, failure, crash, preservation, and
login/boot evidence. Neither these tests nor a PR authorizes production activation.
