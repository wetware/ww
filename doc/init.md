# Init and Export Policy

`/etc/init.glia` is required at boot.

- Boot is fail-closed.
- Any parse/eval/policy error aborts boot.
- Legacy `{:export {:caps ... :methods ...}}` is rejected.

## Return Contract

`init.glia` must return a **bare export map**:

```clojure
{:host host
 :runtime runtime}
```

Keys are exported cap names. Values are capability values, including attenuated caps.

Export nothing:

```clojure
{}
```

## Orchestration

`/lib/init/default.glia` is orchestration-only (`init.d` discovery/eval). Policy stays image-local:

```clojure
(load-file "/lib/init/default.glia")
{:host host}
```

## Recursive Attenuation

Use existing `attenuate` syntax in both shell and init scripts.

Vector form:

```clojure
(attenuate host [:id :network])
```

Keyword form with recursive returns:

```clojure
(attenuate host
  :allow [:id :network]
  :returns {:network
            {:stream-dialer (attenuate :self :allow [:dial])
             :vat-client    (attenuate :self :allow [:dial])}})
```

Notes:

- `:self` is only valid inside `:returns`.
- Policy validation is strict: unknown cap names, methods, and return fields fail boot.
- Enforcement is at kernel/RPC proxy boundaries (including returned sub-caps), not evaluator-local only.
