# Shell

The `ww shell` transport is currently unavailable.

The previous local admin UDS path has been removed, and the replacement
remote shell transport/auth path has not landed yet. For now, all
invocations of `ww shell` return `NOT IMPLEMENTED`.

## CLI Surface (Forward-Compatible)

```sh
ww shell
ww shell <multiaddr>
ww shell --discover
```

The command shape is intentionally preserved so the remote-shell rollout
can land without another CLI-breaking change.
