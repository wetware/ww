# #738 publication negative control

This regression module reproduces a publication ordering failure in **unmodified
Boxo v0.27.2**, the Boxo version pinned by Kubo v0.33.0. It uses real Ed25519 IPNS
records, the actual `offline.NewOfflineRouter`, and **go-ds-leveldb v0.5.0 on a
temporary disk directory**, matching Kubo v0.33.0's default root datastore plugin.
It does not modify a dependency fork or connect to a Kubo daemon.

Run from this directory:

```sh
go test -v -count=1 .
WW_REQUIRE_MONOTONIC=1 go test -v -count=1 .
```

The first command must exit 0: the sequential stale-record control is rejected,
and the deliberately scheduled race returns authenticated sequence 1 after
sequence 2 was acknowledged. The second command must exit 1 because the same
trace violates the desired monotonic publication assertion. The test verifies the expected regression directly; the negative-control mode
intentionally fails the stronger contract.

## Exact trace

1. Seed one IPNS name with signed sequence 0.
2. Submit signed sequence 1. The real router validates the record, reads 0,
   and selects 1 as newer. A datastore wrapper pauses the operation upon entry
   to its final `Put`, before forwarding that write to LevelDB.
3. Cancel that first operation's context. The accepted datastore write remains
   in flight, modeling an ambiguous publication result after a waiter disappears.
4. Retry the exact sequence-1 record. The retry reads 0, writes 1, and succeeds.
5. Publish signed sequence 2. The operation reads 1, writes 2, and succeeds.
   A verified `GetValue` confirms sequence 2.
6. Release the paused first operation. LevelDB accepts the old write even though
   its context is canceled. The first operation succeeds.
7. Read and independently validate the stored IPNS record. Its sequence is **1**.

All three records share one key/name. No competing external writer, key theft,
mutable descriptor, invalid signature, same-sequence equivocation, clock expiry,
or rollback of the local publisher journal is involved. The retry and newer
publication occur sequentially; only the original ambiguous backend operation
continues concurrently.

## Why cancellation is a valid assumption

`go-ds-leveldb v0.5.0`'s `accessor.Put(ctx, key, value)` accepts a context but does
not inspect it. It calls `a.ldb.Put` with synchronous writes. The leveldb API
receives no context. Therefore cancellation of an HTTP request or publication
future does not establish that an already-entered datastore write cannot finish.

## Sources

- [Kubo v0.33.0 dependency pins](https://github.com/ipfs/kubo/blob/v0.33.0/go.mod):
  Boxo v0.27.2, go-ds-leveldb v0.5.0, kad-dht v0.28.2.
- [Boxo offline router](https://github.com/ipfs/boxo/blob/v0.27.2/routing/offline/offline.go#L43):
  validation, `GetValue`, selection, and final datastore `Put` are separate.
- [Kubo Routing V1 adapter](https://github.com/ipfs/kubo/blob/v0.33.0/core/corehttp/routing.go#L79):
  `PutIPNS` delegates to the node's `Routing.PutValue`.
- [Kubo default datastore](https://github.com/ipfs/kubo/blob/v0.33.0/config/init.go):
  root mount uses levelds; `/blocks` uses flatfs.
- [LevelDB plugin](https://github.com/ipfs/kubo/blob/v0.33.0/plugin/plugins/levelds/levelds.go#L74):
  creates go-ds-leveldb.
- [LevelDB Put](https://github.com/ipfs/go-ds-leveldb/blob/v0.5.0/datastore.go#L78):
  context is not inspected or forwarded.
- [HTTP Routing V1 IPNS PUT](https://specs.ipfs.tech/routing/http-routing-v1/#put-routing-v1-ipns-name):
  no expected predecessor or conditional-update token is specified.

## Limits

The barrier deliberately selects the harmful schedule. This proves that the
unmodified record router and actual default datastore do not atomically protect
the selection-plus-write operation. It does **not** reproduce the schedule through
a complete Kubo HTTP server, prove how often the schedule occurs in a deployed
daemon, test DHT propagation, or simulate power loss. No production publication
or signing key is used. A consumer that already persisted sequence 2 rejects the
sequence-1 observation; a fresh consumer has no such floor.

Environment: `go version go1.27.3 darwin/arm64`. Exact Go modules and checksums
are recorded in `go.mod`, `go.sum`, and `boxo-version.json`.
