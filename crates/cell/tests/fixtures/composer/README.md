# Kubo HAMT fixture

`hamt.car` contains the complete DAG exported by Kubo 0.33.0 (Boxo 0.27.2).
`hamt-root.dagpb` is the root block from that DAG. `hamt.json` records its CID,
generation parameters, and SHA-256 digest.

Generate the input directory with 4,000 files. For each index from 0 through
3,999, use the name `entry-{index:04}-abcdefghijklmnopqrstuvwxyz0123456789`.
Each file contains the single byte `x`. In an isolated initialized repository:

```sh
ipfs add -Qr --cid-version=1 --raw-leaves=true /path/to/input
ipfs block get "$fixture_cid" > hamt-root.dagpb
ipfs dag export "$fixture_cid" > hamt.car
```

Kubo 0.33.0 converts a basic directory to HAMT when the sum of UTF-8 name
lengths and encoded CID lengths reaches 262,144 bytes. The HAMT fanout is 256.
The fixture crosses that threshold through ordinary `ipfs add`, without
constructing synthetic HAMT metadata. Source:
[Boxo v0.27.2 directory.go](https://github.com/ipfs/boxo/blob/v0.27.2/ipld/unixfs/io/directory.go).

Composer v1 rejects HAMT inputs. The integration test imports this CAR,
verifies Kubo exposes all 4,000 files, and checks rejection at both the layer
root and a nested directory. The root block also permits decoder regression
tests without a running Kubo daemon.

Run the isolated integration suite:

```sh
WW_TEST_REQUIRE_KUBO=1 cargo test -p cell --test composer_kubo
```

The test requires `ipfs` version 0.33.0 on PATH. Each test starts and stops its
own offline Kubo daemon with an isolated repository and ephemeral API port.
Input layers are pinned before composition, matching the production caller's
retention responsibility. The differential test compares visible paths and
file CIDs with MFS, removes the reference namespace and input pins, runs GC,
and verifies the composed root remains readable.
