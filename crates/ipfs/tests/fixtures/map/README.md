# Inline Map v1 fixtures

`profile.json` freezes the numeric profile. Each `.hex` file contains canonical
DAG-CBOR bytes; the matching `.cid` file identifies those bytes with
CIDv1 / DAG-CBOR / SHA-256. CID fixtures were independently calculated from
fixture bytes with Python's `hashlib` and base32 encoding.

- `empty`: `[1, {}]`.
- `ordered`: `[1, {"a": true, "zz": -1}]`, including unequal-length key ordering.
- `nested`: all scalar types, signed i64 endpoints, bytes, a tag-42 CID link,
  a list and a nested map.

The envelope is exactly `[1, <string-key map>]`. Envelope elements do not count
against list-element or depth budgets. The root map has depth zero; each edge
to a map value or list element adds one. Keys must contain valid UTF-8 and use
exact byte equality, without Unicode normalization. String keys sort by their
canonical encoded length, then encoded bytes; for text strings this is byte
length followed by lexical bytes. Logical iteration uses UTF-8 lexical order.

All nested maps contribute to the 4096-entry budget. All nested lists contribute
to the 16384-element budget. The complete envelope must fit in 1048576 bytes.
The tests exercise exact boundaries and one over each boundary. No content is
loaded or retained by Map drop. `Blockstore` supplies explicit async storage;
retention belongs to the release owner.

The crate-private release graph traversal accepts a valid Map envelope under
these budgets. It also accepts a standalone profile value under the same
budgets with that value at depth zero. Generic traversal validates codec
structure and links, not descriptor versions or release authentication.
