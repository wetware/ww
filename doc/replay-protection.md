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

When the epoch advances (on-chain `HeadUpdated` event, finalized by the
confirmation-depth strategy), all outstanding capabilities fail simultaneously.
The agent must call `Membrane.graft()` again to receive fresh capabilities
bound to the new epoch.

This is the runtime backstop. Even if Layers 1 and 2 were somehow bypassed,
a capability issued under epoch N cannot be used during epoch N+1.

Targeted `RevocationGuard`s can also invalidate one recipient or policy
decision inside an epoch. They compose with `EpochGuard`; they do not replace
Atom as the global epoch source.

## Layer 4: On-chain finality (Stem contract)

The epoch sequence is anchored to the Stem contract's `HeadUpdated` event.
The `Finalizer` requires K-deep confirmation (default: 6 blocks) before
accepting an event, and cross-checks every event against the canonical
on-chain state via `eth_call` to `Atom.head()`. This prevents:

- **Reorg attacks**: events on reorged forks are silently discarded.
- **Downgrade attacks**: replaying an old organization snapshot fails because
  the on-chain `seq` has advanced past the stale value.

## Graceful epoch shutdown (drain)

Epoch transitions support an optional drain duration. When configured:

1. New CID is pinned and CidTree is swapped (FS serves new content).
2. **Drain window** begins. In-flight operations on old capabilities continue.
3. After the drain expires, `epoch_tx.send(new_epoch)` fires.
4. All old capabilities die with `staleEpoch`.

The drain provides graceful shutdown semantics (SIGTERM before SIGKILL)
without weakening security: no new capabilities are issued for the old
epoch during the drain, and the window is bounded and configurable.
Default: 1 second (`--epoch-drain-secs 1`). Set to 0 for instant
epoch advance.

## Summary

| Layer | Mechanism | Defends against |
|-------|-----------|-----------------|
| Domain separation | SigningDomain in SignedEnvelope | Cross-protocol replay |
| Epoch-bound nonce | `nonce \|\| epoch_seq` in login payload | Same-epoch and cross-epoch replay |
| Epoch guards | `EpochGuard.check()` on every RPC | Stale capability use |
| On-chain finality | K-deep confirmation + canonical cross-check | Reorg and downgrade attacks |
| Graceful drain | Configurable delay before epoch broadcast | In-flight operation interruption |
