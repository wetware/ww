//! Expr — analyzed expression tree for Glia.
//!
//! The pipeline is: `String → Token → Val (parsed) → Expr (analyzed) → Val (result)`.
//! The analyzer (`analyze`) walks a parsed `Val` and produces an `Expr` tree that
//! makes the structure of special forms explicit.  `eval_expr` (in `eval.rs`) then
//! evaluates the `Expr` tree in an environment.
//!
//! Aligns with Clojure's compilation model: fn bodies are analyzed once at
//! definition time (stored as `FnBody::Analyzed`), not re-analyzed on every call.

use crate::Val;
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// An analyzed expression — the IR between parsed Val and evaluated result.
#[derive(Debug, Clone)]
pub enum Expr {
    /// A constant value (Int, Float, Str, Bool, Nil, Keyword, Bytes, etc.).
    Const(Val),

    /// Symbol lookup — resolved at eval time from Env.
    Sym(String),

    /// `(def name value?)`
    Def { name: String, value: Box<Expr> },

    /// `(if test then else?)`
    If {
        test: Box<Expr>,
        then: Box<Expr>,
        else_: Box<Expr>,
    },

    /// `(do exprs...)`
    Do { body: Vec<Expr> },

    /// `(let [bindings...] body...)` — supports destructuring patterns.
    Let {
        bindings: Vec<(crate::pattern::LetBinding, Expr)>,
        body: Vec<Expr>,
    },

    /// `(quote val)` — stores the raw Val, returned as-is at eval time.
    Quote(Val),

    /// `(fn [params] body...)` or `(fn (arity1) (arity2) ...)`
    /// Bodies are pre-analyzed.  Captures env at eval time.
    Fn { arities: Vec<FnArityExpr> },

    /// `(loop [bindings...] body...)` — supports destructuring patterns.
    Loop {
        bindings: Vec<(crate::pattern::LetBinding, Expr)>,
        body: Vec<Expr>,
    },

    /// `(recur args...)`
    Recur { args: Vec<Expr> },

    /// `(defmacro name [params] body...)` — macro bodies stay as raw Val.
    DefMacro { name: String, raw_args: Vec<Val> },

    /// `(perform target args...)` — signal an effect.
    ///
    /// Two forms:
    /// - `(perform :keyword data)` — keyword effect (environmental/global)
    /// - `(perform cap :method args...)` — cap-targeted effect (object-scoped)
    Perform { target: Box<Expr>, args: Vec<Expr> },

    /// `(perform* target payload)` — apply-style perform: `payload` evaluates
    /// to a list/vector whose elements become the perform args. This is what
    /// lets a generic interposition handler delegate its `(method args...)`
    /// payload onward without knowing the arity: `(perform* cap data)`.
    PerformStar {
        target: Box<Expr>,
        payload: Box<Expr>,
    },

    /// `(with-effect-handler target handler body...)` — install a single effect handler.
    ///
    /// Two forms:
    /// - `(with-effect-handler :keyword handler-fn body...)` — keyword handler
    /// - `(with-effect-handler cap handler-fn body...)` — cap handler
    ///
    /// Multiple keyword handlers use inline kwargs (Clojure-style):
    /// `(with-effect-handler :k1 fn1 :k2 fn2 body...)` — nests into single handlers.
    WithEffectHandler {
        target: Box<Expr>,
        handler: Box<Expr>,
        body: Vec<Expr>,
    },

    /// `(match expr pattern1 body1 pattern2 body2 ...)` — pattern matching.
    /// Value clauses only — effect clauses are compiled away by the analyzer
    /// into a wrapping `WithEffectHandler`.
    Match {
        expr: Box<Expr>,
        clauses: Vec<(crate::pattern::Pattern, Expr)>,
    },

    /// A function/builtin/dispatch call — unified.
    /// `raw_args` preserved for potential macro expansion (macros need unevaluated Val args).
    Call {
        head: String,
        args: Vec<Expr>,
        raw_args: Vec<Val>,
    },

    /// `(apply fn args...)`
    Apply { args: Vec<Expr> },

    /// `[exprs...]` — vector literal with analyzed elements.
    Vector(Vec<Expr>),

    /// `{k1 v1 k2 v2 ...}` — map literal with analyzed keys and values.
    Map(Vec<(Expr, Expr)>),

    /// `#{exprs...}` — set literal with analyzed elements.
    Set(Vec<Expr>),
}

impl Expr {
    /// Return the set of free lexical variables referenced by this expression.
    pub fn free_vars(&self) -> BTreeSet<String> {
        fn union_exprs(exprs: &[Expr]) -> BTreeSet<String> {
            let mut out = BTreeSet::new();
            for expr in exprs {
                out.extend(expr.free_vars());
            }
            out
        }

        fn free_vars_in_raw_val(value: &Val) -> BTreeSet<String> {
            match value {
                Val::Sym(name) => BTreeSet::from([name.clone()]),
                // Runtime-only value; carries no syntax.
                Val::Atom(_) => BTreeSet::new(),
                Val::List(items) | Val::Vector(items) | Val::Set(items) => {
                    let mut out = BTreeSet::new();
                    for item in items {
                        out.extend(free_vars_in_raw_val(item));
                    }
                    out
                }
                Val::Map(map) => {
                    let mut out = BTreeSet::new();
                    for (key, value) in map.iter() {
                        out.extend(free_vars_in_raw_val(key));
                        out.extend(free_vars_in_raw_val(value));
                    }
                    out
                }
                Val::Nil
                | Val::Bool(_)
                | Val::Int(_)
                | Val::Float(_)
                | Val::Str(_)
                | Val::Keyword(_)
                | Val::Bytes(_)
                | Val::Fn { .. }
                | Val::Recur(_)
                | Val::Macro { .. }
                | Val::Effect { .. }
                | Val::NativeFn { .. }
                | Val::AsyncNativeFn { .. }
                | Val::Resume(_)
                | Val::Cap { .. }
                | Val::Cell { .. } => BTreeSet::new(),
            }
        }

        fn bound_names(binding: &crate::pattern::LetBinding) -> BTreeSet<String> {
            match binding {
                crate::pattern::LetBinding::Simple(name) => BTreeSet::from([name.clone()]),
                crate::pattern::LetBinding::Destructure(pattern) => pattern.bound_names(),
            }
        }

        match self {
            Expr::Const(_) | Expr::Quote(_) => BTreeSet::new(),
            Expr::Sym(name) => BTreeSet::from([name.clone()]),
            Expr::Def { name: _, value } => value.free_vars(),
            Expr::If { test, then, else_ } => {
                let mut out = test.free_vars();
                out.extend(then.free_vars());
                out.extend(else_.free_vars());
                out
            }
            Expr::Do { body } => union_exprs(body),
            Expr::Let { bindings, body } | Expr::Loop { bindings, body } => {
                let mut bound = BTreeSet::new();
                let mut free = BTreeSet::new();

                for (binding, init_expr) in bindings {
                    for name in init_expr.free_vars() {
                        if !bound.contains(&name) {
                            free.insert(name);
                        }
                    }
                    bound.extend(bound_names(binding));
                }

                for expr in body {
                    for name in expr.free_vars() {
                        if !bound.contains(&name) {
                            free.insert(name);
                        }
                    }
                }

                free
            }
            Expr::Fn { arities } => {
                let mut out = BTreeSet::new();
                for arity in arities {
                    out.extend(arity.free_vars.clone());
                }
                out
            }
            Expr::Recur { args } => union_exprs(args),
            Expr::DefMacro { name: _, raw_args } => {
                let mut out = BTreeSet::new();
                for arg in raw_args {
                    out.extend(free_vars_in_raw_val(arg));
                }
                out
            }
            Expr::Perform { target, args } => {
                let mut out = target.free_vars();
                out.extend(union_exprs(args));
                out
            }
            Expr::PerformStar { target, payload } => {
                let mut out = target.free_vars();
                out.extend(payload.free_vars());
                out
            }
            Expr::WithEffectHandler {
                target,
                handler,
                body,
            } => {
                let mut out = target.free_vars();
                out.extend(handler.free_vars());
                out.extend(union_exprs(body));
                out
            }
            Expr::Match { expr, clauses } => {
                let mut out = expr.free_vars();
                for (pattern, body) in clauses {
                    let clause_bound = pattern.bound_names();
                    for name in body.free_vars() {
                        if !clause_bound.contains(&name) {
                            out.insert(name);
                        }
                    }
                }
                out
            }
            Expr::Call { head, args, .. } => {
                let mut out = BTreeSet::from([head.clone()]);
                out.extend(union_exprs(args));
                out
            }
            Expr::Apply { args } => union_exprs(args),
            Expr::Vector(items) | Expr::Set(items) => union_exprs(items),
            Expr::Map(pairs) => {
                let mut out = BTreeSet::new();
                for (key, value) in pairs {
                    out.extend(key.free_vars());
                    out.extend(value.free_vars());
                }
                out
            }
        }
    }
}

/// An analyzed function arity — body forms are pre-analyzed Expr.
#[derive(Debug, Clone)]
pub struct FnArityExpr {
    pub params: Vec<String>,
    pub variadic: Option<String>,
    pub body: Vec<Expr>,
    pub free_vars: BTreeSet<String>,
}

/// Fn body storage: raw Val (macro-produced) or pre-analyzed Expr.
///
/// Aligns with Clojure's "compile once at definition time" semantics.
/// The analyzer produces `Analyzed`; macro-generated fns produce `Raw`.
/// `invoke_fn` dispatches on the variant.
#[derive(Debug, Clone)]
pub enum FnBody {
    /// Macro-produced or reader-constructed bodies (not yet analyzed).
    Raw(Vec<Val>),
    /// Pre-analyzed by the analyzer — no re-analysis on invocation.
    Analyzed(Vec<Expr>),
}

// ---------------------------------------------------------------------------
// Analyzer
// ---------------------------------------------------------------------------

/// Analyze a parsed Val into an Expr tree.
///
/// Pure structural transformation — no env or dispatch needed.
/// Macros are NOT expanded here (they depend on runtime env bindings).
pub fn analyze(val: &Val) -> Result<Expr, String> {
    match val {
        // Self-evaluating constants
        Val::Nil
        | Val::Bool(_)
        | Val::Int(_)
        | Val::Float(_)
        | Val::Str(_)
        | Val::Keyword(_)
        | Val::Bytes(_)
        // Runtime-only value (can appear via macro-produced forms).
        | Val::Atom(_) => Ok(Expr::Const(val.clone())),

        // Symbol — deferred lookup
        Val::Sym(s) => Ok(Expr::Sym(s.clone())),

        // Empty list => nil
        Val::List(items) if items.is_empty() => Ok(Expr::Const(Val::Nil)),

        // Non-empty list => special form or call
        Val::List(items) => analyze_list(items),

        // Collection literals with sub-expression analysis
        Val::Vector(items) => {
            let exprs = items.iter().map(analyze).collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Vector(exprs))
        }
        Val::Map(pairs) => {
            let exprs = pairs
                .iter()
                .map(|(k, v)| -> Result<(Expr, Expr), String> { Ok((analyze(k)?, analyze(v)?)) })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Map(exprs))
        }
        Val::Set(items) => {
            let exprs = items.iter().map(analyze).collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Set(exprs))
        }

        // Runtime-only values (shouldn't appear in source but handle gracefully)
        Val::Fn { .. }
        | Val::Macro { .. }
        | Val::Recur(_)
        | Val::Effect { .. }
        | Val::NativeFn { .. }
        | Val::AsyncNativeFn { .. }
        | Val::Resume(_)
        | Val::Cap { .. }
        | Val::Cell { .. } => Ok(Expr::Const(val.clone())),
    }
}

/// Analyze a non-empty list form.
fn analyze_list(items: &[Val]) -> Result<Expr, String> {
    let head = match &items[0] {
        Val::Sym(s) => s.as_str(),
        // Non-symbol head: analyze as a call expression
        // e.g. ((fn [x] x) 42) — head is a list, not a symbol
        _ => {
            return Err(format!("expected symbol at head of list, got {}", items[0]));
        }
    };
    let raw_args = &items[1..];

    match head {
        "def" => analyze_def(raw_args),
        "if" => analyze_if(raw_args),
        "do" => analyze_do(raw_args),
        "let" => analyze_let(raw_args),
        "quote" => analyze_quote(raw_args),
        "fn" => analyze_fn(raw_args),
        "loop" => analyze_loop(raw_args),
        "recur" => analyze_recur(raw_args),
        "defmacro" => analyze_defmacro(raw_args),
        "perform" => analyze_perform(raw_args),
        "perform*" => analyze_perform_star(raw_args),
        "with-effect-handler" => analyze_with_effect_handler(raw_args),
        "match" => analyze_match(raw_args),
        "unquote" => Err("unquote (~) not inside syntax-quote".into()),
        "splice-unquote" => Err("splice-unquote (~@) not inside syntax-quote".into()),
        "apply" => {
            let args = raw_args
                .iter()
                .map(analyze)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Apply { args })
        }
        _ => {
            let args = raw_args
                .iter()
                .map(analyze)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::Call {
                head: head.to_string(),
                args,
                raw_args: raw_args.to_vec(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Individual analyze helpers
// ---------------------------------------------------------------------------

/// `(def name)` or `(def name value)`
fn analyze_def(args: &[Val]) -> Result<Expr, String> {
    if args.is_empty() || args.len() > 2 {
        return Err("def: expected (def name) or (def name value)".into());
    }
    let name = match &args[0] {
        Val::Sym(s) => s.clone(),
        other => return Err(format!("def: expected symbol for name, got {other}")),
    };
    let value = if args.len() == 2 {
        Box::new(analyze(&args[1])?)
    } else {
        Box::new(Expr::Const(Val::Nil))
    };
    Ok(Expr::Def { name, value })
}

/// `(if test then)` or `(if test then else)`
fn analyze_if(args: &[Val]) -> Result<Expr, String> {
    if args.len() < 2 || args.len() > 3 {
        return Err("if: expected 2-3 args (test then else?)".into());
    }
    let test = Box::new(analyze(&args[0])?);
    let then = Box::new(analyze(&args[1])?);
    let else_ = if args.len() == 3 {
        Box::new(analyze(&args[2])?)
    } else {
        Box::new(Expr::Const(Val::Nil))
    };
    Ok(Expr::If { test, then, else_ })
}

/// `(do body...)`
fn analyze_do(args: &[Val]) -> Result<Expr, String> {
    let body = args.iter().map(analyze).collect::<Result<Vec<_>, _>>()?;
    Ok(Expr::Do { body })
}

/// `(let [name1 val1 name2 val2 ...] body...)`
fn analyze_let(args: &[Val]) -> Result<Expr, String> {
    if args.is_empty() {
        return Err("let: expected bindings vector".into());
    }
    let binding_vec = match &args[0] {
        Val::Vector(v) => v,
        other => return Err(format!("let: expected vector for bindings, got {other}")),
    };
    if binding_vec.len() % 2 != 0 {
        return Err("let: bindings must be pairs".into());
    }
    let mut bindings = Vec::new();
    for pair in binding_vec.chunks(2) {
        let binding = crate::pattern::analyze_binding(&pair[0]).map_err(|e| format!("let: {e}"))?;
        let expr = analyze(&pair[1])?;
        bindings.push((binding, expr));
    }
    let body = args[1..]
        .iter()
        .map(analyze)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Expr::Let { bindings, body })
}

/// `(quote val)`
fn analyze_quote(args: &[Val]) -> Result<Expr, String> {
    if args.len() != 1 {
        return Err(format!("quote: expected 1 arg, got {}", args.len()));
    }
    Ok(Expr::Quote(args[0].clone()))
}

/// `(fn [params] body...)` or `(fn (arity1) (arity2) ...)`
fn analyze_fn(args: &[Val]) -> Result<Expr, String> {
    if args.is_empty() {
        return Err("fn: expected params".into());
    }
    let arities = match &args[0] {
        // Single-arity: (fn [x y] body...)
        Val::Vector(params) => {
            let arity = analyze_fn_arity(params, &args[1..])?;
            vec![arity]
        }
        // Multi-arity: (fn ([x] body1) ([x y] body2) ...)
        Val::List(_) => {
            let mut result = Vec::new();
            for arg in args {
                match arg {
                    Val::List(items) if !items.is_empty() => {
                        let param_vec = match &items[0] {
                            Val::Vector(v) => v,
                            other => {
                                return Err(format!(
                                    "fn: multi-arity clause must start with [params], got {other}"
                                ))
                            }
                        };
                        result.push(analyze_fn_arity(param_vec, &items[1..])?);
                    }
                    other => return Err(format!("fn: expected arity clause (list), got {other}")),
                }
            }
            // Validate no duplicate arities
            let mut seen_counts = std::collections::HashSet::new();
            let mut has_variadic = false;
            for a in &result {
                if a.variadic.is_some() {
                    if has_variadic {
                        return Err("fn: only one variadic arity allowed".into());
                    }
                    has_variadic = true;
                } else if !seen_counts.insert(a.params.len()) {
                    return Err(format!("fn: duplicate arity for {} args", a.params.len()));
                }
            }
            result
        }
        other => {
            return Err(format!(
                "fn: expected [params] or arity clauses, got {other}"
            ))
        }
    };
    Ok(Expr::Fn { arities })
}

/// Parse a single fn arity's params and analyze its body.
fn analyze_fn_arity(param_vec: &[Val], body: &[Val]) -> Result<FnArityExpr, String> {
    let mut params = Vec::new();
    let mut variadic = None;
    let mut i = 0;
    while i < param_vec.len() {
        match &param_vec[i] {
            Val::Sym(s) if s == "&" => {
                i += 1;
                match param_vec.get(i) {
                    Some(Val::Sym(rest_name)) => {
                        if variadic.is_some() {
                            return Err("fn: only one & rest param allowed".into());
                        }
                        variadic = Some(rest_name.clone());
                    }
                    _ => return Err("fn: expected symbol after &".into()),
                }
                if i + 1 < param_vec.len() {
                    return Err("fn: nothing allowed after & rest param".into());
                }
            }
            Val::Sym(s) => params.push(s.clone()),
            other => return Err(format!("fn: parameter must be a symbol, got {other}")),
        }
        i += 1;
    }
    let analyzed_body = body.iter().map(analyze).collect::<Result<Vec<_>, _>>()?;
    let mut free_vars = BTreeSet::new();
    for expr in &analyzed_body {
        free_vars.extend(expr.free_vars());
    }
    for param in &params {
        free_vars.remove(param);
    }
    if let Some(rest_name) = &variadic {
        free_vars.remove(rest_name);
    }
    Ok(FnArityExpr {
        params,
        variadic,
        body: analyzed_body,
        free_vars,
    })
}

/// `(loop [name1 val1 ...] body...)`
fn analyze_loop(args: &[Val]) -> Result<Expr, String> {
    if args.is_empty() {
        return Err("loop: expected bindings vector".into());
    }
    let binding_vec = match &args[0] {
        Val::Vector(v) => v,
        other => return Err(format!("loop: expected vector for bindings, got {other}")),
    };
    if binding_vec.len() % 2 != 0 {
        return Err("loop: bindings must be pairs".into());
    }
    let mut bindings = Vec::new();
    for pair in binding_vec.chunks(2) {
        let binding =
            crate::pattern::analyze_binding(&pair[0]).map_err(|e| format!("loop: {e}"))?;
        let expr = analyze(&pair[1])?;
        bindings.push((binding, expr));
    }
    let body = args[1..]
        .iter()
        .map(analyze)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Expr::Loop { bindings, body })
}

/// `(recur args...)`
fn analyze_recur(args: &[Val]) -> Result<Expr, String> {
    let analyzed = args.iter().map(analyze).collect::<Result<Vec<_>, _>>()?;
    Ok(Expr::Recur { args: analyzed })
}

/// `(defmacro name [params] body...)` — store raw fn-args for eval-time processing.
///
/// The name is extracted and stored separately.  `raw_args` contains only
/// the params and body (`&args[1..]`), NOT the name — avoiding double-storage.
fn analyze_defmacro(args: &[Val]) -> Result<Expr, String> {
    if args.is_empty() {
        return Err("defmacro: expected name".into());
    }
    let name = match &args[0] {
        Val::Sym(s) => s.clone(),
        other => return Err(format!("defmacro: expected symbol for name, got {other}")),
    };
    if args.len() < 2 {
        return Err("defmacro: expected params after name".into());
    }
    Ok(Expr::DefMacro {
        name,
        raw_args: args[1..].to_vec(), // params + body only, no name
    })
}

/// `(perform target args...)`
///
/// Two forms:
/// - `(perform :keyword data)` — 2 args, keyword effect
/// - `(perform cap :method args...)` — 2+ args, cap-targeted effect
fn analyze_perform(args: &[Val]) -> Result<Expr, String> {
    if args.len() < 2 {
        return Err(format!(
            "perform: expected at least 2 args, got {}",
            args.len()
        ));
    }
    let target = Box::new(analyze(&args[0])?);
    let rest = args[1..]
        .iter()
        .map(analyze)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Expr::Perform { target, args: rest })
}

/// `(perform* target payload)` — apply-style perform; `payload` must
/// evaluate to a list or vector of the args `perform` would take.
fn analyze_perform_star(args: &[Val]) -> Result<Expr, String> {
    if args.len() != 2 {
        return Err(format!("perform*: expected 2 args, got {}", args.len()));
    }
    Ok(Expr::PerformStar {
        target: Box::new(analyze(&args[0])?),
        payload: Box::new(analyze(&args[1])?),
    })
}

/// `(with-effect-handler target handler body...)` — unified effect handler.
///
/// Two forms:
/// - Keyword: `(with-effect-handler :k1 fn1 :k2 fn2 body...)` — inline kwargs, nested.
/// - Cap:     `(with-effect-handler cap handler-fn body...)` — single cap target.
fn analyze_with_effect_handler(args: &[Val]) -> Result<Expr, String> {
    if args.len() < 3 {
        return Err("with-effect-handler: need at least target, handler, body".into());
    }

    if let Val::Keyword(_) = &args[0] {
        // Inline kwargs: consume keyword/handler pairs, rest is body.
        let mut pairs = Vec::new();
        let mut i = 0;
        while i + 1 < args.len() {
            if let Val::Keyword(_) = &args[i] {
                pairs.push((&args[i], &args[i + 1]));
                i += 2;
            } else {
                break;
            }
        }
        if pairs.is_empty() || i >= args.len() {
            return Err("with-effect-handler: need at least one handler and a body".into());
        }
        let body: Vec<Expr> = args[i..]
            .iter()
            .map(analyze)
            .collect::<Result<Vec<_>, _>>()?;

        // Nest: innermost pair wraps body, each outer wraps the next.
        let mut result_body = body;
        for (k, v) in pairs.into_iter().rev() {
            result_body = vec![Expr::WithEffectHandler {
                target: Box::new(analyze(k)?),
                handler: Box::new(analyze(v)?),
                body: result_body,
            }];
        }
        Ok(result_body.into_iter().next().unwrap())
    } else {
        // Cap target: (with-effect-handler cap handler body...)
        let target = Box::new(analyze(&args[0])?);
        let handler = Box::new(analyze(&args[1])?);
        let body = args[2..]
            .iter()
            .map(analyze)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Expr::WithEffectHandler {
            target,
            handler,
            body,
        })
    }
}

/// Analyze a `(match expr clause1 clause2 ...)` form.
///
/// Separates value clauses from effect clauses at analysis time.
/// Effect clauses (starting with `(effect ...)`) are compiled into a
/// wrapping `Expr::WithEffectHandler`. The resulting `Expr::Match`
/// contains only value clauses.
fn analyze_match(args: &[Val]) -> Result<Expr, String> {
    if args.is_empty() {
        return Err("match: expected (match expr clauses...)".into());
    }

    let scrutinee = analyze(&args[0])?;
    let clause_args = &args[1..];

    if !clause_args.len().is_multiple_of(2) {
        return Err("match: clauses must be pattern/body pairs (odd number of forms)".into());
    }
    if clause_args.is_empty() {
        return Err("match: at least one clause required".into());
    }

    let mut value_clauses = Vec::new();
    // (effect_type, binding_names, raw_body_val)
    let mut effect_clauses: Vec<(String, Vec<String>, Val)> = Vec::new();

    for chunk in clause_args.chunks(2) {
        let pattern_val = &chunk[0];
        let body_val = &chunk[1];

        // Check if this is an effect clause: (effect :type binding...)
        if let Val::List(items) = pattern_val {
            if let Some(Val::Sym(s)) = items.first() {
                if s == "effect" {
                    if items.len() < 3 {
                        return Err(
                            "match: effect clause needs at least (effect :type data)".into()
                        );
                    }
                    let effect_type = match &items[1] {
                        Val::Keyword(k) => k.clone(),
                        other => {
                            return Err(format!(
                                "match: effect type must be a keyword, got {other}"
                            ))
                        }
                    };
                    let mut bindings = Vec::new();
                    for item in &items[2..] {
                        match item {
                            Val::Sym(name) => bindings.push(name.clone()),
                            other => {
                                return Err(format!(
                                    "match: effect clause binding must be a symbol, got {other}"
                                ))
                            }
                        }
                    }
                    if bindings.is_empty() || bindings.len() > 2 {
                        return Err(
                            "match: effect clause expects 1-2 bindings (data [resume])".into()
                        );
                    }
                    effect_clauses.push((effect_type, bindings, body_val.clone()));
                    continue;
                }
            }
        }

        // Value clause — analyze pattern and body
        let pattern = crate::pattern::analyze_pattern(pattern_val)?;
        let body = analyze(body_val)?;
        value_clauses.push((pattern, body));
    }

    let match_expr = Expr::Match {
        expr: Box::new(scrutinee),
        clauses: value_clauses,
    };

    if effect_clauses.is_empty() {
        // Pure match — no handler installation
        Ok(match_expr)
    } else {
        // Compile effect clauses into nested WithEffectHandler wrappers.
        // Each effect clause becomes a single-target handler wrapping the match.
        let mut result: Expr = match_expr;
        for (effect_type, bindings, body_val) in effect_clauses.into_iter().rev() {
            let params: Vec<Val> = bindings.iter().map(|b| Val::Sym(b.clone())).collect();
            let handler_fn_val =
                Val::List(vec![Val::Sym("fn".into()), Val::Vector(params), body_val]);
            result = Expr::WithEffectHandler {
                target: Box::new(analyze(&Val::Keyword(effect_type))?),
                handler: Box::new(analyze(&handler_fn_val)?),
                body: vec![result],
            };
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read;
    use std::collections::BTreeSet;

    fn analyze_str(input: &str) -> Result<Expr, String> {
        let val = read(input).map_err(|e| format!("parse: {e}"))?;
        analyze(&val)
    }

    fn free_vars_str(input: &str) -> BTreeSet<String> {
        analyze_str(input).unwrap().free_vars()
    }

    fn fv(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn analyze_int() {
        match analyze_str("42").unwrap() {
            Expr::Const(Val::Int(42)) => {}
            other => panic!("expected Const(Int(42)), got {other:?}"),
        }
    }

    #[test]
    fn analyze_symbol() {
        match analyze_str("foo").unwrap() {
            Expr::Sym(s) if s == "foo" => {}
            other => panic!("expected Sym(foo), got {other:?}"),
        }
    }

    #[test]
    fn analyze_empty_list() {
        match analyze_str("()").unwrap() {
            Expr::Const(Val::Nil) => {}
            other => panic!("expected Const(Nil), got {other:?}"),
        }
    }

    #[test]
    fn analyze_def_with_value() {
        match analyze_str("(def x 42)").unwrap() {
            Expr::Def { name, value } => {
                assert_eq!(name, "x");
                assert!(matches!(*value, Expr::Const(Val::Int(42))));
            }
            other => panic!("expected Def, got {other:?}"),
        }
    }

    #[test]
    fn analyze_def_no_value() {
        match analyze_str("(def x)").unwrap() {
            Expr::Def { name, value } => {
                assert_eq!(name, "x");
                assert!(matches!(*value, Expr::Const(Val::Nil)));
            }
            other => panic!("expected Def, got {other:?}"),
        }
    }

    #[test]
    fn analyze_if_with_else() {
        match analyze_str("(if true 1 2)").unwrap() {
            Expr::If { test, then, else_ } => {
                assert!(matches!(*test, Expr::Const(Val::Bool(true))));
                assert!(matches!(*then, Expr::Const(Val::Int(1))));
                assert!(matches!(*else_, Expr::Const(Val::Int(2))));
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn analyze_if_no_else() {
        match analyze_str("(if true 1)").unwrap() {
            Expr::If { else_, .. } => {
                assert!(matches!(*else_, Expr::Const(Val::Nil)));
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    #[test]
    fn analyze_do() {
        match analyze_str("(do 1 2 3)").unwrap() {
            Expr::Do { body } => assert_eq!(body.len(), 3),
            other => panic!("expected Do, got {other:?}"),
        }
    }

    #[test]
    fn analyze_let() {
        match analyze_str("(let [x 1 y 2] (+ x y))").unwrap() {
            Expr::Let { bindings, body } => {
                assert_eq!(bindings.len(), 2);
                assert!(
                    matches!(&bindings[0].0, crate::pattern::LetBinding::Simple(s) if s == "x")
                );
                assert!(
                    matches!(&bindings[1].0, crate::pattern::LetBinding::Simple(s) if s == "y")
                );
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    #[test]
    fn analyze_quote() {
        match analyze_str("(quote (a b c))").unwrap() {
            Expr::Quote(Val::List(items)) => assert_eq!(items.len(), 3),
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn analyze_fn_single_arity() {
        match analyze_str("(fn [x y] (+ x y))").unwrap() {
            Expr::Fn { arities } => {
                assert_eq!(arities.len(), 1);
                assert_eq!(arities[0].params, vec!["x", "y"]);
                assert!(arities[0].variadic.is_none());
                assert_eq!(arities[0].body.len(), 1);
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn analyze_fn_variadic() {
        match analyze_str("(fn [x & rest] x)").unwrap() {
            Expr::Fn { arities } => {
                assert_eq!(arities[0].params, vec!["x"]);
                assert_eq!(arities[0].variadic, Some("rest".into()));
            }
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn analyze_loop() {
        match analyze_str("(loop [i 0] (recur (+ i 1)))").unwrap() {
            Expr::Loop { bindings, body } => {
                assert_eq!(bindings.len(), 1);
                assert!(
                    matches!(&bindings[0].0, crate::pattern::LetBinding::Simple(s) if s == "i")
                );
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected Loop, got {other:?}"),
        }
    }

    #[test]
    fn analyze_recur() {
        match analyze_str("(recur 1 2)").unwrap() {
            Expr::Recur { args } => assert_eq!(args.len(), 2),
            other => panic!("expected Recur, got {other:?}"),
        }
    }

    #[test]
    fn analyze_call() {
        match analyze_str("(+ 1 2)").unwrap() {
            Expr::Call {
                head,
                args,
                raw_args,
            } => {
                assert_eq!(head, "+");
                assert_eq!(args.len(), 2);
                assert_eq!(raw_args.len(), 2);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn analyze_defmacro() {
        match analyze_str("(defmacro m [x] x)").unwrap() {
            Expr::DefMacro { name, raw_args } => {
                assert_eq!(name, "m");
                assert_eq!(raw_args.len(), 2); // [x] and x (name stored separately)
            }
            other => panic!("expected DefMacro, got {other:?}"),
        }
    }

    #[test]
    fn analyze_vector_literal() {
        match analyze_str("[1 (+ 2 3)]").unwrap() {
            Expr::Vector(exprs) => {
                assert_eq!(exprs.len(), 2);
                assert!(matches!(exprs[0], Expr::Const(Val::Int(1))));
                assert!(matches!(exprs[1], Expr::Call { .. }));
            }
            other => panic!("expected Vector, got {other:?}"),
        }
    }

    #[test]
    fn analyze_apply() {
        match analyze_str("(apply + (list 1 2))").unwrap() {
            Expr::Apply { args } => {
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn analyze_unquote_errors() {
        assert!(analyze_str("(unquote x)").is_err());
        assert!(analyze_str("(splice-unquote x)").is_err());
    }

    #[test]
    fn analyze_nested() {
        // (if (> x 0) (do (def y x) y) nil)
        match analyze_str("(if (> x 0) (do (def y x) y) nil)").unwrap() {
            Expr::If { test, then, else_ } => {
                assert!(matches!(*test, Expr::Call { .. }));
                assert!(matches!(*then, Expr::Do { .. }));
                assert!(matches!(*else_, Expr::Const(Val::Nil)));
            }
            other => panic!("expected If, got {other:?}"),
        }
    }

    // --- perform / with-handler analyzer tests ---

    #[test]
    fn analyze_perform_basic() {
        match analyze_str("(perform :test 42)").unwrap() {
            Expr::Perform { .. } => {}
            other => panic!("expected Perform, got {other:?}"),
        }
    }

    #[test]
    fn analyze_perform_no_args() {
        assert!(analyze_str("(perform)").is_err());
    }

    #[test]
    fn analyze_perform_one_arg() {
        assert!(analyze_str("(perform :test)").is_err());
    }

    #[test]
    fn analyze_perform_variadic() {
        // Cap-targeted: (perform cap :method arg1 arg2)
        match analyze_str("(perform cap :method :a :b)").unwrap() {
            Expr::Perform { args, .. } => assert_eq!(args.len(), 3),
            other => panic!("expected Perform, got {other:?}"),
        }
    }

    #[test]
    fn analyze_with_effect_handler_keyword() {
        match analyze_str("(with-effect-handler :fail handler body)").unwrap() {
            Expr::WithEffectHandler { target, .. } => {
                assert!(matches!(*target, Expr::Const(Val::Keyword(ref k)) if k == "fail"));
            }
            other => panic!("expected WithEffectHandler, got {other:?}"),
        }
    }

    #[test]
    fn analyze_with_effect_handler_no_args() {
        assert!(analyze_str("(with-effect-handler)").is_err());
        assert!(analyze_str("(with-effect-handler :fail)").is_err());
        assert!(analyze_str("(with-effect-handler :fail handler)").is_err());
    }

    #[test]
    fn analyze_with_effect_handler_multi_kwargs() {
        // Two keyword handlers should nest into two WithEffectHandler nodes.
        match analyze_str("(with-effect-handler :a f1 :b f2 body)").unwrap() {
            Expr::WithEffectHandler { target, body, .. } => {
                assert!(matches!(*target, Expr::Const(Val::Keyword(ref k)) if k == "a"));
                assert_eq!(body.len(), 1);
                assert!(matches!(&body[0], Expr::WithEffectHandler { .. }));
            }
            other => panic!("expected nested WithEffectHandler, got {other:?}"),
        }
    }

    #[test]
    fn analyze_with_effect_handler_cap_target() {
        // Cap target: (with-effect-handler cap handler body)
        match analyze_str("(with-effect-handler my-cap handler body)").unwrap() {
            Expr::WithEffectHandler {
                target,
                handler,
                body,
            } => {
                assert!(matches!(*target, Expr::Sym(ref s) if s == "my-cap"));
                assert!(matches!(*handler, Expr::Sym(ref s) if s == "handler"));
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected WithEffectHandler, got {other:?}"),
        }
    }

    #[test]
    fn analyze_with_effect_handler_multi_body() {
        match analyze_str("(with-effect-handler :fail handler a b c)").unwrap() {
            Expr::WithEffectHandler { body, .. } => {
                assert_eq!(body.len(), 3);
            }
            other => panic!("expected WithEffectHandler, got {other:?}"),
        }
    }

    // --- free_vars tests ---

    #[test]
    fn free_vars_quote_is_empty() {
        assert_eq!(free_vars_str("(quote x)"), BTreeSet::new());
    }

    #[test]
    fn free_vars_symbol_is_self() {
        assert_eq!(free_vars_str("x"), fv(&["x"]));
    }

    #[test]
    fn free_vars_def_ignores_name_binding() {
        assert_eq!(free_vars_str("(def x y)"), fv(&["y"]));
    }

    #[test]
    fn free_vars_let_is_sequential_shadowing() {
        assert_eq!(free_vars_str("(let [x x] x)"), fv(&["x"]));
    }

    #[test]
    fn free_vars_let_is_sequential_prior_binding_visible() {
        assert_eq!(free_vars_str("(let [x 1 y x] y)"), BTreeSet::new());
    }

    #[test]
    fn free_vars_loop_is_sequential() {
        assert_eq!(free_vars_str("(loop [x 1 y x] y)"), BTreeSet::new());
    }

    #[test]
    fn free_vars_nested_fn_shadowing() {
        assert_eq!(free_vars_str("(fn [x] (fn [x] x))"), BTreeSet::new());
    }

    #[test]
    fn free_vars_fn_multi_arity_union() {
        assert_eq!(free_vars_str("(fn ([x] y) ([x y] z))"), fv(&["y", "z"]));
    }

    #[test]
    fn free_vars_fn_variadic_subtracts_rest() {
        assert_eq!(free_vars_str("(fn [a & rest] (foo a rest))"), fv(&["foo"]));
    }

    #[test]
    fn free_vars_match_clause_pattern_shadowing() {
        assert_eq!(free_vars_str("(match v [x] x _ y)"), fv(&["v", "y"]));
    }

    #[test]
    fn free_vars_let_destructure_binds_pattern_names() {
        assert_eq!(free_vars_str("(let [[a b] xs] a)"), fv(&["xs"]));
    }

    #[test]
    fn free_vars_call_includes_head_lookup() {
        assert_eq!(free_vars_str("(f x y)"), fv(&["f", "x", "y"]));
    }

    #[test]
    fn free_vars_apply_unions_args() {
        assert_eq!(free_vars_str("(apply f xs ys)"), fv(&["f", "xs", "ys"]));
    }

    #[test]
    fn free_vars_perform_unions_target_and_args() {
        assert_eq!(
            free_vars_str("(perform target x y)"),
            fv(&["target", "x", "y"])
        );
    }

    #[test]
    fn free_vars_with_effect_handler_unions_parts() {
        assert_eq!(
            free_vars_str("(with-effect-handler target handler body)"),
            fv(&["target", "handler", "body"])
        );
    }

    #[test]
    fn free_vars_defmacro_name_not_treated_as_lexically_bound() {
        let vars = free_vars_str("(defmacro m [x] m)");
        assert!(vars.contains("m"));
    }
}
