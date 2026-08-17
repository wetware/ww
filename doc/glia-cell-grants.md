# Migrating Glia cells to explicit grants

> **Historical migration guide.** Glia was removed from active mainline. The
> final legacy implementation is preserved by `archive/glia-final`.

`cell` no longer captures capability-valued lexical bindings. A child now
receives exactly the references named in `:grants`; omitting `:grants`
intentionally means zero application authority.

Before:

```clojure
(with [status-host restricted-host]
  (cell status-image))
```

After:

```clojure
(with [status-host restricted-host]
  (cell status-image
    :grants {:host status-host}))
```

Grant-map keys are keywords. They become the child-visible string names, so a
key can rename a reference. Values must be exportable capabilities and may be
attenuated before insertion:

```clojure
(def status-host (attenuate host [:id :addrs :peers]))
(def base-grants {:host status-host})
(def status-grants (assoc base-grants :metrics metrics))

(cell status-image :grants status-grants)
```

Use `(cell image)` or `(cell image :grants {})` for a substrate-only child.
`with` remains useful as ordinary lexical composition, but it does not transfer
authority. During migration, Glia warns when a no-`:grants` cell is evaluated
inside a local scope containing capability-valued bindings; top-level
zero-grant cells do not warn.

Direct Glia `Executor` calls use their existing wire-shaped option:

```clojure
(perform executor :spawn :caps {})
```

Glia-native capabilities created by `defcap` cannot yet cross a cell boundary.
Granting one fails before spawn; use a Cap’n Proto-backed capability until the
separate `defcap` export bridge lands.

## Examples

The five parse-checked patterns formerly stored at `examples/grants/` cover
zero grants, one status grant, attenuation, an image-bound Executor, and a
deliberate Runtime grant. The files remain available on `archive/glia-final`.
