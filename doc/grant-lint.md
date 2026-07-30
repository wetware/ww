# Explicit-grant review lint

`grant-lint` reviews high-signal Glia child-authority patterns. It is not a
security boundary: runtime confinement comes from the child’s immutable initial
authority record, regardless of whether lint ran or was suppressed.

```sh
cargo run -p grant-lint -- std examples
cargo run -p grant-lint -- --json path/to/init.glia
cargo run -p grant-lint -- --deny-warnings path/to/reviewed/files
```

Warnings and advisory hints are non-blocking by default. Structural errors
return a failing status. `--deny-warnings` is available for a reviewed,
warning-free scope such as the repository’s canonical examples.

## Rules

| Rule | Severity | Pattern and rewrite |
|------|----------|---------------------|
| `GLIA001` | error | Glia reader or structural grant-map failure, including duplicate literal grant names. Fix the source; it cannot be suppressed. |
| `WWG101` | warning | Sensitive `host`, `runtime`, `identity`, `authority`, `routing`, `ipfs`, `http-client`, or `vat-listener` grant lacks a local justification. Prefer the catalog’s narrower reference or attenuation, then document any deliberate remainder. |
| `WWG102` | warning | Credential-like bearer strings appear in `:args` or `:env` near `Executor :spawn`. Grant a scoped operation/secret-provider capability instead. |
| `WWG103` | warning | A no-`:grants` cell appears under `with`, resembling the removed lexical-capture idiom. Add an explicit map or move a deliberate zero-grant spawn out of the misleading scope. |
| `WWG104` | warning | Broad `host` is granted while an attenuated Host binding is visibly constructed in the same file. Grant the attenuated binding under `:host`. |
| `WWG201` | advisory | An inline computed grant expression prevents local review. Give the reviewed bundle a descriptive name and pass that symbol to `:grants`. |

The tool intentionally does not attempt broad dataflow analysis, infer whether
an arbitrary guest implementation needs an undocumented capability, or treat
all URLs and paths as authority. Those lower-confidence questions remain review
hints for humans and coding agents.

## Suppression

Suppress only one diagnostic at one nearby site, with a non-empty reason:

```clojure
;; grant-lint: allow WWG101 runtime -- trusted compiler must load submitted WASM
(cell compiler-image :grants {:runtime runtime})

;; grant-lint: allow WWG201 -- helper is a reviewed pure grant-bundle constructor
(cell image :grants (make-reviewed-grants profile))
```

For a grant-specific rule, the marker must include the grant name. Markers apply
only within the three lines immediately above the diagnostic. There is no
file-wide disable or reason-free suppression.

Every diagnostic reports what it found, why the pattern is risky or unclear, a
concrete explicit-capability rewrite, and the exact suppression form.
