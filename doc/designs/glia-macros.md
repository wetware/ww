# Glia Macro System Design

> **Historical design document.** Glia was removed from active mainline. The
> final legacy implementation is preserved by `archive/glia-final`.

## Status: Phase 1 (defmacro + gensym) — shipping with #166

## Overview

Glia's macro system follows Clojure's model: macros are functions from
unevaluated forms to forms, expanded at eval time. This document captures
design decisions and the planned evolution toward full macro hygiene.

## Phase 1: defmacro + gensym (current)

### Mechanism

`(defmacro name [params] body)` stores a `Val::Macro` in the root Env frame.
When the evaluator encounters a macro name in call position:

1. Pass the **unevaluated** argument forms to the macro function
2. Evaluate the macro body (which constructs a new form)
3. Recursively macroexpand + eval the result

```clojure
(defmacro when [test & body]
  (list 'if test (cons 'do body) nil))

(when (> x 0)
  (print "positive")
  (print "done"))

;; expands to:
(if (> x 0) (do (print "positive") (print "done")) nil)
```

### Hygiene: gensym

Macros that introduce local bindings risk **variable capture** — the macro's
internal variable name collides with a name in the caller's scope.

**The problem:**
```clojure
(defmacro and [a b]
  (list 'let ['tmp a]
    (list 'if 'tmp b 'tmp)))

;; If the caller has a variable named `tmp`:
(let [tmp 42]
  (and (> tmp 0) (print tmp)))

;; Expands to:
(let [tmp 42]
  (let [tmp (> tmp 0)]    ;; shadows caller's `tmp`!
    (if tmp (print tmp)    ;; prints `true`, not `42`
      tmp)))
```

**The solution:** `gensym` generates unique symbols that cannot collide:
```clojure
(defmacro and [a b]
  (let [g (gensym)]
    (list 'let [g a]
      (list 'if g b g))))

;; Now expands to:
(let [G__1 (> tmp 0)]
  (if G__1 (print tmp) G__1))   ;; no collision
```

`gensym` uses a global counter producing symbols like `G__1`, `G__2`, etc.
The `G__` prefix is chosen to be unlikely to collide with user symbols.

### Form construction

Without syntax-quote, macros construct forms explicitly:

```clojure
;; Explicit construction (Phase 1):
(defmacro defn [name params & body]
  (list 'def name (cons 'fn (cons params body))))
```

This is verbose but correct. Every Lisp started here.

### What Phase 1 does NOT have

- **Syntax-quote** (`` ` ``): template-style macro bodies
- **Unquote** (`~`): splice evaluated values into templates
- **Splice-unquote** (`~@`): splice sequences into templates
- **Auto-qualification**: symbols in macros are not namespace-qualified
- **Namespaces**: no module system, symbols are flat strings

### Known limitations

1. **No compile-time expansion.** Macros expand at eval time, not at read/analyze
   time. This means macroexpansion happens on every evaluation, not once. Acceptable
   for a shell language; the Analyzer/Expr pipeline (#201) moves expansion to
   analysis time.

2. **No `macroexpand` / `macroexpand-1` introspection.** Users cannot inspect
   what a macro expands to. Add these as built-in functions when debugging macros
   becomes a pain point.

3. **Infinite expansion = stack overflow.** If macro A expands to macro B which
   expands to macro A, the evaluator stack-overflows. Same behavior as Clojure.
   This is the macro author's bug.

## Phase 2: Syntax-quote sugar (#197)

Add reader-level transformations:

| Syntax | Expansion |
|--------|-----------|
| `` `(if ~test ~@body) `` | `(list 'if test (concat body))` (roughly) |
| `~expr` | evaluate expr, splice result into template |
| `~@expr` | evaluate expr (must be seq), splice elements into template |

This makes macro bodies look like their output (template-style):

```clojure
;; Phase 1 (explicit):
(defmacro when [test & body]
  (list 'if test (cons 'do body) nil))

;; Phase 2 (syntax-quote):
(defmacro when [test & body]
  `(if ~test (do ~@body) nil))
```

Same semantics, dramatically better ergonomics.

### Auto-qualification

Clojure's syntax-quote auto-qualifies symbols with their namespace, preventing
a class of capture bugs. Since Glia doesn't have namespaces yet (#198), Phase 2
syntax-quote will NOT auto-qualify. `gensym` remains the hygiene mechanism.

When namespaces arrive, syntax-quote gains auto-qualification for free — this
is purely additive.

## Phase 3: Namespaces (#198) + full hygiene

With namespaces, syntax-quote can auto-qualify symbols:

```clojure
;; In namespace `mylib`:
(defmacro my-and [a b]
  `(let [tmp# ~a]        ;; tmp# = gensym, 'let = mylib/let (auto-qualified)
     (if tmp# ~b tmp#)))
```

The `#` suffix is sugar for gensym (Clojure convention). Auto-qualified symbols
(`mylib/let`) cannot collide with the caller's unqualified symbols.

This is the endgame for macro hygiene. It matches Clojure's production model
(17 years of validation) without the complexity of Scheme's `syntax-rules` /
`syntax-case`.

## Design rationale: why not hygienic macros?

Scheme-style hygienic macros (`syntax-rules`, `syntax-case`) provide safety
by default — variable capture is impossible. Clojure chose unhygienic +
conventions instead. Glia follows Clojure because:

1. **Simplicity.** `defmacro` is one concept: functions on forms. Hygienic
   macros are a separate, complex subsystem with its own pattern language.

2. **Power.** Intentional capture (anaphoric macros) is sometimes desired.
   Hygienic macros make this harder. `defmacro` lets you do anything.

3. **Precedent.** Clojure's approach works in production at scale. The
   convention-based hygiene (gensym + syntax-quote + namespaces) catches
   99% of issues. The remaining 1% is expert-level macro authoring where
   the author understands capture.

4. **Incremental.** Each phase (defmacro → syntax-quote → namespaces) adds
   safety without breaking existing code. No big-bang migration.

## References

- [Clojure Special Forms](https://clojure.org/reference/special_forms)
- [Clojure Macros](https://clojure.org/reference/macros)
- [spy16/slurp](https://github.com/spy16/slurp) — Go LISP toolkit architecture
- Clojure Compiler.java: special forms as hardcoded match in analyzer
