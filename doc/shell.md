# Shell

`ww shell` connects to a running node and opens a Glia REPL.

## Modes

```sh
ww shell
ww shell --select <index|peer-id>
ww shell <multiaddr>
```

- `ww shell`: discover hosts via mDNS. If multiple hosts are found:
  interactive TTY sessions prompt for a selection; non-TTY sessions fail
  with explicit next steps.
- `ww shell --select <index|peer-id>`: choose a discovered host
  non-interactively.
- `ww shell <multiaddr>`: dial an explicit target.

## Auth

Shell transport uses libp2p streams and Terminal(Membrane) challenge-response
authentication. `ww shell` signs terminal challenges with the local identity
key (`WW_IDENTITY` or `~/.ww/identity`).

## Multi-Result Discovery

When mDNS returns multiple candidates and no deterministic preferred target
is found, `ww shell` uses this order:

1. Prefer the local identity peer id if exactly one discovered host matches.
2. If in TTY mode, show an interactive selector.
3. Otherwise, return an error and suggest `--select` or an explicit
   `<multiaddr>`.
