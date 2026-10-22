# Wetware Composer v1

For the mount-specific local capture and publication library, see
[Local mount snapshots](local-snapshots.md). CLI and deployment replay use these
same frozen operations across authoritative epochs.

`cell::image::COMPOSER_PROFILE` identifies `wetware-composer-v1`. For accepted
inputs, the same ordered layer CIDs and this profile produce the same root CID.
Historical Kubo MFS root identity is not part of the contract.

## Typed target operations

`image::inspect_source` validates a retained immutable CID and returns a
`FrozenSource`. Callers cannot supply its type or cumulative size. A source is
an ordinary directory or a file (raw codec, UnixFS Raw, or UnixFS File).
Standalone symlinks fail. Directory sources can contain opaque symlink entries.
`FrozenSource` records content identity; it does not own or acquire a pin.
The caller must retain sources before inspection and through composition.

`FrozenMount::new` pairs a source with a validated `GuestTarget`. Targets are
absolute UTF-8 paths. `/` denotes the root. Empty components, trailing slashes,
`.` and `..`, and ASCII controls including DEL fail. Targets preserve spaces,
backslashes, punctuation, and Unicode bytes exactly. Targets are bounded at
4 KiB and 128 components. A root target requires a directory.

`image::compose_mounts` accepts an optional initial directory root and an ordered
slice of frozen operations. All sources are validated, including overwritten
sources. The initial root counts toward the 128-operation bound. With no initial
root, the first root operation seeds the result directly, preserving its CID.
If the first operation is targeted, Composer starts with the canonical empty
UnixFS directory. No initial root and no operations is an error.

At an exact target, directories merge recursively using the existing overlay
rules. Other collisions select the source link, including its full CID and
verified cumulative size. No source basename is appended. A file or symlink in
a strict ancestor causes an error; Composer never follows symlinks. Existing
strict ancestors retain their exact metadata bytes and unrelated entries.
Missing ancestors use only UnixFS metadata `[0x08, 0x01]`.

Composer edits each target path directly and rebuilds changed ancestors from
the target to the root. It applies the existing base-preferred identity rule at
directory collisions. Target depth plus full source height, including file
chunk links, must fit the existing depth bound. Path traversal, directory
copying, and encoding consume the same work, node, and byte budgets.

`dag_merge_cids` remains the root-only adapter. Public CLI mounts support targeted
files and directories. Deployment uses the installed baseline as the lowest
layer, then the authoritative head, frozen namespace images, and frozen CLI
operations. Baseline and head each count toward the operation bound. Kernel
selection reads `/boot/kernel.wasm` from the composed effective root; Composer
has no kernel-specific precedence rule.

Deterministic input and composition failures carry `CompositionError`, available
through `anyhow::Error::downcast_ref`. Transport, cancellation, and publication
errors retain their existing error chains.

## Data flow

`image::dag_merge` parses layer CIDs, invokes `image::composer`, then retains the
result. The composer reads blocks through a `BlockSource`. `image::codec` decodes
typed directories, validates UnixFS metadata, and encodes changed directories.
Directory entries use exact UTF-8 strings as `BTreeMap` keys.

Kubo remains the block transport, exchange, storage, pinning, and IPNS backend.
Composition makes no `/ls` or `/files/*` requests. `CidTree` still uses Kubo
directory listings for guest access; its representation and publication lifecycle
are unchanged.

## Overlay rules and sharing

Layers apply left-to-right. A later entry replaces an earlier file or symlink.
Directory/directory collisions merge child entries recursively and take the
overlay directory's metadata wholesale. There is no per-field inheritance:
if the overlay omits `mode` or `mtime`, the result omits that field too.
This applies at the root and in nested directories, even when no child changes.
File/directory conflicts use the later entry. There are no whiteouts or deletion
markers.

Unchanged directories retain their input CID, including single-layer roots.
Wholesale replacements retain the overlay CID. Reuse selection occurs after
semantic composition. If the result matches the base entries and metadata,
Composer v1 reuses the base CID. Otherwise, a result matching the overlay entries
and metadata reuses the overlay CID. When both qualify, the base CID takes
precedence, including CIDv0/CIDv1 and Data-first/Links-first representations.
This identity-selection rule does not change later-layer metadata precedence.
Only changed
directories and their ancestors require encoding. Intermediate results from
earlier layers are excluded from the stored blocks when the final tree does not
reference them. File blocks are never rewritten.

## Encoding and metadata profile

| Property | Composer v1 rule |
| --- | --- |
| Codec | Standard DAG-PB, using `ipld-dagpb` 0.2.2. |
| New directory encoding | Contiguous `Links` fields precede `Data`; each link encodes `Hash`, `Name`, then `Tsize`. |
| Link ordering | Ascending UTF-8 byte order, independent of insertion order. Duplicate directory names fail. |
| New directory CIDs | CIDv1, DAG-PB (`0x70`), sha2-256 (`0x12`, 32 bytes). |
| Accepted inspected CIDs | CIDv0 or CIDv1, sha2-256. Other hashes and codecs fail. |
| Directory metadata | Only `Type=Directory`, optional `mode` (12 permission bits), and optional `mtime`. |
| Recursive metadata collision | Use the overlay directory's entire metadata byte string. Omitted overlay fields remain omitted; no fields inherit from the base. |
| Replacement metadata | Reuse the overlay node and its metadata unchanged. |
| Time | No clock-derived metadata. `mtime` requires seconds. Zero fractional nanoseconds must be omitted. If present, the fraction must be `1..=999_999_999`; explicit zero and values at least `1_000_000_000` fail. |
| Logical file size | Bytes represented by file content: inline `Data` length plus child logical sizes. Each `blocksizes` entry must equal the corresponding child's logical size. |
| DAG cumulative size | `Tsize` is the referenced block's serialized length plus its links' cumulative `Tsize` values. It is distinct from logical file size. Sum both size forms with checked `u64` arithmetic. |

Input DAG-PB must have sorted links and canonical fields. Both standard PBNode
orders, Data-first and Links-first, are accepted. Unknown fields, duplicate fields,
nonminimal CID/protobuf encodings, missing hashes, and missing `Tsize` fail.
Directory links also require `Name`. UnixFS metadata must use canonical protobuf
field order and unpacked `blocksizes`. Unknown metadata fields and unsupported
type-specific fields fail; the composer never silently drops them.

The composer checks each inspected block against its requested CID. It checks
directory-link and file-link `Tsize` against inspected DAG-PB child cumulative
sizes. It walks UnixFS file descendants and accepts raw-codec chunks, DAG-PB
UnixFS `Raw`, and recursive DAG-PB UnixFS `File` nodes. For each `File`, the
composer checks child types, link count, `blocksizes`, and `filesize` against
computed logical content sizes. Directory, HAMTShard, and Symlink nodes cannot
be file children.

Raw payload bytes remain opaque. The composer reads a raw-codec block when the
block is a standalone source or a file descendant so it can verify the CID and
use the block length as the logical size. It does not interpret payload contents. A raw-codec block used
only as an ordinary directory entry remains unfetched; when identical raw CIDs
have different directory-link sizes, the later link supplies the size.

## Supported inputs and limits

Layer roots must be ordinary UnixFS directories. Directory children can be
ordinary directories, raw-codec files, or DAG-PB UnixFS `File`, `Raw`, and
`Symlink` nodes. Every DAG-PB UnixFS `File` in a file sub-DAG must have
consistent inline size, `filesize`, `blocksizes`, and unnamed chunk links.
File descendants can be raw-codec chunks, recursive DAG-PB UnixFS `File`, or
legacy DAG-PB UnixFS `Raw` nodes. Legacy `Raw` nodes require an explicit
`filesize` equal to the inline data length, including `Some(0)` for empty data.
Missing `filesize` fails this profile rule. Symlinks require a UTF-8 target and
no child links. The composer does not resolve symlinks or accept them as file
content.

All input directory and file subtrees are inspected, including unchanged
subtrees and subtrees replaced by later layers. This makes unsupported-input
rejection independent of name collisions. Shared subtrees cache their validated
type, cumulative `Tsize`, logical file size where applicable, and height. A
raw-codec cache entry is upgraded if file validation needs its payload length.
Reuse at a deeper path cannot bypass the depth limit.

Composer v1 rejects all UnixFS `HAMTShard` nodes. It does not flatten shards or
transition ordinary directories into HAMTs. Ordinary encoded directories must
fit the block limit. Kubo 0.33.0 / Boxo 0.27.2 normally transitions to a fanout-256
HAMT when its legacy estimated directory size reaches 256 KiB. Composer v1 has
its own explicit block bound and does not reproduce that mutation policy.
An actual Kubo-generated 4,000-entry HAMT fixture tests root and nested rejection.

| Bound | Limit |
| --- | --- |
| Source operations, including an initial root | 128 |
| Entry depth | 128 from the layer root at depth zero |
| Distinct cached nodes, including generated nodes | 100,000 |
| Inspected block or generated directory block | 1 MiB |
| Total bytes read | 64 MiB |
| Total bytes generated, including intermediate layer results | 64 MiB |
| Directory-entry and file-link visits during validation, merge, and final reachability | 1,000,000 |
| CAR including framing | 64 MiB |
| Complete import response | 64 KiB |

Each bound is inclusive. The operation that would make a counter exceed the
limit fails. The node counter counts distinct cached CIDs, including generated
directories; upgrading a cached raw-codec node with its payload length does not
add a node. Read bytes count fetched block bodies once per validated CID.
Generated bytes count each new directory, including intermediate layer results.
Work counts directory entries and file links during validation, both directory
inputs during merge, and directory entries during final reachability. Each target
descent also charges one path visit and the current ancestor's entry count before
copying. Encoding and height scans use those already charged entry sets.

The existing name policy rejects empty names, `.`, `..`, slash, and ASCII control
bytes. Backslashes, punctuation, and other UTF-8 bytes remain exact. No Unicode
normalization is applied; issue #687 owns that policy.

## Storage and cancellation

Production `Deployment::bootstrap` recursively pins frozen layers.
`prepare_root` pins the current head before composition. Those existing owners
retain input DAGs. Direct `dag_merge` callers must likewise make input DAGs
locally available and retain them for the operation. The composer does not add
or remove input pins.

The client places all reachable generated directory blocks
in one CARv1 with exactly one root. It sends `/dag/import?pin-roots=true`.
[Kubo 0.33.0's importer](https://github.com/ipfs/kubo/blob/v0.33.0/core/commands/dag/import.go)
acquires `Blockstore.PinLock` before CAR ingestion. It holds the lock through
block-batch commit, recursive root pinning, and pin flush. Thus GC cannot collect
generated blocks between their storage and the root's retention. Import pinning
is offline; missing reused descendants fail the operation.

The client consumes the complete response and requires one matching root with
an empty `PinErrorMsg`. HTTP success alone is insufficient. A reused root uses
a header-only CAR with no block writes and the same validated pin acknowledgement.

Reads use the existing boot retry policy. Imports use one bounded attempt;
deployment owns retries. Cancellation drops pending transport operations and
is checked during structural traversal through cooperative yields. Failure or
cancellation can leave unpinned imported blocks, or a completed pin whose
response was lost. Neither condition reports successful composition. The import
is GC-safe, but it is not a rollback transaction.

## Removed machinery and follow-up boundaries

The production MFS composer, temporary merge namespaces, owner/reapable markers,
drop-time cleanup, stale namespace sweeper, and `BootMfs` wrapper are removed.
Generic `HttpClient` MFS methods and strict response parsers remain for existing
API tests and Kubo differential fixtures. No legacy MFS namespaces are deleted
automatically by the structural composer.

HAMT support, noncanonical protobuf compatibility, additional metadata, and
larger profiles require explicit profile decisions and fixtures. This change
does not implement Unicode normalization, alter `CidTree`, or replace Kubo.

## Validation fixtures

`image::codec` and `image::composer` tests run without HTTP. They cover overlay
semantics, metadata, CID reuse, fixed v1 root CIDs, malformed input, bounds,
and cancellation. An independent semantic model checks 64 root-only tree
sequences and every prefix of 192 ordered target-operation sequences. Another
64 cases compare root-operation identity with the root-only adapter. The model
uses neither production `apply_at` nor production `merge` as its oracle.

The richer identity vector composes three layers with nested directories,
non-ASCII names, mode and fractional mtime replacement, CIDv0 and CIDv1 inputs,
an unchanged CIDv0 subtree, a replaced subtree, files, raw nodes, and symlinks.
Its Composer v1 root is
`bafybeihsp2xyejzajvqbbuwz2kep2bgbzho2waqi6pp4dfdlaf7jsqqkaq`.
HTTP tests cover read/import failures and strict acknowledgements. Oversized
successful block responses produce typed resource errors for both Content-Length
and chunked bodies. Oversized HTTP error pages and interrupted reads retain
transport/protocol errors.

Run real Kubo fixtures with the repository's pinned Kubo 0.33.0 executable on PATH:

```sh
WW_TEST_REQUIRE_KUBO=1 cargo test -p cell --test composer_kubo
```

The fixtures compare visible trees and file CIDs with MFS, read generated DAGs
through Kubo, remove input pins and run GC, verify later-directory mode/mtime
and omission rules, check mtime fractions and legacy Raw sizes, and reject actual
HAMT input. They retain a Kubo-generated recursive chunked file and reproduce
rejection of File-to-Directory links and incorrect file `blocksizes`. They do
not require equality with historical MFS root CIDs. Targeted fixtures check
strict-ancestor metadata, target metadata omission, ordering and obstruction,
standalone raw/Raw/File sizes, CIDv0/v1 preservation, repeated chunks counted per
link in cumulative size, and retained composed roots during and after GC.

### Local fuzzing and deterministic replay

The CI fuzz lane pins Rust `nightly-2026-08-30` and `cargo-fuzz` 0.13.2. Match
that environment from the repository root:

```sh
rustup toolchain install nightly-2026-08-30
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo install cargo-fuzz --version 0.13.2 --locked

RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz build fuzz_composer_dagpb_decode
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz build fuzz_composer_unixfs_metadata
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz build fuzz_kubo_dag_import_response
```

Run a bounded 60-second mutation campaign for each Composer target:

```sh
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz run fuzz_composer_dagpb_decode fuzz/corpus/fuzz_composer_dagpb_decode -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz run fuzz_composer_unixfs_metadata fuzz/corpus/fuzz_composer_unixfs_metadata -- -max_total_time=60
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz run fuzz_kubo_dag_import_response fuzz/corpus/fuzz_kubo_dag_import_response -- -max_total_time=60
```

Replay one checked-in seed directly by passing its file as the corpus argument:

```sh
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz run fuzz_composer_dagpb_decode fuzz/corpus/fuzz_composer_dagpb_decode/duplicate_data_field
WW_FUZZ_SEMANTIC_REPLAY=1 RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz run fuzz_kubo_dag_import_response fuzz/corpus/fuzz_kubo_dag_import_response/accepted/canonical_success
```

`WW_FUZZ_SEMANTIC_REPLAY=1` enables the import-response semantic oracle used by
CI. Import-response regression seeds must begin with `expect:accept\n` or
`expect:reject\n`; the remainder is the Kubo response. Leave the variable unset
during mutation campaigns so mutations probe arbitrary parser input without
turning every changed expectation marker into a finding.

LibFuzzer writes a discovered failure under
`fuzz/artifacts/<target>/`. Replay and minimize that exact artifact with:

```sh
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz run <target> fuzz/artifacts/<target>/<artifact>
RUSTUP_TOOLCHAIN=nightly-2026-08-30 cargo fuzz tmin <target> fuzz/artifacts/<target>/<artifact>
```

Replay the minimized artifact, give it a descriptive stable name, and copy it
under `fuzz/corpus/<target>/`. For `fuzz_kubo_dag_import_response`, preserve or
add the applicable expectation marker and replay with
`WW_FUZZ_SEMANTIC_REPLAY=1` before committing the seed.

The deterministic model cases are library tests, not a separate integration
test target:

```sh
cargo test -p cell image::composer::property_tests::production_matches_independent_overlay_model_for_64_cases --lib --locked -- --exact --nocapture
```

A failure identifies its case number. Reproduce that case with the command
above, reduce its generated layers manually, and preserve the reduced input as
a fixed unit-test fixture. No automatic property-case shrinker is required.

References: [DAG-PB specification](https://ipld.io/specs/codecs/dag-pb/spec/),
[UnixFS specification](https://specs.ipfs.tech/unixfs/), and
[CARv1 specification](https://ipld.io/specs/transport/car/carv1/).
