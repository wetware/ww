# crates

Rust libraries consumed by the host binary or shared between host and guests.
Nothing in `crates/` ships in the namespace directly.

| Crate | Role |
|-------|------|
| `authority/` | Epoch guards, authenticated sessions, and authority policy. |
| `cell/` | Host-side WASM execution, filesystem integration, and production FHS packaging validation. |
| `ipfs/` | Kubo/IPLD transport, bounded inline Map, CID identity and pure IPNS record verification. |
| `release/` | Release formats, authentication and consumer freshness; optional offline package construction/tool. |
| `membrane/` | Hook-level capability policy and recursive wrapping. |
| `rpc/` | Cap'n Proto and libp2p transport implementations. |
| `atom/` | On-chain Atom -- linearizable register backed by a smart contract. |
| `cache/` | CID cache -- content-addressed storage layer. |
| `guest/auth/` | Shared auth types -- common authentication structures. |
| `wagi-guest/` | CGI helpers for HTTP guest components. |

## vs std/

`crates/` = Rust libraries for host-side code and shared types.
`std/` = the Rust PID0, standard guest components, and the guest SDK.
