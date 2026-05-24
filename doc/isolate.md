# Isolate, Free Variables, and Closure Safety in Glia

This document explains the main `isolate` problem in Glia:

- We want to import useful helpers (functions/macros) into `:env`.
- Those values close over lexical scope.
- Closed-over scope can covertly carry capability authority.

Free-variable analysis is what makes this safe and usable: it narrows closure capture to only what the function/macro actually references, which lets `isolate` accurately decide whether that import is authority-free.

This document also covers closure snapshot slimming, which falls out of the same analysis and reduces capture cost.

It is written for technical readers who may be new to Lisp and new to free-variable analysis.

## The problem `isolate` solves

`isolate` creates a fresh execution context with only explicitly imported bindings.

```clojure
(isolate {:env {:directory directory-ro
                :add       add}}
  ...)
```

The goal is strict authority control with ergonomic imports:
- Capability values import as capabilities.
- Non-capability values must be authority-free.

Before this work, closures made the boundary fuzzy: a value that *looked* like data could carry hidden authority through captured environment bindings.

Example risk:
```clojure
(def db ...capability...)
(def helper (fn [k] (perform db :read k)))

(isolate {:env {:helper helper}}
  (helper "x"))
```

If `helper` carries `db` in its closure environment, `isolate` can be bypassed. The runtime now prevents this.

This is the key motivation for free-variable analysis in this area:
- We need helper functions/macros to be usable in `:env`.
- We need their closure capture to be explicit and inspectable.
- We must reject helpers that still carry capability authority.

## Lisp background: evaluation and lexical scope

In Lisp-like languages, code is data (lists), and function literals (`fn`) close over lexical scope.

```clojure
(let [x 41]
  (fn [y] (+ x y)))
```

The returned function remembers `x`. That remembered state is the closure environment.

Two concepts matter here:
- **Evaluation**: run expressions to values.
- **Analysis**: inspect expression structure *before* evaluation.

Glia uses analysis to determine what names a function body actually needs.

## Free variables, in plain terms

A **free variable** of an expression is a name used in the expression but not defined inside it.

Examples:
- Expression: `(+ x y)`
  - Free vars: `+`, `x`, `y`
- Expression: `(fn [x] (+ x y))`
  - Free vars: `+`, `y` (`x` is bound by params)

Why this matters: if we know free vars, we can capture only those bindings when constructing a closure.

## What Glia analyzes

For analyzed `Expr::Fn` forms, each arity stores a computed `free_vars` set.
At function construction time, Glia unions those sets and captures only those names from the current environment.

That gives two properties we need for `isolate`:
- Closure capture is proportional to what the function actually references, not all visible bindings.
- Authority checks can operate on a tight captured env, so closures/macros that are actually authority-free are admitted instead of being over-rejected.

## Sequential `let`/`loop` semantics

Glia's `let`/`loop` bindings are sequential during analysis, matching runtime behavior.

- `(let [x x] body)` keeps outer `x` as free in the initializer.
- `(let [x 1 y x] body)` treats `x` as bound when analyzing `y`'s initializer.

This detail is critical for correctness; wrong ordering would misclassify free vars and capture the wrong environment.

## Closure snapshot slimming

At analyzed `fn` construction, Glia now:
1. Computes union of free vars across arities.
2. Filters the current env to only those names.
3. Stores that filtered env in the closure.

Effects:
- Lower capture cost (no full environment clone for analyzed `fn`).
- Smaller closure state.
- Better security signal for `isolate` checks, because captured authority is explicit and narrow.

## Why raw `fn`/`defmacro` still capture full snapshots

There are raw evaluation paths that do not go through analyzed `FnArityExpr` free-var metadata.
Those paths intentionally keep full snapshots because they do not have the analysis information needed for safe slimming.

This is an intentional asymmetry in the current design.

## Authority checks at `:env` import time

When importing into `isolate`, Glia now enforces:
- Plain data is allowed.
- Capability values are imported as capabilities.
- Closures/macros are allowed only if they are authority-free.

Function and macro values cache:
- `is_cap_free`
- `cap_violation` (first offending captured binding name)

So errors are concrete, for example:
- `function carries capability authority via captured 'db'`
- `macro carries capability authority via captured 'db'`

## Handler stack security note

Function and macro invocation now use the **caller's** effect-handler stack, not the definition-time stack.

This blocks handler smuggling patterns where an outer handler could be reached indirectly from code executed inside `isolate`.

## Practical guidance

Use this mental model:
- `isolate` boundary = exactly what `:env` imports, plus whatever those imports can reach.
- If a value is a capability, it is imported as capability authority.
- If a value is not a capability, it must be authority-free.
- If you pass closures/macros, expect the runtime to inspect captured authority.

If an import fails, inspect the reported captured binding name first; that is usually the shortest path to a safe refactor.

## Related docs

- [capabilities.md](capabilities.md)
- [shell.md](shell.md)
- [architecture.md](architecture.md)
