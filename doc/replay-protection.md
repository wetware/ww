# Replay Protection

This document describes how Wetware prevents replay and downgrade attacks
across the authentication and capability lifecycle.

## Threat model

An attacker who can observe or capture network traffic may attempt to:

1. **Replay a login signature** to impersonate a peer and obtain capabilities.
2. **Replay a stale epoch** to re-activate capabilities that should be dead.
3. **Use a signature from one context in another** (cross-protocol replay).

Wetware defends against all three through a layered model: domain-separated
signatures, epoch-bound challenges, and epoch guards on issued capabilities.

## Layer 1: Domain-separated signatures

Every signing context gets a unique `SigningDomain` (defined in `crates/guest/auth/`).
The domain string is embedded in the libp2p SignedEnvelope (RFC 0002) alongside the
payload. A signature produced for domain `"ww-terminal-membrane"` cannot verify
under domain `"ww-terminal-wallet"`, even if the nonce and key are identical.

Wire format (varint-length-prefixed):

```
varint(domain_len) domain
varint(payload_type_len) payload_type
varint(payload_len) payload
```

Well-known domains:
- `ww-terminal-membrane` — stable wire domain for Terminal login; a successful
  login may now return any typed, policy-constructed session.
- `ww-membrane-graft` — Legacy direct graft signing (pre-Terminal). This wire
  identifier remains stable across Rust crate renames.

## Layer 2: Epoch-bound challenge-response (Terminal login)

The Terminal authentication gate binds two values into every login challenge:

| Value | Source | Purpose |
|-------|--------|---------|
| `nonce` (u64) | OS CSPRNG (`rand::random()`) | Prevents replay within the same epoch |
| `epoch_seq` (u64) | `watch::Receiver<Epoch>` | Prevents cross-epoch reuse |

The signed payload is `nonce.to_be_bytes() || epoch_seq.to_be_bytes()` (16 bytes).

### Login flow

```
Client                          Terminal                    Signer
  │                                │                          │
  │  login(signer)                 │                          │
  ├───────────────────────────────>│                          │
  │                                │                          │
  │                  nonce = rand::random()                   │
  │                  epoch = epoch_rx.borrow()                │
  │                                │                          │
  │                  sign(nonce, epoch.seq)                   │
  │                                ├─────────────────────────>│
  │                                │  SignedEnvelope(          │
  │                                │    nonce || epoch_seq)    │
  │                                │<─────────────────────────┤
  │                                │                          │
  │                  verify signature + domain                │
  │                  verify payload == nonce || epoch_seq      │
  │                  verify epoch hasn't advanced              │
  │                  AuthPolicy.authorize(identity, template) │
  │                  build fresh attenuated session           │
  │                                │                          │
  │<───────────────────────────────┤                          │
  │  LoginStatus + session          │                          │
```

### Why both values are needed

- **Nonce alone** prevents replay within a session, but a captured signature
  could be reused after an epoch advance (before the EpochGuard catches it
  at capability-use time).
- **Epoch alone** would allow replay of any signature captured during the
  same epoch, since the epoch_seq is deterministic and predictable.
- **Together** they ensure a signature is valid only for one login attempt
  within one epoch. Neither value alone is sufficient.

### Race condition handling

The Terminal verifies that `epoch_rx.borrow().seq` still matches the
challenge's epoch sequence after the signer responds and again after the
asynchronous policy future completes. It commits the completed `SessionGrant`
only while the epoch is still current. Expected authentication/policy
rejections return a typed `LoginStatus` with no session; transport, malformed
protocol, and internal failures remain RPC errors.

## Layer 3: Epoch guards on capabilities

Every epoch-scoped graft capability and policy-issued session is wrapped with
an `EpochGuard` that captures the epoch sequence at issuance time. Every RPC
call checks the guard before proceeding:

```rust
pub fn check(&self) -> Result<(), Error> {
    let current = self.receiver.borrow();
    if current.seq != self.issued_seq {
        Err(Error::failed("staleEpoch: session epoch no longer current"))
    }
    Ok(())
}
```

When deployment accepts an authoritative Stem update, host-issued capabilities
guarded by the old local epoch fail simultaneously. This does not revoke
arbitrary non-host capabilities.
Trusted pid0 may call `Membrane.graft()` again. Ordinary children have no graft
surface: they receive fresh references only through explicit ancestor
re-delegation or respawn.

This is the runtime backstop. Even if Layers 1 and 2 were somehow bypassed,
a capability issued under epoch N cannot be used during epoch N+1.

Targeted `RevocationGuard`s can also invalidate one recipient or policy
decision inside an epoch. They compose with `EpochGuard`; they do not replace
the configured Stem as the deployment source.

## Layer 4: On-chain finality (Stem contract)

The Atom Source polls the canonical chain tip and reads `Atom.head()` at
`tip - confirmation_depth`. The default confirmation depth is six blocks.
Bootstrap and follow mode apply this same rule. `HeadUpdated` delivery is not
required for correctness. This mechanism provides:

- **Depth-bounded reorg handling**: only state at the configured chain depth
  becomes authoritative.
- **Canonical reconciliation**: polling observes the current canonical state
  after transport failures or missed events.
- **Contract progression**: each non-duplicate `setHead` call increments Atom's
  contract-local sequence, and polling reads the selected state directly.

The contract revision remains private to the Atom Source. `Epoch.seq` is a
host-local counter that starts at zero on each process start.

## Epoch authority advance

Deployment publishes each accepted local epoch before it prepares the
filesystem root. Existing capabilities then fail with `staleEpoch`.
Deployment publishes the same epoch with its effective root only after
preparation succeeds and old PID0 terminates.

`--epoch-drain-secs` is deprecated and inert. The Host accepts the option for
CLI compatibility, but the value does not delay the authority broadcast.

## Summary

| Layer | Mechanism | Defends against |
|-------|-----------|-----------------|
| Domain separation | SigningDomain in SignedEnvelope | Cross-protocol replay |
| Epoch-bound nonce | `nonce \|\| epoch_seq` in login payload | Same-epoch and cross-epoch replay |
| Epoch guards | `EpochGuard.check()` on every RPC | Stale capability use |
| On-chain finality | K-deep confirmation + canonical cross-check | Reorg and downgrade attacks |
| Authority-first epoch broadcast | Immediate capability invalidation before Host preparation | Stale-generation authority use |
