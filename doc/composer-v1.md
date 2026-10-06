# Wetware Composer v1

`cell::image::COMPOSER_PROFILE` identifies `wetware-composer-v1`. For accepted
inputs, the same ordered layer CIDs and this profile produce the same root CID.
Historical Kubo MFS root identity is not part of the contract.

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
block is a file descendant so it can verify the CID and use the block length as
the logical size. It does not interpret payload contents. A raw-codec block used
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
| Layers | 128 |
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
inputs during merge, and directory entries during final reachability.

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
and cancellation. HTTP tests cover read/import failures and strict
acknowledgements.

Run real Kubo fixtures with the repository's pinned Kubo 0.33.0 executable on PATH:

```sh
WW_TEST_REQUIRE_KUBO=1 cargo test -p cell --test composer_kubo
```

The fixtures compare visible trees and file CIDs with MFS, read generated DAGs
through Kubo, remove input pins and run GC, verify later-directory mode/mtime
and omission rules, check mtime fractions and legacy Raw sizes, and reject actual
HAMT input. They retain a Kubo-generated recursive chunked file and reproduce
rejection of File-to-Directory links and incorrect file `blocksizes`. They do
not require equality with historical MFS root CIDs.

References: [DAG-PB specification](https://ipld.io/specs/codecs/dag-pb/spec/),
[UnixFS specification](https://specs.ipfs.tech/unixfs/), and
[CARv1 specification](https://ipld.io/specs/transport/car/carv1/).
