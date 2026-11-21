# Release package content transport

The optional `package` feature provides offline release construction and content
transport. It does not choose a channel or sequence, hold signing authority,
publish to Kubo, or claim retention.

The four content-only functions are separate from `Package`'s existing six
methods:

| Function | Operation |
| --- | --- |
| `pack_tree(tree, spool)` | Encode an unsigned filesystem tree with the existing canonical UnixFS builder. |
| `write_car(package, car)` | Export exactly the reachable graph, ordered by encoded CID bytes. |
| `read_car(car, spool)` | Verify bounded CAR framing, block hashes, and exact root closure into a fresh spool. |
| `unpack_tree(package, tree)` | Validate and materialize the unsigned tree profile into a fresh private directory. |

Each destination must not exist. Failures remove the newly created destination;
existing files and directories are never replaced. Callers must own the private
source and destination parent for the duration of each operation. Transport is
content-only: filesystem modes, timestamps, and ownership are not encoded.
Materialized directories and files use private permissions on Unix.

The unsigned tree profile accepts regular files and plain UnixFS directories.
Names use ASCII letters, digits, `.`, `_`, and `-`; empty names, `.` and `..`,
path separators within names, and case-insensitive collisions are rejected.
Relative paths are limited to 512 bytes and 64 edges. The root counts toward the
limit of 1,024 directories; there may be at most 1,024 files. Expanded file bytes
across the entire tree are limited to 8 GiB, and expanded traversal work is
limited to 131,072 block visits. Shared DAGs do not bypass these global limits.
The complete materialization plan is checked before creating the destination.

`RELEASE-DESCRIPTOR.cbor`, `RELEASE-DESCRIPTOR`, `release-descriptor`, and
`CHECKSUMS.txt.sig` are reserved names at every depth, case-insensitively. They
are rejected rather than omitted. An unsigned `CHECKSUMS.txt` remains ordinary
content. This profile can carry release inputs and their build receipts without
allocating the private operator's metadata or signing authority.

CAR import/export supports signed packages as well as unsigned trees. It uses
single-root CARv1, with at most 32,768 blocks, 2 MiB per block, and 8 GiB of block
payloads. A separate checked framing allowance covers the bounded header, CIDs,
and record lengths. Import rejects duplicate records, unreachable or missing
blocks, noncanonical framing, and unsupported codecs or hashes. Content checks
do not authenticate a release: call `Package::authenticate` with independent
trust before using a signed release. Signed package roots are not accepted by
the unsigned tree materializer.

These are public Rust library APIs. The single operational `ww-release-package`
executable lives in private `wetware/infra`, imports this library, and owns
preparation, signing, finalization and transport orchestration. This package has
no executable target. Independent verification remains available through
`release::auth::TrustedKeys`, `release::package::Package::authenticate`, the public
installer and the existing `ww` installation/upgrade commands.

The `release` package name replaces `ww-release`; Rust consumers import `release`.
Descriptors, manifests, signatures, CARs and CIDs do not change with this rename.
