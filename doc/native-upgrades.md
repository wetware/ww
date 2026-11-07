# Native Host replacement engine

`ww upgrade` connects authenticated release selection to the #739 transaction
engine. `PreverifiedCandidate::from_release` accepts only an authenticated
release and selects the current platform. Native replacement remains gated by
`NativeService` compatibility and restart admission. The installer is separate
and refuses different existing native bytes.

## Admission boundaries

The release selector must authenticate both the installed artifact and the
selected candidate. Version text and source revision are provenance, not exact
artifact identity. `ArtifactIdentity` names an explicit digest algorithm, exact
digest, and executable size. Staging recomputes the selected digest and size
from the supplied bytes. Supported digest algorithms are SHA-256 and BLAKE3;
the engine never infers an algorithm from digest length.

Candidate bytes arrive separately from authenticated selection. The CLI checks
manifest signature, descriptor binding, platform compatibility profile,
transport-root link, and exact reconstructed digest/size. Artifact download is
bounded and uses signed artifact CIDs. Independent release keys remain the trust
root. Explicit `--release CID` permits historical selection without lowering
freshness state; automatic channel selection awaits trusted channel provisioning.

A new `install --release` records an untrusted CID locator in
`installed-release.json`. Upgrade reauthenticates that release and verifies the
installed executable; the locator itself supplies no authentication. Legacy or
local-payload installs without provenance refuse native replacement.

`installation.json` accepts legacy version 1 and scoped version 2. BLAKE3
ownership hashes do not authenticate a release. The native engine changes only
the executable slot and preserves FHS and legacy namespace/service provenance.
Incomplete and unknown receipts cause refusal.

## Transaction ownership

`Engine::open` holds `~/.ww-installation.lock` until the engine is dropped.
Installation, existing update/removal callers, and daemon definition writers use
the same lock. Existing mutators refuse when `host-upgrade.json` exists, even
after the process that held the lock exits. Service-manager child commands retain
the lock if their CLI parent exits. Do not unlink the lock to bypass contention.

The engine changes only these installation paths:

| Path under `~/.ww` | Purpose |
|---|---|
| `bin/ww` | Installed native executable |
| `bin/.ww-host-upgrade/` | Verified staging, replacement, and retained executable bytes |
| `host-upgrade.json` | One pending transaction, bounded to 64 KiB |
| `host-upgrade-retained.json` | Current and one previous artifact's recorded provenance |
| `installation.json` | Existing receipt; only executable ownership changes |

Metadata writes sync the temporary file, rename it, and sync the containing
directory. Executable staging occurs on the destination filesystem. The installed
executable changes only after the complete staged bytes match the selected
identity and operational compatibility has passed. Candidate download or staging
failure leaves the installed executable unchanged.

The transaction records intent before replacement and restart. Previous bytes
remain available through replacement, restart, readiness checks, and receipt
commit. After success, the engine retains one previous artifact. Cleanup does not
prune IPFS content or alter deployment authority.

| Durable phase | Recovery policy |
|---|---|
| `Staging`, `Ready` | Abort staging; verify and preserve the original installed executable |
| `Replacing`, `Installed`, `Restarting`, `Verified` | Restore recorded previous bytes and receipt after service/compatibility checks |
| `Restoring` | Continue recorded recovery; retain pending state if recovery cannot complete |
| `Restored` | Verify the restored executable, original receipt, and original retention metadata; finish cleanup without restarting again |
| `Committed` | Verify the committed executable/receipt and finish cleanup |

`operator_upgrade` resolves a pending transaction before calling its new-release
selection callback. The recovery result identifies the exact artifact and whether
recovery aborted staging, restored the previous artifact, or completed a commit.
The same invocation does not then select a different release. `operator_rollback` uses only the
previous recorded artifact and retained bytes. Rollback uses the same replacement
owner and does not lower release freshness or rewind deployment heads.

The authenticated shell recovery path uses `Engine::require_pending_recovery`
before native network selection, then `Engine::recover_authenticated`. The latter
requires an independently authenticated selection that matches the full recorded
previous or candidate generation. The CLI also verifies its own executable bytes.
This recovery-only path cannot start a new transaction. See the shell invocation
and endpoint requirements in [installation and recovery](installation.md).

`upgrade --json` reports a structured outcome, selected and observed artifact
identities, native artifact paths changed, activation, and recovery action.
Observed identity describes bytes on disk; it does not certify running readiness.
Changed paths exclude journal and retained-copy metadata. Unknown failure state
uses null values for activation and changed paths.

Records currently depend on the operator's private local filesystem custody.
Production integration must define retained signed-evidence validation with
#738; private journal metadata must not become a substitute signature verifier.

## Service and deployment checks

Service observations distinguish registration, startup enablement, running PID,
failed state, executable path, and effective configuration. Unknown or
contradictory state refuses replacement. Custom service definitions and drop-ins
are observed without rewriting them.

Only a previously running managed service can be restarted. A stopped service,
including a failed service with no running process, remains stopped. An absent
service can yield executable installation, never a claim of live readiness.

Live replacement requires evidence that captured startup configuration, prepared
FHS/local sources, and publisher target match the next startup. The restarted
Host must enforce that admission before publishing or activating guests. Admission
must cover every launch after replacement, including automatic service-manager
restarts; checking files only before an explicit restart is insufficient. Serving readiness must accompany
exact native artifact identity and the service manager's matching new process.
Git SHA, version text, process existence, and `/healthz` alone do not qualify.

Current Hosts do not provide the required captured startup/native identity
evidence. `NativeService` therefore observes systemd/launchd state but refuses
live admission, restart, and compatibility certification. A production adapter
must check the staged candidate against the effective prepared deployment before
enabling even a stopped-service command. Missing live evidence requires explicit
service shutdown; it does not authorize guessing or activating pending edits.

Readiness uses a finite caller-supplied deadline. There is no production default:
existing Kubo wait/retry, source preparation, and kernel lifecycle bounds can
exceed the provisional 120 seconds. The service backend must honor its deadline;
the byte supplier must separately bound download I/O. Deadline checks guard
executable replacement and receipt commit. An admitted commit finishes durability
and cleanup even if those filesystem operations outlast the deadline. Recovery failure retains
the journal and previous bytes and reports the residual state. An incompatible
previous Host must remain stopped rather than changing the authoritative image.

## Qualification and production gates

Unit tests use real temporary filesystem operations and deterministic service
evidence. They qualify transaction policy separately from native service-manager
restart behavior. Native observation tests do not establish successful live
restart, cold-start timing, power-loss durability, or production trust provisioning.

Before permitting native replacement through the CLI, integrate and qualify:

1. Trusted automatic channel selection, retained signed evidence, and atomic
   installed-release provenance changes during native commit/recovery.
2. Candidate/effective-deployment compatibility using the prepared FHS machinery.
3. Captured startup evidence and admission enforced before restarted Host effects.
4. Exact running native identity and serving readiness on systemd and launchd.
5. Recovery from real service restart failure and measured bounded cold startup.

The CLI separates `install`, `image update`, `upgrade`, `uninstall`, and daemon
registration/removal. The native engine never updates FHS or service defaults,
publishes IPNS values, or authorizes a production rollout.
