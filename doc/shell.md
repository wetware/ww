# Shell

`ww shell` connects to a running node and opens a Glia REPL.
Transport/auth stays remote (`/ww/0.1.0` + `Terminal(Membrane)`), while
Glia evaluation runs locally in the `ww shell` process.

## Modes

```sh
ww shell
ww shell --select <index|peer-id>
ww shell <multiaddr>
```

- `ww shell`: discover hosts via local host-state (`~/.ww/run/host.json`,
  or `$WW_HOST_STATE_PATH` override). If multiple hosts are found, interactive
  TTY sessions prompt for a selection; non-TTY sessions fail with explicit next
  steps.
- `ww shell --select <index|peer-id>`: choose a discovered host
  non-interactively.
- `ww shell <multiaddr>`: dial an explicit target.

## Auth

Shell transport uses libp2p streams and Terminal(Membrane) challenge-response
authentication. `ww shell` signs terminal challenges with the local identity
key (`WW_IDENTITY` or `~/.ww/identity`).

## Execution model

- Connect path performs `dial -> login -> graft`.
- `ww shell` does **not** start a daemon-side shell process
  (`runtime.load`/`executor.spawn`/`process.bootstrap` are not used by shell
  connect).
- A process-local Glia environment is created in the CLI process and populated
  with grafted capabilities.

## Path semantics

- `(perform :load "/ipfs/...")`, `(perform :load "/ipns/...")`, and
  `(perform :load "/ipld/...")` route through the grafted `ipfs` capability
  (`system.Ipfs.read`).
- `(perform :load path)` uses the shell process local filesystem for non-IPFS
  paths.
- `ww shell` never talks directly to Kubo.

## Standard host effects

Glia code in the shell uses explicit effects for embedding work:

```clojure
(perform :load "glia/register.glia")
(perform :stdout value)
(perform :exit nil)
```

The terminal embedding loads and prints normally. The local CLI shell turns
`:exit` into a sentinel that its outer loop handles. In `ww shell --mcp`,
`:stdout` and `:exit` fail with a typed protocol-mode-unavailable error rather
than corrupting JSON-RPC stdout; `:load` remains subject to the configured
loader. Effects expose the semantic interaction boundary only. Grafted
capabilities and their membranes remain the authority boundary.

## Multi-Result Discovery

When discovery returns multiple candidates and no deterministic preferred target
is found, `ww shell` uses this order:

1. Prefer the local identity peer id if exactly one discovered host matches.
2. If in TTY mode, show an interactive selector.
3. Otherwise, return an error and suggest `--select` or an explicit
   `<multiaddr>`.

## Troubleshooting

- `No local wetware host discovered`: confirm daemon is running and host state
  exists at `~/.ww/run/host.json` (or your `$WW_HOST_STATE_PATH` override).
- `login auth failed: signing key does not match expected identity`: use the
  same identity for daemon and shell (`WW_IDENTITY` / `~/.ww/identity`).
- If discovery is ambiguous, use `ww shell --select <index|peer-id>` or pass an
  explicit multiaddr.
