# Local mount snapshots

Landing B of #734 adds a library capture/publication path. Public `ww run`,
mount parsing, deployment epochs, and `ww push` still use their existing paths.
Landing C integrates the library with startup and deployment ownership.

## Capture

`CaptureBatch::capture` takes explicit `CaptureRequest` values and an admitted
`cache::ForegroundTransaction`. All local sources enter one batch before any
source is published. No identity, key, namespace, or adjacent `~/.ww` discovery
occurs. Explicitly selecting private content still imports that content.
Relative paths use the worker's captured CWD descriptor. Landing C must construct
absolute requests from invocation-time CWD before starting capture.

One blocking worker uses descriptor-relative `rustix` opens. Every source
component and included descendant rejects symlinks. Directory opens require
`O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`; file opens use
`O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC` and verify regular-file type before reading.
Special nodes fail. macOS `/tmp` and `/var` aliases fail as symlink components;
callers must explicitly select the real path. Capture never canonicalizes a
source to silently bypass this policy.

Names remain exact UTF-8. Empty, dot, parent, separator, and ASCII-control names
fail. Empty directories remain entries. Hardlinks are separate selected files.
Local modes, timestamps, ownership, ACLs, and xattrs are omitted from publication.

Root sources exclude exactly `target`, `.git`, `.gitignore`, and `Cargo.lock`
at every depth. Capture charges excluded names but does not open excluded entries.
Targeted directories include these names. Capture does not interpret `.gitignore`.
Targeted regular files have no exclusions.

Capture compares file identity, type, size, mtime, and ctime before and after
reading. Before sealing the batch, capture rechecks directory child sets, entry
identities, and metadata. Capture failures abort without rereading the source.
Sources must be quiescent. These checks detect common concurrent mutations;
they do not provide an atomic filesystem snapshot against arbitrary writers.
No-follow traversal does not establish historical containment after directory
renames and does not exclude hardlink aliases.

`CaptureCancellation` requests cancellation between entries and 64 KiB reads.
An awaited cancellation completes after the worker releases descriptors.
Dropping the capture future also requests cancellation; filesystem scope
completion remains the cleanup barrier. A hung filesystem syscall has no hard
cancellation deadline. The worker admission remains held until actual completion.

## Publication and verification

`CaptureBatch::publish` consumes frozen bytes. Each source uses one multipart
root named `source`; directory parts use `application/x-directory`, including
empty directories. Retries reuse the same multipart bytes and share cumulative
allowances. Publication passes no host path to Kubo.

The Kubo 0.33.0 profile explicitly sets:

```text
pin=true wrap-with-directory=false cid-version=1 hash=sha2-256
raw-leaves=true chunker=size-262144 trickle=false inline=false
nocopy=false fscache=false preserve-mode=false preserve-mtime=false
progress=false silent=false only-hash=false
```

No `to-files`, mode, or timestamp option is sent. Kubo performs UnixFS chunking.
The client validates the complete bounded `/add` response, exact manifest paths,
strict CIDs, decimal sizes, allowed field types, and final root event. Duplicate,
unknown, missing, progress, malformed, truncated, or late error records fail.
HTTP 200 or an early root event does not establish publication success.

A header-only `import_composed(root, [])` must acknowledge recursive retention.
The existing Composer traversal then validates the source DAG. Capture-specific
inspection also fetches raw directory children; ordinary Composer keeps its
opaque raw-directory-child contract. Verification checks every block CID,
manifest entry name/type, file length and SHA-256 digest, cumulative `Tsize`,
event CID/size, and metadata omission. Verification requests make one attempt;
failure aborts the batch. A source becomes available as `FrozenSource` only
after acknowledgement and verification succeed for the complete batch.

## Bounds

Limits are inclusive and apply across the complete batch. Releasing buffers or
retrying transport does not reset counters. These limits bound data and work;
they are not an exact process RSS limit.

| Quantity | Limit |
| --- | ---: |
| Local source operations | 128 |
| Entry depth | 128 |
| Included nodes, including roots | 100,000 |
| Enumeration/recheck entry visits, including exclusions | 1,000,000 |
| Captured file bytes | 64 MiB |
| Stored relative-path bytes | 8 MiB |
| One relative path | 4,096 UTF-8 bytes |
| Read chunk | 64 KiB |
| Capture workers / descriptors | 1 / at most 256 |
| Multipart bytes, including framing and attempts | 80 MiB |
| `/add` response per attempt / aggregate | 16 MiB / 16 MiB |
| One `/add` JSON line | 16 KiB |
| Verification block / read bytes | 1 MiB / 64 MiB |
| Verification nodes / work visits | 100,000 / 1,000,000 |
| CAR acknowledgement response | Existing 64 KiB limit |

Landing C must count immutable/named sources and the authoritative head against
the same 128-operation envelope. Composer additionally checks target depth plus
source DAG height. A captured payload can fit 64 MiB while its imported DAG
exceeds verification or composition bounds; that batch fails before activation.
The stored-path allowance includes manifest, digest, and mutation-record keys.

## Retention ownership

`PublishedSources` carries verified identities, not independent pin claims.
Landing C must transfer all roots to deployment's frozen-source ownership before
preparation. Roots hidden by later operations still require retention.

Dropping success or failure values never unpins. `SnapshotPublicationFailure`
reports acknowledged roots and an uncertain source index when upload outcome
is unknown. Interrupted publication can leave blocks or pins. An aborted future
cannot return that report; callers must treat its publication outcome as unknown.
Repeated Kubo pins are CID-centric and cannot justify compensating unpins.

This path adds no general retention registry, durable snapshot catalog, or
restart recovery protocol. Source identity and retention ownership remain
separate. Equal frozen inputs and Composer profile determine composition
identity; equal local bytes across Kubo importer versions are not that contract.
