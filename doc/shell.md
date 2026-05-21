# Shell

`ww shell` connects to a running node and opens a Glia REPL.

## Modes

```sh
ww shell
ww shell <multiaddr>
```

- `ww shell`: discover hosts via mDNS, then connect only when target
  selection is unambiguous.
- `ww shell <multiaddr>`: dial an explicit target.

## Auth

Shell transport uses libp2p streams and Terminal(Membrane) challenge-response
authentication. `ww shell` signs terminal challenges with the local identity
key (`WW_IDENTITY` or `~/.ww/identity`).

## Multi-Result Discovery

When mDNS returns multiple candidates and no deterministic preferred target
is found, `ww shell` refuses to guess and asks for an explicit multiaddr.

Interactive multi-select UX is tracked in issue #479.
