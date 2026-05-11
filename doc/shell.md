# Shell

The `ww shell` command opens an interactive Glia REPL against a running
wetware daemon. Today only the local-UDS path is implemented; the
forward-stable CLI surface for remote shell access (libp2p multiaddr,
mDNS LAN browse) is documented below but exits with
`Error: NOT IMPLEMENTED`.

## Connecting

```sh
ww shell                                          # connect to local daemon via UDS
ww shell <multiaddr>                              # NOT IMPLEMENTED — future remote dial
ww shell --discover                               # NOT IMPLEMENTED — future LAN browse
```

With no arguments, `ww shell` scans the run directories (`/var/run/ww/`
first on Linux, `$HOME/.ww/run/` fallback on macOS and when `/var/run/`
isn't writable) for `<peer-id>.sock` files. If exactly one daemon is
running locally, it connects. If multiple are running, it prompts you
to choose. If none are running, it errors with a hint to run
`ww run .` first.

## Local admin gate (UDS)

The local path is an admin endpoint by design. Whoever can write to
`~/.ww/run/` (or `/var/run/ww/`) has full administrative control of
the daemon — by convention with `/var/run/docker.sock`,
`~/.ipfs/api`, `~/.podman/podman.sock`, and similar local-CLI sockets.
Filesystem permissions on the run directory ARE the auth boundary;
there is no Noise handshake, no Terminal challenge, no auth token.

The spawned shell cell receives the daemon's **full membrane** — every
capability the daemon exposes, without attenuation. Admin scope is
exempt from epoch-based capability expiry: the shell remains usable for
the daemon's lifetime regardless of stem activity.

If you need auth-gated remote shell access in the future, that's a
separate `:listen` registration via init.d, with a `Terminal(Shell)`
gate per the April-2 design — see the
[design doc](https://github.com/wetware/ww/issues/452) for context.

## Syntax

Every expression is an S-expression. Effects use the `perform` form:
the first argument is the capability, the second is a keyword naming
the method, and the rest are method arguments.

```
(perform capability :method [args...])
```

Strings are double-quoted. Symbols are bare words. Comments start with
`;` and run to end of line.

## Capabilities exposed to the shell

The shell cell currently grafts these caps from its membrane (see
`std/shell/src/lib.rs::run_impl`):

### host

| Method     | Example                                                       | Description                            |
| ---------- | ------------------------------------------------------------- | -------------------------------------- |
| `id`       | `(perform host :id)`                                          | Peer ID (bs58-encoded string)          |
| `addrs`    | `(perform host :addrs)`                                       | Listen multiaddrs                      |
| `peers`    | `(perform host :peers)`                                       | Connected peers with addresses         |
| `connect`  | `(perform host :connect "/ip4/1.2.3.4/tcp/2025/p2p/12D3...")` | Dial a peer                            |
| `listen`   | `(perform host :listen "/ip4/0.0.0.0/tcp/0")`                 | Listen on an additional address        |

### routing

| Method          | Example                                       | Description                          |
| --------------- | --------------------------------------------- | ------------------------------------ |
| `provide`       | `(perform routing :provide cid)`              | Announce as provider for a CID       |
| `findProviders` | `(perform routing :findProviders cid)`        | Find providers for a CID over DHT    |

### Local effect handlers

The shell cell also wires three glia-only effect handlers that do not
go through a remote capability:

- **`fs`** — read paths from the cell's WASI filesystem (reactive to
  stem updates per [`architecture.md`](architecture.md))
- **`import`** — load other Glia source files
- The kernel's built-in expressions: arithmetic, `let`, `if`, `defn`,
  etc.

### Built-ins

| Form               | Description                                              |
| ------------------ | -------------------------------------------------------- |
| `(def name value)` | Bind a value in the session environment (persists)       |
| `(help)`           | Print available capabilities and methods                 |
| `(exit)`           | Disconnect cleanly                                       |

`def` state and any other env bindings persist for the lifetime of
the connection. A new `ww shell` session starts with a fresh cell
and an empty environment.

## Connection model

Each `ww shell` invocation spawns a fresh shell cell on the daemon
side via `executor.spawn_request()` — sessions are isolated by
construction. The cell exits cleanly when you `(exit)` or close stdin
(Ctrl-D).

The daemon side bridges your `tokio::net::UnixStream` to the cell's
WASI stdio via the existing `handle_vat_connection_spawn` helper
(`crates/rpc/src/vat_listener.rs`) — the same one used by the libp2p
path. The cell itself doesn't know which transport you connected over;
it sees a generic Cap'n Proto duplex.

For implementation details of the daemon-side service, see
[`src/admin_uds.rs`](../src/admin_uds.rs).
