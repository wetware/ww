# Key Management

## Design decisions

### Ed25519 node keys

wetware uses **Ed25519** for node identity. The decision separates two
concerns that were previously conflated:

1. **Node identity (Ed25519)** — the `PeerId` derived from the Ed25519 public
   key identifies the node on the p2p network and is used for Terminal
   challenge-response signing. Ed25519 is libp2p's default and best-supported
   key type: simpler (32-byte seed, 32-byte pubkey, no compressed/uncompressed
   distinction), and the ecosystem default.

2. **Operator identity (secp256k1)** — the Stem contract owner key. This is
   the one key that calls `setHead()` to advance the on-chain epoch. It is
   an operator concern, not a node concern. Nodes are chain readers (they
   watch HeadUpdated events via the AtomIndexer); they never transact.

**Why the split:** The original "one key, two roles" design gave every node
an EVM address it never used. Nodes don't transact on-chain. Only the cluster
operator does. Tying node identity to secp256k1 for a speculative future use
case (agentic wallets, on-chain metering) is premature. If nodes ever need
on-chain identity, the binding can be solved separately via a registry
contract, signed attestation, or ERC-4337 Account Abstraction with Ed25519
verification.

**Implementation:** PR #289 completed the migration. `k256`/`sha3` dropped from
the host binary; `ed25519-dalek` added. `ethereum_address()` removed. Terminal
challenge-response auth uses `try_into_ed25519()`. Guest auth
(`crates/guest/auth`) was already algorithm-agnostic and required no changes.

### Key storage

Keys are stored as **base58btc** (Bitcoin alphabet, ~44 characters for 32 bytes)
in a plain text file. Hex-encoded keys are also accepted on load for backward
compatibility. The default location is `~/.ww/key`.

Rationale:
- Denser than hex (44 vs 64 chars), no ambiguous characters (no 0/O/I/l).
- Native to the IPFS ecosystem (same alphabet as CIDv0 and libp2p Peer IDs).
- Key files belong on encrypted volumes or in a secrets manager at the
  infrastructure level, not wrapped in application-level encryption that just
  moves the password storage problem.
- Private key material **never touches IPFS** or any other content-addressed
  store. Even if the node CID is public, the key file stays local.

### Identity resolution

`ww run` resolves the node identity in this order (first match wins):

1. `--identity PATH` — explicit path to a key file
2. `$WW_IDENTITY` — environment variable pointing to a key file
3. `~/.ww/identity` — default path (when `HOME` is set)
4. Ephemeral — only when `--insecure-ephemeral` is set

Each time the host resolves the identity it logs the source at `INFO` level
so the active source is always visible in the log output.

The ephemeral fallback is fine for local development and testing but means
the node's Peer ID and EVM address change on every restart. Use a persistent
key for any deployment that other nodes need to remember across restarts.

For daemon/service mode, pass identity through the same host flags/env:
`--identity PATH` or `WW_IDENTITY=PATH`.

## Usage

```sh
# Print a new secret to stdout (metadata on stderr)
ww keygen

# Save to a file
ww keygen --output ~/.ww/key
ww keygen > ~/.ww/key          # equivalent

# Run with a persistent identity
ww run --identity ~/.ww/key images/my-app
```

## File format

```
# ~/.ww/key — base58btc, ~44 chars (hex also accepted on load)
6MRyAjQq8ud7hVNYcfnVPJqcVpscN5So8BhtHuGYqET5
```

`ww keygen` prints the secret to stdout and metadata to stderr:

```
$ ww keygen 2>/dev/null
6MRyAjQq8ud7hVNYcfnVPJqcVpscN5So8BhtHuGYqET5

$ ww keygen --output ~/.ww/key
Secret written to: /home/user/.ww/key
Peer ID:        12D3KooWAbcDef...
```
