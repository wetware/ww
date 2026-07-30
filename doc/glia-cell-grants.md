# Migrating Glia cells to explicit grants

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

## Find and review grantable interfaces

Use `(capabilities)` or `(capabilities :runtime)` for static JSON documentation.
Listing an interface does not mean the current Glia environment possesses it.
Only an existing capability value can be placed in `:grants`; labels and
interface IDs never resolve authority.

The five parse-tested patterns in [`examples/grants/`](../examples/grants/)
cover zero grants, one status grant, attenuation, an image-bound Executor, and
a deliberate Runtime grant. Review Glia files with:

```sh
cargo run -p grant-lint -- path/to/init.glia
```

See [`capability-catalog.md`](capability-catalog.md) and
[`grant-lint.md`](grant-lint.md) for generation, rule severity, concrete fixes,
and narrow reasoned suppressions.
