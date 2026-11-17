# Authenticated native replacement and recovery

`ww upgrade` delegates native replacement to `UpgradeCoordinator`, the single
installation mutation owner. The local administrator and operating-system
service manager are trusted. The updater checks untrusted release artifacts,
configuration mistakes, cooperating-operation races, failed restarts, and
interrupted writes. It makes no protection claim against a malicious local
administrator.

The transaction follows this order:

```text
authenticate → stage → stop and confirm quiescence → replace
             → restart → check /readyz → commit or recover
```

Native replacement changes the Host executable and its authenticated provenance.
It preserves the FHS, identity, namespaces, service definition, enablement, and
publication authority. Initial automatic replacement supports managed GNU ARM64
Linux/systemd only. Unsupported platforms and unmanaged replacement refuse.
Current native qualification results are recorded separately; enabling this
adapter in source does not authorize production activation or publication.

## Ordinary startup and FHS capture

`ww run` requires no transaction-bound admission, launch permit, or instance
lease. Native-upgrade metadata does not authorize ordinary startup. A damaged or
unknown native journal blocks further native upgrades while remaining available
for explicit operator recovery. Ordinary startup still validates its FHS ownership
and refuses incomplete FHS updates.

`~/.ww-installation.lock` serializes installation mutation. FHS writers acquire
that `MutationLock` first, then an exclusive `~/.ww-fhs.lock` guard. Startup takes
only a shared FHS guard when any local source overlaps the managed FHS. The local
capture worker owns the guard until its source descriptors close, including
when its asynchronous caller is cancelled. The guard is released before network
publication and is never held throughout Host execution.

The FHS lock contains no lease state or authoritative journal. Native replacement
holds `MutationLock` without taking the FHS writer guard. Its restarted Host can
therefore capture the unchanged FHS. Do not unlink lock files to bypass contention.

## Independent release authentication

Select a release with `--release CID` and independently provisioned public keys:

```sh
ww upgrade --release CID --trusted-release-keys /etc/wetware/release-keys --timeout-secs 300
ww upgrade --release CID --check
ww upgrade --rollback --timeout-secs 300
```

The timeout is an explicit example budget. Changed upgrades, rollback, recovery,
and baseline repair require a positive `--timeout-secs`. Exact-artifact no-ops and
checks do not require a restart budget. Checks can download and stage
candidate bytes in temporary storage but do not replace installed software.
Without `--release`, upgrade resolves the independently configured `WW_RELEASE_IPNS`
identity (or `--release-ipns NAME`). Channel checks persist authenticated freshness
floors in `~/.ww/release-consumer`; explicit `--release` / `--release-cid` selection
does not change those floors. See [release discovery configuration](installation.md#separate-software-and-service-operations).

Signed release proofs bind the manifest, descriptor, transport root, target,
exact executable digest, and size. Downloaded keys cannot establish trust.
Versions, source revisions, paths, ownership hashes, and manager PIDs do not
substitute for authenticated release identity. Retained public-key evidence and
signed proofs authenticate the exact current and previous generations during
offline rollback and recovery. New candidates use independently supplied trust.

Automatic channel discovery authenticates the selected release and persists
consumer freshness floors. Explicit historic release selection does not lower a
consumer freshness floor. Same-version releases with different authenticated
executable bytes are changed upgrades. Identical executable bytes are a no-op
and preserve
accepted provenance, even when another signed envelope contains those bytes.

## Durable records

| Path under `~/.ww` | Responsibility |
|---|---|
| `bin/ww` | Installed native executable |
| `bin/.ww-host-upgrade/` | Verified staged/retained executables, signed proofs, repair archives |
| `host-upgrade.json` | The single version-3 native transaction or baseline-repair journal |
| `host-upgrade-retained.json` | Current and one previous authenticated generation |
| `installation.json` | Local software ownership receipt; not release authority |
| `installed-release.json` | Compact untrusted release locator |
| `uninstalled-native-retention-<digest>.json` | Preserved inactive native metadata after uninstall |

Active retention selects current and one previous generation. Physical historical
executables and proofs remain preserved for operator recovery; disk use can grow
across upgrades. No automatic garbage collector removes those files.

Records are bounded to 64 KiB. Signed proof envelopes have a separate 8 MiB
bound and retain each release-verifier bound. Writes sync temporary file bytes,
rename the file, then sync the parent directory. Cross-directory moves sync both
parents. Recovery validates the recorded before/after tuple; journal removal
occurs last. The metadata files do not change simultaneously.

Fresh signed installation completes its ownership receipt and software before
retaining native proof through a metadata transaction. Existing installations
without native provenance require explicit authenticated baseline repair before
a changed upgrade. Interrupted software installation still uses its incomplete
ownership receipt and exact-package retry. An interruption before native
initialization can require baseline repair after software installation completes.

Exact-byte reinstall preserves accepted native provenance. Uninstall durably
archives active native retention before detaching that record. It preserves
retained executable files, signed proofs, private state, and recovery evidence.
A later fresh installation can establish its own authenticated generation.

## Managed stop, restart, and readiness

The GNU ARM64 adapter reads effective systemd configuration, including the loaded
unit, drop-ins, arguments, working directory, and environment. It supports a
constrained direct `simple`/`exec` service for the installed executable. Unsupported
indirection, activation, or namespace settings refuse before replacement.
Configuration must remain unchanged during the transaction.

A running service requires a fixed loopback admin address. The adapter uses the
existing `/readyz` endpoint. `--with-http-admin off`, a non-loopback address, or an
indeterminate endpoint refuses automatic replacement of a running service before
transaction mutation. Ordinary Host startup may still disable the admin listener.

The adapter requests bounded manager stop, confirms the observed old process
exited, and confirms the service is stopped before replacement. Restart uses the
existing service definition. Process observation compares the manager's process
and `/proc/PID/exe` file identity with the installed executable. It does not hash
the running executable repeatedly or exchange a parent/child authentication
protocol. `/readyz` must return HTTP 200 while manager observations remain stable.
The coordinator checks the ready process again around metadata commit.

An originally stopped service remains stopped and reports `live_ready: false`.
A successful file restoration does not establish service readiness. A running
service's readiness failure triggers one bounded attempt to restore the
authenticated previous generation. The upgrade still returns its original error.
If rollback fails, the coordinator attempts to stop the service and preserves the
journal for explicit recovery.

## Recovery and explicit repair

A normal invocation with a pending recognized journal recovers that recorded
transaction before considering new selection. Recovery aborts untouched staging,
restores authenticated retained bytes, or finishes terminal metadata cleanup as
appropriate. A terminal cleanup does not initiate another restart or claim new
readiness. Corrupt or missing rollback bytes/proofs cause refusal and preserve
evidence.

For a damaged installed executable, the authenticated shell recovery entrypoint
can stage and run a selected recovery Host:

```sh
sh scripts/install.sh --recover --release RECOVERY_EXECUTABLE_CID --timeout-secs 300
```

The selected release must authenticate the helper's exact executable bytes and
match a generation recorded by the recognized transaction. The helper does not
fabricate provenance or turn recovery into fresh installation. Historical V1
recovery remains restricted to its recorded exact generations. Descriptor CIDs
are not treated as transport roots. Unknown versions, including unpublished V2
protocol records, require explicit repair.

When native metadata is damaged but installed executable bytes are valid, use:

```sh
ww upgrade --repair-baseline --release INSTALLED_CID --timeout-secs 300
# Add only when historical rollback cannot be authenticated:
ww upgrade --repair-baseline --release INSTALLED_CID --timeout-secs 300 --acknowledge-missing-rollback
```

Repair independently authenticates the selected installed bytes and validates the
complete FHS ownership receipt. It archives damaged records before superseding
them and retains existing artifacts. A valid known transaction must use its
normal recovery path; baseline repair cannot discard its obligations. Missing
historical rollback requires explicit acknowledgement and is reported as
unavailable. Repair leaves executable bytes and service state unchanged and
reports no new readiness. Later upgrades retain the newly authenticated current
executable as their rollback source.

If executable bytes or FHS ownership cannot be verified, restore verified bytes
or ownership evidence explicitly before repair. Preserving a damaged journal
never makes its unverified bytes an authenticated release. Manual startup of an
otherwise valid Host remains independent of native journal integrity.

## Operational responsibility

Wetware authenticates replacements, preserves rollback bytes and durable journal
state, and verifies executable identity and readiness. It does not change
operator restart settings or reload the systemd manager. Recovery resets only
`ww.service` failure counters after restoring authenticated executable bytes.
Terminal recovery verifies the recorded running or stopped disposition before
removing the journal.

Systemd may restart a failing candidate after updater death. The operator owns
availability monitoring, crash-loop diagnosis, and explicit recovery. No
autonomous recovery or availability guarantee follows from the journal. Explicit
recovery requests manager stop, confirms quiescence, restores the authenticated
previous executable, and checks readiness when the service was originally running.
Operator-configured service dependencies retain their normal systemd semantics;
Wetware does not rewrite dependencies or manage unrelated services.
