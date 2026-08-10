# Design: IPLD-backed Persistent HAMT for Val::Map

Status: REJECTED / SUPERSEDED — this proposal did not ship. Its value-semantics
problem statement was superseded by `value-contract.md`; the persistent
HAMT/IPLD roadmap was not adopted.
Date: 2026-04-04

## Problem

Val::Map is currently Vec<(Val, Val)> — a flat association list with O(n) lookup.
This blocks: persistent/immutable data structures for E-ordered concurrency,
content-addressed agent state, and efficient network transfer of state diffs.

## Proposal

Replace Val::Map with a 64-way persistent HAMT (Hash Array Mapped Trie) backed
by IPLD. Each node is a CID in the content store. Mutations return a new root
CID with structural sharing.

## Design

### 64-way branching (6 bits per level)

- 64-bit bitmap + up to 64 children per node
- popcount is a single CPU instruction (x86-64, ARM, WASM i64.popcnt)
- In WASM (32-bit pointers): full node = 264 bytes (~4 cache lines)
- Depth for 1B entries: 5 levels
- Shallower than 32-way (5 bits/level, 6 levels for 1B)

### Why 64-way over 32-way

- Same cache locality in WASM (32-bit pointers make 64-way same size as 32-way on 64-bit)
- Fewer content store lookups for lazy-loaded IPLD nodes (depth IS cost)
- 64-bit popcount is universally available

### IPLD backing

Each HAMT node serializes as an IPLD block (DAG-CBOR). Children are CID links.
The content store caches nodes. Lazy loading: only resolve CIDs when accessed.

Properties:
- **Immutable/persistent:** every mutation returns a new root CID. Old roots stay valid.
- **Structural sharing:** mutation touches log64(n) nodes. Everything else is shared CIDs.
- **Content-addressed:** root CID authenticates the entire map. Merkle verification.
- **Network diff:** two peers exchange root CIDs. HAMT diff = differing nodes only.
- **Time travel:** every prior state is a resolvable CID.

### Integration with Glia

```clojure
(def state {:balance 100 :name "alice"})     ;; persistent HAMT, returns root CID
(def state2 (assoc state :balance 50))        ;; new root, structural sharing
;; state still resolves to {:balance 100 :name "alice"}
;; state2 resolves to {:balance 50 :name "alice"}
```

### Integration with content store

The existing PinsetCache (crates/cache/) and CidTree (src/vfs.rs) provide the
content-addressed storage layer. HAMT nodes are stored/retrieved through the
same infrastructure that serves the virtual filesystem.

### Implementation path

1. HAMT crate with 64-way branching, in-memory first (no IPLD)
2. IPLD serialization (DAG-CBOR) for nodes
3. Lazy loading from content store (resolve CIDs on access)
4. Replace Val::Map(Vec) with Val::Map(Hamt)
5. Glia operations (assoc, dissoc, get, keys, vals) use HAMT natively

### Open questions

- Key hashing: BLAKE3 (already in the codebase) or a faster non-cryptographic hash for in-memory use?
- Inline small values vs always CID-link children?
- Bucket threshold: at what leaf size do we stop splitting and use a flat vec?
