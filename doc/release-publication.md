# Public release formats and private publication

`ww-release` defines release authentication, descriptors, consumer freshness and
offline package construction. Private `wetware/infra` owns official publication
policy, the sole durable `Owner`, journal, recovery, Kubo announcement and signing
workflow. Production publication remains held. This extraction authorizes no
credential provisioning, deployment or activation.

## Map v1

A Map block is canonical DAG-CBOR `[1, {entries...}]`. Its CID is
CIDv1 / DAG-CBOR (`0x71`) / SHA-256 (`0x12`, 32 bytes).
Keys are exact UTF-8 strings. No Unicode normalization occurs.
Values are null, Boolean, signed i64, text, bytes, CID links, lists, or nested
string-key maps. CID links use tag 42 and a zero-prefixed binary CID.
The binary CID decoder consumes the full input and requires an exact round trip.
A link is a content identity; it does not authenticate or retain its target.

| Limit | Value |
| --- | ---: |
| Encoded block, including envelope | 1,048,576 bytes |
| Depth, excluding envelope | 32 |
| Total map entries, including nested maps | 4,096 |
| Total list elements, including nested lists | 16,384 |
| UTF-8 bytes per key | 1,024 |

Depth counts edges from the root map, which has depth zero. Empty containers
at depth 32 are accepted; their children would exceed the limit. The envelope
consumes neither map/list counters nor depth. The 1 MiB block limit follows
the existing composer profile and remains below the 2 MiB transport bound.
Golden files in `crates/ipfs/tests/fixtures/map` freeze the profile, bytes,
and CIDs.

The decoder rejects nonminimal integers/lengths, indefinite lengths, floats,
unsupported tags, duplicate or out-of-order keys, non-string keys, invalid
UTF-8, unknown envelope versions, trailing data, and exceeded limits. It checks
lengths and collection budgets before allocating decoded collections.
Canonical wire ordering follows encoded-key length then encoded-key bytes,
not ordinary string order. See the [DAG-CBOR specification](https://ipld.io/specs/codecs/dag-cbor/spec/).

`Map::get`, `iter`, `insert`, and `remove` operate on validated inline values.
Updates clone the bounded representation and return new Maps. Nested maps and
lists use ordinary Rust `BTreeMap` and `Vec` values, without structural sharing.
The entire Map encodes into one block. Iteration uses UTF-8 lexical order and
returns at most 4,096 entries. `from_entries` bounds ingestion even for an
unbounded iterator.
`encode` and `cid` are fallible. `persist` and `load` use an explicit async
`Blockstore`; `load` bounds acquisition and verifies the returned bytes.
The supplied store must honor its byte limit before allocating content.
Logical operations and Map destruction have no storage, pin, or cleanup effect.
CID equality is specific to this format; it is not promised across future
inline/tree representations. There is no guest WIT API or lazy traversal.

### Relationship to future persistent collections

`Value` is a versioned inline storage profile, not a universal Wetware value
model. This Map is not the definitive persistent-collection API explored in
[#698](https://github.com/wetware/ww/issues/698). Persistent Maps, bit-partitioned
vector tries, and other IPLD-backed collections may require different value
types, representations, APIs, and lifetime contracts.

That architecture will be revisited under #698 after the release/upgrade MVP.
The future work must evaluate reuse or evolution of this inline implementation
before introducing an independent Map. This foundation selects neither a
general value model nor a future collection API.

### Single-block link decoding

`ipfs::map::inline_v1::links(bytes)` returns links from one bounded inline-profile
value or version-1 Map envelope. The helper preserves both accepted forms,
including standalone lists that are not Map envelopes. It does not verify a
block's CID, walk a DAG, establish closure or decide retention.

Package DAG-CBOR decoding still requires a Map envelope. The public package
implementation does not substitute the broader inline-profile decoder.

## Crate and executable boundary

| Component | Owner and interface |
| --- | --- |
| `ww-release` default features | `auth`, `descriptor`, authenticated selection and consumer freshness |
| `ww-release` feature `package` | `Metadata`, `prepare`, `finalize`, `Package`, CAR/tree transfer functions and offline `ww-release-package` |
| `ipfs` | Map, CID identity, pure `ipns` records, bounded CARv1 framing and Kubo/IPLD transport |
| `cell::packaging` | Production FHS component validation and staging |
| `ww` | Host deployment `Publisher`, `RecordStore`, `Republisher`; compatibility reexports |
| Private `ww-release-publisher` | Official metadata/sequence policy, stage planning, single durable Owner and announcement |

`ww::release::{auth,descriptor}`, `ww::packaging`, and the pure items under
`ww::ipns` reexport the same types and functions. The extraction removes
`ipfs::release`, including its 11 public Owner/policy declarations. This is an
intentional pre-1.0 API removal. No public journal or publication-policy adapter
replaces that module.

Private infra depends on `ww-release` and `ipfs` at one exact public Git revision.
It does not depend on root `ww` or its embedded-guest build prerequisites.
The package feature still uses Cell/Wasmtime for production FHS validation.
Public crates have no private dependency.

`Package` exposes `open`, `root`, `authenticate`, `fetch`, `read_block`, and `links`.
Construction and acquisition check graph closure, CID integrity, supported codecs,
canonical block formats, depth, block count and byte limits. The graph pass
bounds distinct blocks and examined edges and checks cached subtree height.
Private stage partitioning and its traversal budget remain private policy.
Acquisition does not authenticate a release; callers must supply independent
trust to `authenticate`.

Four free functions support offline transfer without adding publication policy:
`pack_tree`, `unpack_tree`, `read_car`, and `write_car`. Unsigned input trees use
the same UnixFS builder as release packages. Signed CAR export preserves the
existing release root and every reachable block. CAR import checks bounded
framing, CID integrity and exact graph closure before returning a package.
Unsigned tree materialization also bounds expanded bytes, file/directory counts,
path depth and total traversal work; shared blocks do not bypass those bounds.

The transfer profile uses one CARv1 root and deterministic encoded-CID block
ordering. It rejects duplicate, missing and unreachable blocks. CAR framing
has a separate bounded allowance above the existing 8 GiB payload budget.
`ipfs::car::{Reader,Writer,Limits}` supplies shared streaming framing. Kubo import
continues to use bounded CAR stages and its existing acknowledgement contract.
Neither CAR import nor a staging Kubo pin supplies publication retention authority.

```sh
cargo test --locked -p ww-release
cargo test --locked -p ww-release --features package
cargo test --locked -p ipfs
cargo build --release --locked -p ww-release --features package --bin ww-release-package
```

## Authentication and release graph

`ww_release::descriptor::Descriptor` uses canonical versioned IPLD Map bytes.
The descriptor contains real links to the FHS and native artifacts, platform
and ABI identity, file sizes and SHA-256 digests, actual source/build provenance,
and the Host/PID0 compatibility profile. Runtime file digests differ from UnixFS
storage-root identities. Reused Host artifacts retain their actual build revision.

The graph remains acyclic: artifact roots → descriptor → `RELEASE-DESCRIPTOR`
binding → manifest → detached signature → final UnixFS transport root. The final
root directly links the descriptor as `release-descriptor` and preserves legacy
installer paths. The binding is exactly canonical descriptor CID text plus LF;
its ordinary SHA-256 entry is signed by the existing #412 Ed25519 authority.
The descriptor never includes the signature, manifest hash, or enclosing root.

Consumers authenticate the manifest and binding before descriptor interpretation.
Artifact selection requires a supported platform, matching compatibility profile,
and verified artifact bytes. The consumer watermark stores the raw signed IPNS
record and authenticated release proof in one locked durable state file. It
rejects lower IPNS sequences, equal-IPNS-sequence value changes, lower signed
release sequences, and equal-release-sequence descriptor changes. Expiry does not
erase either floor. Explicit historical selection authenticates and checks
compatibility without updating channel freshness state.

The existing shell installer continues to authenticate its manifest and payloads.
It does not interpret descriptors or persist freshness watermarks. Legacy readers
therefore retain their existing replay limitation during migration. #739 owns
Host replacement; #740 owns CLI migration. Neither is changed here.

## Discovery and local authority

Official ordering belongs to infra's single durable Owner. IPNS remains eventual
discovery. The Host's deployment-IPNS publisher has a separate lifecycle.

Three events have different meanings:

| Event | Meaning |
| --- | --- |
| Local commitment | The owner selects a completely retained, authenticated release and persists its immutable announcement request in the same journal transaction. |
| Delivery acknowledgement | Kubo accepted one request for the committed root. The journal can record that acknowledgement. |
| Eventual discoverability | A consumer finds the signed IPNS record and fetches its release graph. No local transaction or acknowledgement proves this event globally. |

A delayed older request can finish after a newer request and temporarily expose
an older release. Consumers with an accepted signed release watermark reject
that release. A fresh consumer has no floor and cannot prove it found the latest
signed release through IPNS alone. The tracked
[`tests/publication-negative-control`](../tests/publication-negative-control/README.md)
reproduces the delayed-write counterexample with unmodified Boxo v0.27.2 and
LevelDB. The counterexample is regression evidence, not a production incident.

## Qualification and holds

Public tests preserve canonical bytes/CIDs, signatures, provenance, strict package
closure and consumer freshness. Real FHS tests use the production Cell validator.
The public Boxo delayed-write control remains independently reproducible.

Private infra owns the 28 crash checkpoints, lock probes, journal recovery,
retention ordering, direct Kubo tests and private executable qualification.
Process-crash tests do not prove power-loss durability. Local Kubo tests do not
qualify production storage, worker isolation, credentials or public discovery.

The public `publish` workflow job remains unconditionally disabled and refuses
publication. `make publish` refuses before builds or external calls. The private
signing/publication workflow also remains inactive. Existing publication fences
and the production hold remain in force.

See [the public package handoff](release-publisher-infra-contract.md),
[authentication](release-authentication.md), and
[infra's operator contract](https://github.com/wetware/infra/blob/master/ops/vps/publisher-contract.md).
