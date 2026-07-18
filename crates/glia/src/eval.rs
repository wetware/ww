//! Evaluator for Glia expressions.
//!
//! Resolution order for list forms:
//! 1. Special forms (`def`, `if`, `do`, `let`, `fn`, `quote`, `defmacro`) — unevaluated args
//! 2. Macro expansion — if head resolves to `Val::Macro`, expand with raw args then re-eval
//! 3. Env lookup — if head resolves to `Val::Fn`, invoke the closure
//! 4. Built-in functions (`+`, `list`, `cons`, `apply`, etc.) — eval args, call builtin
//! 5. Generic dispatch — eval args, delegate to [`Dispatch`]
//!
//! Non-list values are self-evaluating (returned as-is), except symbols
//! which are looked up in [`Env`] (unbound symbols pass through).
//!
//! Capability dispatch (host, executor, ipfs, etc.) is provided by the
//! caller via the [`Dispatch`] trait — the evaluator itself is host-agnostic.

use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Poll;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use crate::effect::{self, HandlerStack};
use crate::error;
use crate::expr::FnBody;
use crate::{make_cap, oneshot, AttenuatedCapInner, FnArity, GliaCapInner, Val, ValMap};

/// Monotonic counter for `gensym`.
static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Env — lexical scope chain
// ---------------------------------------------------------------------------

/// A lexical environment: a stack of frames where each frame maps names to values.
///
/// Lookup walks from the innermost (last) frame outward.  `push_frame` /
/// `pop_frame` create and destroy child scopes (used by future `let` / `fn`
/// special forms).
///
/// The `handler_stack` is dynamic scope for the effect system.
/// Closures and macros use the caller's handler stack at invocation time,
/// not the handler stack captured at definition time.
#[derive(Debug, Clone)]
pub struct Env {
    frames: Vec<Frame>,
    handler_stack: HandlerStack,
}

impl Default for Env {
    /// Default creates an Env with one root frame (same as `Env::new()`).
    fn default() -> Self {
        Self::new()
    }
}

type Frame = std::collections::HashMap<String, Val>;

impl Env {
    /// Create a new, empty environment with a single root frame.
    pub fn new() -> Self {
        Self {
            frames: vec![Frame::new()],
            handler_stack: effect::new_handler_stack(),
        }
    }

    /// Look up a binding by name, searching from innermost scope outward.
    pub fn get(&self, name: &str) -> Option<&Val> {
        for frame in self.frames.iter().rev() {
            if let Some(v) = frame.get(name) {
                return Some(v);
            }
        }
        None
    }

    /// Bind `name` to `val` in the innermost (current) frame.
    pub fn set(&mut self, name: String, val: Val) {
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name, val);
        }
    }

    /// Push a new empty child frame (enters a new scope).
    pub fn push_frame(&mut self) {
        self.frames.push(Frame::new());
    }

    /// Pop the innermost frame (exits a scope).  The root frame cannot be popped.
    pub fn pop_frame(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }

    /// Bind `name` to `val` in the root (outermost) frame.
    /// Used by `def` — definitions are always global, like Clojure's `def`.
    pub fn set_root(&mut self, name: String, val: Val) {
        if let Some(frame) = self.frames.first_mut() {
            frame.insert(name, val);
        }
    }

    /// Collect all `Val::Cap` bindings visible in the current scope.
    ///
    /// Searches from innermost scope outward, returning `(name, cap)` pairs.
    /// Inner bindings shadow outer ones (only the innermost binding per name
    /// is returned). Used by `cell` to capture granted capabilities.
    pub fn collect_caps(&self) -> Vec<(String, Val)> {
        let mut seen = std::collections::HashSet::new();
        let mut caps = Vec::new();
        for frame in self.frames.iter().rev() {
            for (name, val) in frame {
                if matches!(val, Val::Cap { .. }) && seen.insert(name.clone()) {
                    caps.push((name.clone(), val.clone()));
                }
            }
        }
        caps
    }

    /// Collapse all frames into a single merged HashMap (inner overrides outer).
    /// Collect all visible bindings (inner overrides outer) as `(name, val)` pairs.
    /// Used by import to extract a module's exported definitions.
    #[must_use]
    pub fn bindings(&self) -> Vec<(String, Val)> {
        let mut merged = Frame::new();
        for frame in &self.frames {
            for (k, v) in frame {
                merged.insert(k.clone(), v.clone());
            }
        }
        let mut bindings: Vec<(String, Val)> = merged.into_iter().collect();
        bindings.sort_by(|(left, _), (right, _)| left.cmp(right));
        bindings
    }

    /// Returns a new Env with one frame containing all visible bindings.
    /// Used by `fn` to capture the definition-time environment.
    pub fn snapshot(&self) -> Self {
        let mut merged = Frame::new();
        for frame in &self.frames {
            for (k, v) in frame {
                merged.insert(k.clone(), v.clone());
            }
        }
        Self {
            frames: vec![merged],
            // Keep the current stack on snapshots; invocation still routes through
            // the caller's handler stack via `Env::for_call`.
            handler_stack: self.handler_stack.clone(),
        }
    }

    /// Return a snapshot filtered to a set of binding names.
    ///
    /// Names not present in this env are ignored.
    pub fn filter_to(&self, names: BTreeSet<&String>) -> Self {
        let mut filtered = Frame::new();
        for name in names {
            if let Some(value) = self.get(name) {
                filtered.insert(name.clone(), value.clone());
            }
        }
        Self {
            frames: vec![filtered],
            handler_stack: self.handler_stack.clone(),
        }
    }

    /// Create a new Env for function invocation.
    ///
    /// Instead of cloning the captured env (which recurses infinitely when
    /// closures capture their own scope), this creates a new Env that COPIES
    /// only the captured snapshot's single frame (no deep clone of Val::Fn envs).
    /// The captured frame's Val::Fn values keep their Rc<Env> references shared.
    pub fn for_call(captured: &Rc<Env>, caller_hs: &HandlerStack) -> Self {
        // The captured env is a snapshot (single frame).
        // Copy its bindings into a new env's root frame.
        // Val::Fn values inside are Rc-wrapped, so cloning them is O(1).
        let mut root = Frame::new();
        for frame in &captured.frames {
            for (k, v) in frame {
                root.insert(k.clone(), v.clone());
            }
        }
        Self {
            frames: vec![root, Frame::new()], // root + param frame
            handler_stack: caller_hs.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch — external command routing
// ---------------------------------------------------------------------------

/// Trait for dispatching evaluated calls to external handlers.
///
/// The kernel (or any host) implements this to route capability calls
/// like `(host id)`, `(ipfs cat ...)`, etc.
pub trait Dispatch {
    /// Invoke the command `name` with already-evaluated `args`.
    ///
    /// Takes `&self` (not `&mut self`) — implementations use interior mutability
    /// for any mutable state. This enables sharing dispatch between body and
    /// handler futures in the effect system's state machine.
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a [Val],
    ) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>>;
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

/// Returns true if `val` is logically truthy (Clojure model).
/// Only `nil` and `false` are falsy — everything else is truthy,
/// including `0`, empty string, and empty collections.
fn is_truthy(val: &Val) -> bool {
    !matches!(val, Val::Nil | Val::Bool(false))
}

fn cap_descriptor_bytes(name: &str, schema_cid: &str, methods: &BTreeSet<String>) -> Vec<u8> {
    let mut method_vec: Vec<&str> = methods.iter().map(String::as_str).collect();
    method_vec.sort_unstable();
    format!(
        "glia.cap.v1\nname={name}\nschema={schema_cid}\nmethods={}\n",
        method_vec.join(",")
    )
    .into_bytes()
}

fn parse_allow_methods(value: &Val) -> Result<BTreeSet<String>, Val> {
    let items = match value {
        Val::Vector(v) | Val::List(v) => v,
        other => {
            return Err(error::type_mismatch(
                "attenuate allow-methods",
                "vector or list",
                other,
            ))
        }
    };

    let mut allow = BTreeSet::new();
    for item in items {
        match item {
            Val::Keyword(k) => {
                allow.insert(k.clone());
            }
            other => return Err(error::type_mismatch("attenuate method", "keyword", other)),
        }
    }
    Ok(allow)
}

fn is_authority_free(value: &Val) -> bool {
    match value {
        Val::Nil
        | Val::Bool(_)
        | Val::Int(_)
        | Val::Float(_)
        | Val::Str(_)
        | Val::Sym(_)
        | Val::Keyword(_)
        | Val::Bytes(_) => true,
        Val::List(items) | Val::Vector(items) | Val::Set(items) => {
            items.iter().all(is_authority_free)
        }
        Val::Map(m) => m
            .iter()
            .all(|(k, v)| is_authority_free(k) && is_authority_free(v)),
        Val::Fn { is_cap_free, .. } | Val::Macro { is_cap_free, .. } => *is_cap_free,
        Val::Cell { .. } | Val::NativeFn { .. } | Val::AsyncNativeFn { .. } | Val::Cap { .. } => {
            false
        }
        Val::Recur(_) | Val::Effect { .. } | Val::Resume(_) => false,
    }
}

fn compute_cap_status(env: &Env) -> (bool, Option<String>) {
    for (name, value) in env.bindings() {
        if !is_authority_free(&value) {
            return (false, Some(name));
        }
    }
    (true, None)
}

/// Evaluate a function/macro body, dispatching on `FnBody` variant.
///
/// `Analyzed` bodies are evaluated via `eval_expr` (no re-analysis).
/// `Raw` bodies are analyzed first, then evaluated (one-time cost for
/// macro-produced closures).
async fn eval_fn_body<'a, D: Dispatch>(
    body: &'a FnBody,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    match body {
        FnBody::Raw(forms) => {
            let mut result = Val::Nil;
            for form in forms {
                result = eval(form, env, dispatch).await?;
            }
            Ok(result)
        }
        FnBody::Analyzed(exprs) => {
            let mut result = Val::Nil;
            for expr in exprs {
                result = eval_expr(expr, env, dispatch).await?;
            }
            Ok(result)
        }
    }
}

/// Evaluate arguments: recursively evaluate nested lists, look up symbols
/// in env (pass through if unbound), and return non-list/non-sym values as-is.
///
/// Used by the generic dispatch path and future fn invocation.
async fn eval_args<'a, D: Dispatch>(
    raw_args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Vec<Val>, Val> {
    let mut args = Vec::with_capacity(raw_args.len());
    for a in raw_args {
        match a {
            Val::List(_) => args.push(eval(a, env, dispatch).await?),
            Val::Sym(s) => match env.get(s) {
                Some(v) => args.push(v.clone()),
                None => args.push(a.clone()),
            },
            other => args.push(other.clone()),
        }
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// Special forms — each receives RAW (unevaluated) args
// ---------------------------------------------------------------------------

/// `(def name value)` — evaluate value, bind name in root frame.
async fn eval_def<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    if args.is_empty() || args.len() > 2 {
        return Err(error::arity("def", "1-2", args.len()));
    }
    let name = match &args[0] {
        Val::Sym(s) => s.clone(),
        other => return Err(error::type_mismatch("def", "symbol", other)),
    };
    let val = match args.get(1) {
        Some(expr) => eval(expr, env, dispatch).await?,
        None => Val::Nil,
    };
    env.set_root(name, val.clone());
    Ok(val)
}

/// `(if test then)` or `(if test then else)` — lazy eval of branches.
async fn eval_if<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    if args.len() < 2 || args.len() > 3 {
        return Err(error::arity("if", "2-3", args.len()));
    }
    let test_val = eval(&args[0], env, dispatch).await?;
    if is_truthy(&test_val) {
        eval(&args[1], env, dispatch).await
    } else if args.len() == 3 {
        eval(&args[2], env, dispatch).await
    } else {
        Ok(Val::Nil)
    }
}

/// `(do forms...)` — evaluate sequentially, return last.
async fn eval_do<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    let mut result = Val::Nil;
    for form in args {
        result = eval(form, env, dispatch).await?;
    }
    Ok(result)
}

/// `(let [bindings...] body...)` — local scope with sequential bindings.
async fn eval_let<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    let bindings = match args.first() {
        Some(Val::Vector(v)) => v,
        Some(other) => return Err(error::type_mismatch("let", "vector of bindings", other)),
        None => return Err(error::arity("let", "at least 1", 0)),
    };
    if bindings.len() % 2 != 0 {
        return Err(error::internal(
            "let",
            "bindings must be pairs (even number of forms)",
        ));
    }

    env.push_frame();

    // Evaluate bindings and body in a block so we always pop the frame,
    // even if an eval error occurs mid-binding or mid-body.
    let result = async {
        for pair in bindings.chunks(2) {
            let name = match &pair[0] {
                Val::Sym(s) => s.clone(),
                other => {
                    return Err(error::type_mismatch("let binding name", "symbol", other));
                }
            };
            let val = eval(&pair[1], env, dispatch).await?;
            env.set(name, val);
        }

        // Body forms (implicit do).
        let body = &args[1..];
        let mut result = Val::Nil;
        for form in body {
            result = eval(form, env, dispatch).await?;
        }
        Ok(result)
    }
    .await;

    env.pop_frame();
    result
}

/// Parse a parameter vector into an FnArity.
/// Handles `[x y]` (fixed) and `[x & rest]` (variadic).
fn parse_params(param_vec: &[Val], body: &[Val]) -> Result<FnArity, Val> {
    let mut params = Vec::new();
    let mut variadic = None;
    let mut i = 0;
    while i < param_vec.len() {
        match &param_vec[i] {
            Val::Sym(s) if s == "&" => {
                // Next symbol is the variadic rest param
                i += 1;
                match param_vec.get(i) {
                    Some(Val::Sym(rest_name)) => {
                        if variadic.is_some() {
                            return Err(error::internal("fn", "only one & rest param allowed"));
                        }
                        variadic = Some(rest_name.clone());
                    }
                    _ => return Err(error::internal("fn", "expected symbol after &")),
                }
                if i + 1 < param_vec.len() {
                    return Err(error::internal("fn", "nothing allowed after & rest param"));
                }
            }
            Val::Sym(s) => params.push(s.clone()),
            other => return Err(error::type_mismatch("fn parameter", "symbol", other)),
        }
        i += 1;
    }
    Ok(FnArity {
        params,
        variadic,
        body: FnBody::Raw(body.to_vec()),
    })
}

/// `(fn [params] body...)` or `(fn ([params] body...) ([params] body...))` — create a closure.
fn eval_fn(args: &[Val], env: &Env) -> Result<Val, Val> {
    if args.is_empty() {
        return Err(error::arity("fn", "at least 1", 0));
    }

    let arities = match &args[0] {
        // Single-arity: (fn [x y] body...)
        Val::Vector(params) => {
            let arity = parse_params(params, &args[1..])?;
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
                                return Err(error::type_mismatch(
                                    "fn multi-arity clause",
                                    "vector of params",
                                    other,
                                ))
                            }
                        };
                        result.push(parse_params(param_vec, &items[1..])?);
                    }
                    other => return Err(error::type_mismatch("fn arity clause", "list", other)),
                }
            }
            // Check for overlapping arities (same fixed param count, ignoring variadic)
            let mut seen_counts = std::collections::HashSet::new();
            let mut has_variadic = false;
            for a in &result {
                if a.variadic.is_some() {
                    if has_variadic {
                        return Err(error::internal("fn", "only one variadic arity allowed"));
                    }
                    has_variadic = true;
                } else if !seen_counts.insert(a.params.len()) {
                    return Err(error::internal(
                        "fn",
                        format!("duplicate arity for {} args", a.params.len()),
                    ));
                }
            }
            result
        }
        other => {
            return Err(error::type_mismatch(
                "fn",
                "[params] or arity clauses",
                other,
            ))
        }
    };

    // Raw fn path: no FnArityExpr, no free-vars data. Keep full snapshot;
    // is_cap_free check below still works correctly because it walks all bindings.
    // Slim closures only apply to the analyzed pipeline (expr::Expr::Fn).
    let captured_env = Rc::new(env.snapshot());
    let (is_cap_free, cap_violation) = compute_cap_status(&captured_env);
    Ok(Val::Fn {
        arities,
        env: captured_env,
        is_cap_free,
        cap_violation,
    })
}

/// Invoke a Val::Fn with evaluated arguments. Matches arity and evaluates body.
async fn invoke_fn<'a, D: Dispatch>(
    arities: &'a [FnArity],
    captured_env: &'a Rc<Env>,
    args: &[Val],
    dispatch: &'a D,
    caller_hs: HandlerStack,
) -> Result<Val, Val> {
    // Find matching arity: prefer exact fixed-arity match over variadic.
    // This ensures (fn ([x y] ...) ([x & rest] ...)) called with 2 args
    // picks the fixed 2-arity, not the variadic.
    let arity = arities
        .iter()
        .find(|a| a.variadic.is_none() && args.len() == a.params.len())
        .or_else(|| {
            arities
                .iter()
                .find(|a| a.variadic.is_some() && args.len() >= a.params.len())
        })
        .ok_or_else(|| {
            let expected: Vec<String> = arities
                .iter()
                .map(|a| {
                    if a.variadic.is_some() {
                        format!("{}+", a.params.len())
                    } else {
                        a.params.len().to_string()
                    }
                })
                .collect();
            error::arity("fn", &expected.join(" or "), args.len())
        })?;

    // Build fn environment: captured env + new frame with param bindings.
    // Uses Env::for_call to avoid infinite recursion from Env::clone when
    // closures capture their own scope.
    let mut fn_env = Env::for_call(captured_env, &caller_hs);

    // Bind positional params
    for (name, val) in arity.params.iter().zip(args.iter()) {
        fn_env.set(name.clone(), val.clone());
    }

    // Bind variadic rest param
    if let Some(rest_name) = &arity.variadic {
        let rest_args: Vec<Val> = args[arity.params.len()..].to_vec();
        fn_env.set(rest_name.clone(), Val::List(rest_args));
    }

    // Number of expected recur args: fixed params + (1 if variadic)
    let recur_arity = arity.params.len() + usize::from(arity.variadic.is_some());

    // Evaluate body (implicit do) with recur support.
    // If the body returns Val::Recur, re-bind params and loop — same
    // semantics as loop/recur but targeting the enclosing fn.
    let result = async {
        loop {
            let result = eval_fn_body(&arity.body, &mut fn_env, dispatch).await?;

            match result {
                Val::Recur(new_vals) => {
                    if new_vals.len() != recur_arity {
                        return Err(error::arity(
                            "recur",
                            &recur_arity.to_string(),
                            new_vals.len(),
                        ));
                    }
                    // Re-bind fixed params
                    for (name, val) in arity.params.iter().zip(new_vals.iter()) {
                        fn_env.set(name.clone(), val.clone());
                    }
                    // Re-bind variadic rest param.
                    // Recur passes fixed_params + 1 args; the last arg IS the
                    // new variadic collection (not individual elements to collect).
                    if let Some(rest_name) = &arity.variadic {
                        let rest_val = new_vals[arity.params.len()].clone();
                        fn_env.set(rest_name.clone(), rest_val);
                    }
                    // continue — re-evaluate body with new bindings
                }
                other => return Ok(other),
            }
        }
    }
    .await;

    fn_env.pop_frame();
    result
}

/// Invoke a closure like [`invoke_fn`] but force the handler stack used inside
/// the function body. Used by defcap method dispatch to preserve the caller's
/// handler stack rather than the definition-time stack.
async fn invoke_fn_with_handler_stack<'a, D: Dispatch>(
    arities: &'a [FnArity],
    captured_env: &'a Rc<Env>,
    args: &[Val],
    dispatch: &'a D,
    handler_stack: HandlerStack,
) -> Result<Val, Val> {
    invoke_fn(arities, captured_env, args, dispatch, handler_stack).await
}

/// Parse macro/fn arity definitions from raw Val args.
///
/// Shared by `eval_defmacro` (old path) and `eval_expr` DefMacro handler.
/// `fn_args` is `[params, body...]` or `[(arity1) (arity2) ...]`.
fn parse_macro_arities(fn_args: &[Val]) -> Result<Vec<FnArity>, Val> {
    if fn_args.is_empty() {
        return Err(error::arity("defmacro", "at least 1", 0));
    }
    match &fn_args[0] {
        // Single-arity: [x y] body...
        Val::Vector(params) => {
            let arity = parse_params(params, &fn_args[1..])?;
            Ok(vec![arity])
        }
        // Multi-arity: ([x] body1) ([x y] body2) ...
        Val::List(_) => {
            let mut result = Vec::new();
            for arg in fn_args {
                match arg {
                    Val::List(items) if !items.is_empty() => {
                        let param_vec = match &items[0] {
                            Val::Vector(v) => v,
                            other => {
                                return Err(error::type_mismatch(
                                    "defmacro multi-arity clause",
                                    "vector of params",
                                    other,
                                ))
                            }
                        };
                        result.push(parse_params(param_vec, &items[1..])?);
                    }
                    other => {
                        return Err(error::type_mismatch("defmacro arity clause", "list", other))
                    }
                }
            }
            let mut seen_counts = std::collections::HashSet::new();
            let mut has_variadic = false;
            for a in &result {
                if a.variadic.is_some() {
                    if has_variadic {
                        return Err(error::internal(
                            "defmacro",
                            "only one variadic arity allowed",
                        ));
                    }
                    has_variadic = true;
                } else if !seen_counts.insert(a.params.len()) {
                    return Err(error::internal(
                        "defmacro",
                        format!("duplicate arity for {} args", a.params.len()),
                    ));
                }
            }
            Ok(result)
        }
        other => Err(error::type_mismatch(
            "defmacro",
            "[params] or arity clauses",
            other,
        )),
    }
}

/// `(defmacro name [params] body...)` — define a macro in the root frame.
///
/// Like `fn` but the resulting `Val::Macro` receives unevaluated args;
/// the body evaluates in the captured env and the result is re-evaluated
/// in the caller's env.
async fn eval_defmacro(args: &[Val], env: &mut Env) -> Result<Val, Val> {
    if args.is_empty() {
        return Err(error::arity("defmacro", "at least 2", 0));
    }
    let name = match &args[0] {
        Val::Sym(s) => s.clone(),
        other => return Err(error::type_mismatch("defmacro name", "symbol", other)),
    };
    let fn_args = &args[1..];
    if fn_args.is_empty() {
        return Err(error::arity("defmacro", "at least 2", 1));
    }
    let arities = parse_macro_arities(fn_args)?;
    // Raw macro path: no FnArityExpr, no free-vars data. Keep full snapshot;
    // is_cap_free check below still works correctly because it walks all bindings.
    // Slim closures only apply to the analyzed pipeline (expr::Expr::Fn).
    let captured_env = Rc::new(env.snapshot());
    let (is_cap_free, cap_violation) = compute_cap_status(&captured_env);
    let val = Val::Macro {
        arities,
        env: captured_env,
        is_cap_free,
        cap_violation,
    };
    env.set_root(name, val.clone());
    Ok(val)
}

/// Invoke a macro: like invoke_fn but receives raw (unevaluated) args.
/// The macro body evaluates in the captured env; the result is a new form
/// that the caller will re-evaluate in their own env.
async fn invoke_macro<'a, D: Dispatch>(
    arities: &'a [FnArity],
    captured_env: &'a Rc<Env>,
    raw_args: &[Val],
    dispatch: &'a D,
    caller_hs: HandlerStack,
) -> Result<Val, Val> {
    // Find matching arity (same logic as invoke_fn)
    let arity = arities
        .iter()
        .find(|a| a.variadic.is_none() && raw_args.len() == a.params.len())
        .or_else(|| {
            arities
                .iter()
                .find(|a| a.variadic.is_some() && raw_args.len() >= a.params.len())
        })
        .ok_or_else(|| {
            let expected: Vec<String> = arities
                .iter()
                .map(|a| {
                    if a.variadic.is_some() {
                        format!("{}+", a.params.len())
                    } else {
                        a.params.len().to_string()
                    }
                })
                .collect();
            error::arity("macro", &expected.join(" or "), raw_args.len())
        })?;

    // Build macro environment: captured env + new frame with raw arg bindings
    let mut macro_env = Env::for_call(captured_env, &caller_hs);

    // Bind positional params to RAW (unevaluated) args
    for (name, val) in arity.params.iter().zip(raw_args.iter()) {
        macro_env.set(name.clone(), val.clone());
    }

    // Bind variadic rest param
    if let Some(rest_name) = &arity.variadic {
        let rest_args: Vec<Val> = raw_args[arity.params.len()..].to_vec();
        macro_env.set(rest_name.clone(), Val::List(rest_args));
    }

    // Evaluate body (implicit do) in the macro's captured env
    let result = async { eval_fn_body(&arity.body, &mut macro_env, dispatch).await }.await;

    macro_env.pop_frame();
    result
}

/// `(loop [bindings...] body...)` — tail-recursive iteration.
///
/// Bindings are sequential (like `let`).  Body forms are evaluated in
/// an implicit `do`.  If the result is `Val::Recur`, the bindings are
/// replaced and the body re-evaluated; otherwise the result is returned.
async fn eval_loop<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    let bindings = match args.first() {
        Some(Val::Vector(v)) => v,
        Some(other) => return Err(error::type_mismatch("loop", "vector of bindings", other)),
        None => return Err(error::arity("loop", "at least 1", 0)),
    };
    if bindings.len() % 2 != 0 {
        return Err(error::internal(
            "loop",
            "bindings must be pairs (even number of forms)",
        ));
    }

    let binding_names: Vec<String> = bindings
        .chunks(2)
        .map(|pair| match &pair[0] {
            Val::Sym(s) => Ok(s.clone()),
            other => Err(error::type_mismatch("loop binding name", "symbol", other)),
        })
        .collect::<Result<Vec<_>, _>>()?;

    let num_bindings = binding_names.len();

    env.push_frame();

    let result = async {
        // Evaluate initial bindings sequentially (each sees previous ones).
        for pair in bindings.chunks(2) {
            let name = match &pair[0] {
                Val::Sym(s) => s.clone(),
                _ => unreachable!(), // already validated above
            };
            let val = eval(&pair[1], env, dispatch).await?;
            env.set(name, val);
        }

        let body = &args[1..];
        loop {
            // Evaluate body forms (implicit do).
            let mut result = Val::Nil;
            for form in body {
                result = eval(form, env, dispatch).await?;
            }

            match result {
                Val::Recur(new_vals) => {
                    if new_vals.len() != num_bindings {
                        return Err(error::arity(
                            "recur",
                            &num_bindings.to_string(),
                            new_vals.len(),
                        ));
                    }
                    for (name, val) in binding_names.iter().zip(new_vals) {
                        env.set(name.clone(), val);
                    }
                    // continue loop — re-evaluate body
                }
                other => return Ok(other),
            }
        }
    }
    .await;

    env.pop_frame();
    result
}

/// `(recur args...)` — evaluate args and return a `Recur` sentinel.
///
/// Only meaningful inside `loop` body (tail position).  If it escapes
/// to the top level, `eval_toplevel` converts it to an error.
async fn eval_recur<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    let evaled = eval_args(args, env, dispatch).await?;
    Ok(Val::Recur(evaled))
}

// ---------------------------------------------------------------------------
// Higher-order built-in functions (need async dispatch for fn invocation)
// ---------------------------------------------------------------------------

/// Dispatch `map`, `filter`, or `reduce` — these invoke user closures.
async fn eval_hof<'a, D: Dispatch>(
    name: &str,
    args: &[Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    match name {
        "map" => {
            if args.len() != 2 {
                return Err(error::arity("map", "2", args.len()));
            }
            let (arities, captured_env) = extract_fn("map", &args[0])?;
            let items = extract_seq("map", &args[1])?;
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                let val = invoke_fn(
                    &arities,
                    &captured_env,
                    std::slice::from_ref(item),
                    dispatch,
                    env.handler_stack.clone(),
                )
                .await?;
                result.push(val);
            }
            Ok(Val::List(result))
        }
        "filter" => {
            if args.len() != 2 {
                return Err(error::arity("filter", "2", args.len()));
            }
            let (arities, captured_env) = extract_fn("filter", &args[0])?;
            let items = extract_seq("filter", &args[1])?;
            let mut result = Vec::new();
            for item in items {
                let val = invoke_fn(
                    &arities,
                    &captured_env,
                    std::slice::from_ref(item),
                    dispatch,
                    env.handler_stack.clone(),
                )
                .await?;
                let keep = !matches!(val, Val::Nil | Val::Bool(false));
                if keep {
                    result.push(item.clone());
                }
            }
            Ok(Val::List(result))
        }
        "reduce" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(error::arity("reduce", "2-3", args.len()));
            }
            let (arities, captured_env) = extract_fn("reduce", &args[0])?;
            let (mut acc, items) = if args.len() == 3 {
                (args[1].clone(), extract_seq("reduce", &args[2])?)
            } else {
                let items = extract_seq("reduce", &args[1])?;
                if items.is_empty() {
                    return Err(error::type_mismatch(
                        "reduce",
                        "non-empty collection (or pass an init value)",
                        &Val::List(vec![]),
                    ));
                }
                (items[0].clone(), &items[1..])
            };
            for item in items {
                acc = invoke_fn(
                    &arities,
                    &captured_env,
                    &[acc, item.clone()],
                    dispatch,
                    env.handler_stack.clone(),
                )
                .await?;
            }
            Ok(acc)
        }
        _ => unreachable!(),
    }
}

/// Extract a `Val::Fn` into its arities and captured env, or error.
fn extract_fn(caller: &str, val: &Val) -> Result<(Vec<FnArity>, Rc<Env>), Val> {
    match val {
        Val::Fn {
            arities,
            env: captured_env,
            ..
        } => Ok((arities.clone(), captured_env.clone())),
        other => Err(error::type_mismatch(caller, "function", other)),
    }
}

/// Extract a sequence (list/vector/nil) into a slice reference.
fn extract_seq<'a>(caller: &str, val: &'a Val) -> Result<&'a [Val], Val> {
    match val {
        Val::Nil => Ok(&[]),
        Val::List(v) | Val::Vector(v) => Ok(v.as_slice()),
        other => Err(error::type_mismatch(caller, "collection", other)),
    }
}

// ---------------------------------------------------------------------------
// Built-in functions
// ---------------------------------------------------------------------------

/// Check whether `name` is a built-in function. If so, run it on the
/// already-evaluated `args` and return `Some(result)`.
/// Returns `None` if `name` is not a built-in — the caller should fall
/// through to host dispatch.
fn eval_builtin(name: &str, args: &[Val]) -> Option<Result<Val, Val>> {
    match name {
        // --- Collections ---
        "list" => Some(Ok(Val::List(args.to_vec()))),
        "cons" => Some(builtin_cons(args)),
        "first" => Some(builtin_first(args)),
        "rest" => Some(builtin_rest(args)),
        "count" => Some(builtin_count(args)),
        "vec" => Some(builtin_vec(args)),
        "get" => Some(builtin_get(args)),
        "assoc" => Some(builtin_assoc(args)),
        "conj" => Some(builtin_conj(args)),
        "concat" => Some(builtin_concat(args)),

        // --- Arithmetic ---
        "+" => Some(builtin_add(args)),
        "-" => Some(builtin_sub(args)),
        "*" => Some(builtin_mul(args)),
        "/" => Some(builtin_div(args)),
        "mod" => Some(builtin_mod(args)),

        // --- Comparison ---
        "=" => Some(builtin_eq(args)),
        "<" => Some(builtin_lt(args)),
        ">" => Some(builtin_gt(args)),
        "<=" => Some(builtin_le(args)),
        ">=" => Some(builtin_ge(args)),

        // --- Type ---
        "type" => {
            if args.len() != 1 {
                return Some(Err(error::arity("type", "1", args.len())));
            }
            let kw = match &args[0] {
                Val::Nil => "nil",
                Val::Bool(_) => "bool",
                Val::Int(_) => "int",
                Val::Float(_) => "float",
                Val::Str(_) => "str",
                Val::Sym(_) => "sym",
                Val::Keyword(_) => "keyword",
                Val::List(_) => "list",
                Val::Vector(_) => "vector",
                Val::Map(_) => "map",
                Val::Set(_) => "set",
                Val::Bytes(_) => "bytes",
                Val::Fn { .. } => "fn",
                Val::Recur(_) => "recur",
                Val::Macro { .. } => "macro",
                Val::Effect { .. } => "effect",
                Val::NativeFn { .. } => "native-fn",
                Val::AsyncNativeFn { .. } => "async-native-fn",
                Val::Cap { .. } => "cap",
                Val::Cell { .. } => "cell",
                Val::Resume(_) => "resume",
            };
            Some(Ok(Val::Keyword(kw.into())))
        }
        "nil?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("nil?", "1", args.len())));
            }
            Some(Ok(Val::Bool(matches!(args[0], Val::Nil))))
        }
        "some?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("some?", "1", args.len())));
            }
            Some(Ok(Val::Bool(!matches!(args[0], Val::Nil))))
        }
        "map?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("map?", "1", args.len())));
            }
            Some(Ok(Val::Bool(matches!(args[0], Val::Map(_)))))
        }
        "empty?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("empty?", "1", args.len())));
            }
            let empty = match &args[0] {
                Val::Nil => true,
                Val::List(v) | Val::Vector(v) | Val::Set(v) => v.is_empty(),
                Val::Map(m) => m.is_empty(),
                Val::Str(s) => s.is_empty(),
                other => return Some(Err(error::type_mismatch("empty?", "collection", other))),
            };
            Some(Ok(Val::Bool(empty)))
        }
        "contains?" => Some(builtin_contains(args)),

        // --- Strings ---
        "str" => {
            let mut buf = String::new();
            for arg in args {
                use std::fmt::Write;
                let _ = match arg {
                    Val::Str(s) => write!(buf, "{s}"),
                    Val::Nil => write!(buf, ""),
                    other => write!(buf, "{other}"),
                };
            }
            Some(Ok(Val::Str(buf)))
        }
        "name" => {
            if args.len() != 1 {
                return Some(Err(error::arity("name", "1", args.len())));
            }
            match &args[0] {
                Val::Keyword(k) => Some(Ok(Val::Str(k.clone()))),
                Val::Sym(s) => Some(Ok(Val::Str(s.clone()))),
                other => Some(Err(error::type_mismatch(
                    "name",
                    "keyword or symbol",
                    other,
                ))),
            }
        }
        "println" => {
            let mut buf = String::new();
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    buf.push(' ');
                }
                match arg {
                    Val::Str(s) => buf.push_str(s),
                    other => buf.push_str(&format!("{other}")),
                };
            }
            #[cfg(not(test))]
            std::println!("{buf}");
            Some(Ok(Val::Nil))
        }

        // --- Other ---
        "gensym" => {
            if !args.is_empty() {
                return Some(Err(error::arity("gensym", "0", args.len())));
            }
            let n = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            Some(Ok(Val::Sym(format!("G__{n}"))))
        }

        "ex-info" => {
            if args.len() != 2 {
                return Some(Err(crate::error::arity("ex-info", "2", args.len())));
            }
            let msg = match &args[0] {
                Val::Str(s) => s.clone(),
                other => return Some(Err(crate::error::type_mismatch("ex-info", "string", other))),
            };
            let user_map = match &args[1] {
                Val::Map(m) => m.clone(),
                other => {
                    return Some(Err(crate::error::type_mismatch(
                        "ex-info second arg",
                        "map",
                        other,
                    )))
                }
            };
            // Use the user's :type as the canonical dispatch tag.
            // Missing :type → empty Val::Str (uncatchable by tag, only
            // by wildcard) — defensive default.
            let type_tag = user_map
                .get(&Val::Keyword("type".into()))
                .cloned()
                .unwrap_or_else(|| Val::Str(String::new()));
            // Extras = user's map minus the keys ex-info canonicalizes.
            let extras = user_map
                .dissoc(&Val::Keyword("type".into()))
                .dissoc(&Val::Keyword("message".into()));
            Some(Ok(crate::error::user(type_tag, msg, extras)))
        }

        _ => None, // not a built-in
    }
}

fn builtin_contains(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("contains?", "2", args.len()));
    }
    let found = match &args[0] {
        Val::Map(m) => m.contains_key(&args[1]),
        Val::Set(items) => items.iter().any(|v| v == &args[1]),
        Val::Vector(v) => match &args[1] {
            Val::Int(i) => *i >= 0 && (*i as usize) < v.len(),
            other => return Err(error::type_mismatch("contains? vector key", "int", other)),
        },
        other => {
            return Err(error::type_mismatch(
                "contains?",
                "map, set, or vector",
                other,
            ))
        }
    };
    Ok(Val::Bool(found))
}

// --- Collection built-ins ---

fn builtin_cons(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("cons", "2", args.len()));
    }
    let tail = match &args[1] {
        Val::List(v) | Val::Vector(v) => v,
        other => {
            return Err(error::type_mismatch(
                "cons second arg",
                "list or vector",
                other,
            ))
        }
    };
    let mut result = Vec::with_capacity(1 + tail.len());
    result.push(args[0].clone());
    result.extend_from_slice(tail);
    Ok(Val::List(result))
}

fn builtin_first(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 1 {
        return Err(error::arity("first", "1", args.len()));
    }
    match &args[0] {
        Val::Nil => Ok(Val::Nil),
        Val::List(v) | Val::Vector(v) => Ok(v.first().cloned().unwrap_or(Val::Nil)),
        other => Err(error::type_mismatch("first", "collection", other)),
    }
}

fn builtin_rest(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 1 {
        return Err(error::arity("rest", "1", args.len()));
    }
    match &args[0] {
        Val::Nil => Ok(Val::List(vec![])),
        Val::List(v) | Val::Vector(v) => {
            if v.is_empty() {
                Ok(Val::List(vec![]))
            } else {
                Ok(Val::List(v[1..].to_vec()))
            }
        }
        other => Err(error::type_mismatch("rest", "collection", other)),
    }
}

fn builtin_count(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 1 {
        return Err(error::arity("count", "1", args.len()));
    }
    let n = match &args[0] {
        Val::Nil => 0,
        Val::List(v) | Val::Vector(v) | Val::Set(v) => v.len(),
        Val::Map(m) => m.len(),
        Val::Str(s) => s.chars().count(),
        other => return Err(error::type_mismatch("count", "collection or nil", other)),
    };
    Ok(Val::Int(n as i64))
}

fn builtin_vec(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 1 {
        return Err(error::arity("vec", "1", args.len()));
    }
    match &args[0] {
        Val::Nil => Ok(Val::Vector(vec![])),
        Val::List(v) => Ok(Val::Vector(v.clone())),
        Val::Vector(_) => Ok(args[0].clone()),
        other => Err(error::type_mismatch("vec", "list or vector", other)),
    }
}

fn builtin_get(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("get", "2", args.len()));
    }
    match &args[0] {
        Val::Map(m) => Ok(m.get(&args[1]).cloned().unwrap_or(Val::Nil)),
        Val::Vector(v) => match &args[1] {
            Val::Int(i) => {
                if *i < 0 {
                    Ok(Val::Nil)
                } else {
                    Ok(v.get(*i as usize).cloned().unwrap_or(Val::Nil))
                }
            }
            other => Err(error::type_mismatch("get vector index", "int", other)),
        },
        Val::Nil => Ok(Val::Nil),
        other => Err(error::type_mismatch("get", "map or vector", other)),
    }
}

fn builtin_assoc(args: &[Val]) -> Result<Val, Val> {
    if args.is_empty() || !(args.len() - 1).is_multiple_of(2) {
        return Err(error::arity(
            "assoc",
            "map + key-value pairs (odd total)",
            args.len(),
        ));
    }
    let mut m = match &args[0] {
        Val::Map(m) => m.clone(),
        other => return Err(error::type_mismatch("assoc first arg", "map", other)),
    };
    for chunk in args[1..].chunks(2) {
        m = m.assoc(chunk[0].clone(), chunk[1].clone());
    }
    Ok(Val::Map(m))
}

fn builtin_conj(args: &[Val]) -> Result<Val, Val> {
    if args.len() < 2 {
        return Err(error::arity("conj", "at least 2", args.len()));
    }
    match &args[0] {
        Val::Vector(v) => {
            let mut result = v.clone();
            result.extend_from_slice(&args[1..]);
            Ok(Val::Vector(result))
        }
        Val::List(v) => {
            // Clojure: conj on lists PREPENDS each item
            let mut result = v.clone();
            for item in &args[1..] {
                result.insert(0, item.clone());
            }
            Ok(Val::List(result))
        }
        Val::Map(m) => {
            let mut result = m.clone();
            for item in &args[1..] {
                match item {
                    Val::Vector(pair) if pair.len() == 2 => {
                        result = result.assoc(pair[0].clone(), pair[1].clone());
                    }
                    other => {
                        return Err(error::type_mismatch(
                            "conj map entry",
                            "[key val] vector",
                            other,
                        ))
                    }
                }
            }
            Ok(Val::Map(result))
        }
        other => Err(error::type_mismatch("conj", "collection", other)),
    }
}

fn builtin_concat(args: &[Val]) -> Result<Val, Val> {
    let mut result = Vec::new();
    for arg in args {
        match arg {
            Val::Nil => {}
            Val::List(v) | Val::Vector(v) => result.extend(v.iter().cloned()),
            other => return Err(error::type_mismatch("concat", "sequence or nil", other)),
        }
    }
    Ok(Val::List(result))
}

// --- Arithmetic helpers ---

/// Extract a numeric pair, promoting to Float if mixed.
enum NumPair {
    Ints(i64, i64),
    Floats(f64, f64),
}

fn num_pair(a: &Val, b: &Val) -> Result<NumPair, Val> {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => Ok(NumPair::Ints(*x, *y)),
        (Val::Float(x), Val::Float(y)) => Ok(NumPair::Floats(*x, *y)),
        (Val::Int(x), Val::Float(y)) => Ok(NumPair::Floats(*x as f64, *y)),
        (Val::Float(x), Val::Int(y)) => Ok(NumPair::Floats(*x, *y as f64)),
        _ => Err(error::type_mismatch("arithmetic", "number pair", a)),
    }
}

fn builtin_add(args: &[Val]) -> Result<Val, Val> {
    let mut acc = Val::Int(0);
    for a in args {
        acc = match num_pair(&acc, a)? {
            NumPair::Ints(x, y) => Val::Int(x + y),
            NumPair::Floats(x, y) => Val::Float(x + y),
        };
    }
    Ok(acc)
}

fn builtin_sub(args: &[Val]) -> Result<Val, Val> {
    if args.is_empty() {
        return Err(error::arity("-", "at least 1", 0));
    }
    if args.len() == 1 {
        return match &args[0] {
            Val::Int(n) => Ok(Val::Int(-n)),
            Val::Float(n) => Ok(Val::Float(-n)),
            other => Err(error::type_mismatch("-", "number", other)),
        };
    }
    let mut acc = args[0].clone();
    for a in &args[1..] {
        acc = match num_pair(&acc, a)? {
            NumPair::Ints(x, y) => Val::Int(x - y),
            NumPair::Floats(x, y) => Val::Float(x - y),
        };
    }
    Ok(acc)
}

fn builtin_mul(args: &[Val]) -> Result<Val, Val> {
    let mut acc = Val::Int(1);
    for a in args {
        acc = match num_pair(&acc, a)? {
            NumPair::Ints(x, y) => Val::Int(x * y),
            NumPair::Floats(x, y) => Val::Float(x * y),
        };
    }
    Ok(acc)
}

fn builtin_div(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("/", "2", args.len()));
    }
    match num_pair(&args[0], &args[1])? {
        NumPair::Ints(_, 0) => Err(error::internal("/", "division by zero")),
        NumPair::Ints(x, y) => Ok(Val::Int(x / y)),
        NumPair::Floats(_, 0.0) => Err(error::internal("/", "division by zero")),
        NumPair::Floats(x, y) => Ok(Val::Float(x / y)),
    }
}

fn builtin_mod(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("mod", "2", args.len()));
    }
    match num_pair(&args[0], &args[1])? {
        NumPair::Ints(_, 0) => Err(error::internal("mod", "division by zero")),
        NumPair::Ints(x, y) => Ok(Val::Int(x % y)),
        NumPair::Floats(_, 0.0) => Err(error::internal("mod", "division by zero")),
        NumPair::Floats(x, y) => Ok(Val::Float(x % y)),
    }
}

// --- Comparison built-ins ---

fn builtin_eq(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("=", "2", args.len()));
    }
    Ok(Val::Bool(args[0] == args[1]))
}

fn numeric_cmp(a: &Val, b: &Val) -> Result<std::cmp::Ordering, Val> {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => Ok(x.cmp(y)),
        (Val::Float(x), Val::Float(y)) => x
            .partial_cmp(y)
            .ok_or_else(|| error::internal("comparison", "NaN")),
        (Val::Int(x), Val::Float(y)) => (*x as f64)
            .partial_cmp(y)
            .ok_or_else(|| error::internal("comparison", "NaN")),
        (Val::Float(x), Val::Int(y)) => x
            .partial_cmp(&(*y as f64))
            .ok_or_else(|| error::internal("comparison", "NaN")),
        _ => Err(error::type_mismatch("comparison", "number pair", a)),
    }
}

fn builtin_lt(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("<", "2", args.len()));
    }
    Ok(Val::Bool(numeric_cmp(&args[0], &args[1])?.is_lt()))
}

fn builtin_gt(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity(">", "2", args.len()));
    }
    Ok(Val::Bool(numeric_cmp(&args[0], &args[1])?.is_gt()))
}

fn builtin_le(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("<=", "2", args.len()));
    }
    Ok(Val::Bool(!numeric_cmp(&args[0], &args[1])?.is_gt()))
}

fn builtin_ge(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity(">=", "2", args.len()));
    }
    Ok(Val::Bool(!numeric_cmp(&args[0], &args[1])?.is_lt()))
}

// ---------------------------------------------------------------------------
// Expr-based evaluation (new pipeline)
// ---------------------------------------------------------------------------

use crate::expr::{self, Expr};

/// Evaluate an analyzed Expr in the given environment.
pub fn eval_expr<'a, D: Dispatch>(
    expr: &'a Expr,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
    Box::pin(async move {
        match expr {
            Expr::Const(v) => Ok(v.clone()),

            Expr::Sym(s) => match env.get(s) {
                Some(v) => Ok(v.clone()),
                None => Ok(Val::Sym(s.clone())),
            },

            Expr::Def { name, value } => {
                let val = eval_expr(value, env, dispatch).await?;
                env.set_root(name.clone(), val.clone());
                Ok(val)
            }

            Expr::If { test, then, else_ } => {
                let test_val = eval_expr(test, env, dispatch).await?;
                if is_truthy(&test_val) {
                    eval_expr(then, env, dispatch).await
                } else {
                    eval_expr(else_, env, dispatch).await
                }
            }

            Expr::Do { body } => {
                let mut result = Val::Nil;
                for e in body {
                    result = eval_expr(e, env, dispatch).await?;
                }
                Ok(result)
            }

            Expr::Let { bindings, body } => {
                env.push_frame();
                let result = async {
                    for (binding, val_expr) in bindings {
                        let val = eval_expr(val_expr, env, dispatch).await?;
                        match binding {
                            crate::pattern::LetBinding::Simple(name) => {
                                env.set(name.clone(), val);
                            }
                            crate::pattern::LetBinding::Destructure(pat) => {
                                crate::pattern::bind_pattern(pat, &val, "let", &mut |name, v| {
                                    env.set(name.to_string(), v);
                                })?;
                            }
                        }
                    }
                    let mut result = Val::Nil;
                    for e in body {
                        result = eval_expr(e, env, dispatch).await?;
                    }
                    Ok(result)
                }
                .await;
                env.pop_frame();
                result
            }

            Expr::Quote(val) => Ok(val.clone()),

            Expr::Fn { arities } => {
                // Convert FnArityExpr → FnArity with FnBody::Analyzed
                let free_vars: BTreeSet<&String> = arities
                    .iter()
                    .flat_map(|arity| arity.free_vars.iter())
                    .collect();
                let captured_env = env.filter_to(free_vars);
                let (is_cap_free, cap_violation) = compute_cap_status(&captured_env);
                let fn_arities: Vec<FnArity> = arities
                    .iter()
                    .map(|a| FnArity {
                        params: a.params.clone(),
                        variadic: a.variadic.clone(),
                        body: FnBody::Analyzed(a.body.clone()),
                    })
                    .collect();
                Ok(Val::Fn {
                    arities: fn_arities,
                    env: Rc::new(captured_env),
                    is_cap_free,
                    cap_violation,
                })
            }

            Expr::Loop { bindings, body } => {
                env.push_frame();
                // Evaluate initial bindings — track binding specs for recur
                let mut binding_specs: Vec<crate::pattern::LetBinding> =
                    Vec::with_capacity(bindings.len());
                for (binding, val_expr) in bindings {
                    let val = eval_expr(val_expr, env, dispatch).await?;
                    match binding {
                        crate::pattern::LetBinding::Simple(name) => {
                            env.set(name.clone(), val);
                            binding_specs.push(crate::pattern::LetBinding::Simple(name.clone()));
                        }
                        crate::pattern::LetBinding::Destructure(pat) => {
                            crate::pattern::bind_pattern(pat, &val, "loop", &mut |name, v| {
                                env.set(name.to_string(), v);
                            })?;
                            binding_specs
                                .push(crate::pattern::LetBinding::Destructure(pat.clone()));
                        }
                    }
                }
                let num_bindings = binding_specs.len();

                let result = async {
                    loop {
                        let mut result = Val::Nil;
                        for e in body {
                            result = eval_expr(e, env, dispatch).await?;
                        }
                        match result {
                            Val::Recur(new_vals) => {
                                if new_vals.len() != num_bindings {
                                    return Err(error::arity(
                                        "recur",
                                        &num_bindings.to_string(),
                                        new_vals.len(),
                                    ));
                                }
                                // Re-bind: re-apply patterns for destructuring bindings
                                for (spec, val) in binding_specs.iter().zip(new_vals) {
                                    match spec {
                                        crate::pattern::LetBinding::Simple(name) => {
                                            env.set(name.clone(), val);
                                        }
                                        crate::pattern::LetBinding::Destructure(pat) => {
                                            crate::pattern::bind_pattern(
                                                pat,
                                                &val,
                                                "recur",
                                                &mut |name, v| {
                                                    env.set(name.to_string(), v);
                                                },
                                            )?;
                                        }
                                    }
                                }
                            }
                            other => return Ok(other),
                        }
                    }
                }
                .await;
                env.pop_frame();
                result
            }

            Expr::Recur { args } => {
                let mut evaled = Vec::with_capacity(args.len());
                for a in args {
                    evaled.push(eval_expr(a, env, dispatch).await?);
                }
                Ok(Val::Recur(evaled))
            }

            Expr::Perform { target, args } => {
                let target_val = eval_expr(target, env, dispatch).await?;
                let mut evaled_args = Vec::with_capacity(args.len());
                for a in args {
                    evaled_args.push(eval_expr(a, env, dispatch).await?);
                }

                // Build EffectTarget + data payload from the two perform forms.
                let (effect_target, data_val) = match &target_val {
                    // (perform :keyword data) — keyword/environmental effect
                    Val::Keyword(s) => {
                        if evaled_args.len() != 1 {
                            return Err(error::arity(
                                "perform (keyword effect)",
                                "1 data arg",
                                evaled_args.len(),
                            ));
                        }
                        (
                            effect::EffectTarget::Keyword(s.clone()),
                            evaled_args.into_iter().next().unwrap(),
                        )
                    }
                    // (perform cap :method args...) — cap-targeted effect
                    Val::Cap { .. } => {
                        return perform_cap_value(&target_val, &evaled_args, env, dispatch).await
                    }
                    other => {
                        return Err(error::type_mismatch(
                            "perform target",
                            "keyword or cap",
                            other,
                        ))
                    }
                };

                // Stack walk: find the matching handler frame.
                perform_dispatch(&env.handler_stack, effect_target, data_val).await
            }

            Expr::Match { expr, clauses } => {
                // Evaluate the scrutinee
                let value = eval_expr(expr, env, dispatch).await?;

                // Try each clause in order (linear, first match wins)
                for (pattern, body) in clauses {
                    if let Some(bindings) = crate::pattern::match_pattern(pattern, &value) {
                        // Push new frame with pattern bindings
                        env.push_frame();
                        for (name, val) in bindings {
                            env.set(name, val);
                        }
                        let result = eval_expr(body, env, dispatch).await;
                        env.pop_frame();
                        return result;
                    }
                }

                // No clause matched — runtime error
                Err(error::internal(
                    "match",
                    format!("no clause matched value {value}"),
                ))
            }

            Expr::WithEffectHandler {
                target,
                handler,
                body,
            } => {
                // Evaluate target and handler BEFORE pushing context.
                let target_val = eval_expr(target, env, dispatch).await?;
                let handler_val = eval_expr(handler, env, dispatch).await?;

                let effect_target = match &target_val {
                    Val::Keyword(s) => effect::EffectTarget::Keyword(s.clone()),
                    Val::Cap {
                        name,
                        schema_cid,
                        cap_id,
                        ..
                    } => effect::EffectTarget::Cap {
                        name: name.clone(),
                        schema_cid: schema_cid.clone(),
                        cap_id: *cap_id,
                    },
                    other => {
                        return Err(error::type_mismatch(
                            "with-effect-handler target",
                            "keyword or cap",
                            other,
                        ))
                    }
                };

                // Depth check.
                let hs = env.handler_stack.clone();
                let caller_hs = hs.clone();
                if hs.borrow().len() >= effect::MAX_HANDLER_DEPTH {
                    return Err(error::internal(
                        "with-effect-handler",
                        format!(
                            "handler stack depth limit ({}) exceeded",
                            effect::MAX_HANDLER_DEPTH
                        ),
                    ));
                }

                // Create handler context with the target.
                let ctx = Rc::new(RefCell::new(effect::HandlerContext {
                    slot: Rc::new(RefCell::new(effect::EffectSlot::new())),
                    target: effect_target,
                }));
                hs.borrow_mut().push(ctx.clone());

                // Create body future.
                let mut body_fut = {
                    let body = body.clone();
                    Box::pin(async move {
                        let mut result = Val::Nil;
                        for e in &body {
                            result = eval_expr(e, env, dispatch).await?;
                        }
                        Ok::<Val, Val>(result)
                    })
                };

                // State machine: alternate between polling body and handling effects.
                enum HandlerState<'b> {
                    Polling,
                    Handling(Pin<Box<dyn Future<Output = Result<Val, Val>> + 'b>>),
                }
                let mut state = HandlerState::Polling;

                let result: Result<Val, Val> = std::future::poll_fn(|cx| {
                    loop {
                        match &mut state {
                            HandlerState::Polling => {
                                match body_fut.as_mut().poll(cx) {
                                    Poll::Ready(result) => return Poll::Ready(result),
                                    Poll::Pending => {
                                        let pending = ctx.borrow().slot.borrow_mut().pending.take();
                                        match pending {
                                            Some((_target, data, resume_tx)) => {
                                                // Dispatch to handler based on its type.
                                                match &handler_val {
                                                    Val::Fn {
                                                        arities,
                                                        env: captured_env,
                                                        ..
                                                    } => {
                                                        // Pop before handle (handler's performs go to outer handlers).
                                                        hs.borrow_mut().pop();

                                                        let has_2_arity = arities.iter().any(|a| {
                                                            (a.variadic.is_none()
                                                                && a.params.len() == 2)
                                                                || (a.variadic.is_some()
                                                                    && a.params.len() <= 2)
                                                        });
                                                        let owned_arities = arities.clone();
                                                        let owned_env = captured_env.clone();

                                                        let handler_fut: Pin<
                                                            Box<
                                                                dyn Future<
                                                                        Output = Result<Val, Val>,
                                                                    > + '_,
                                                            >,
                                                        > = if has_2_arity {
                                                            let resume_fn =
                                                                effect::make_resume_fn(resume_tx);
                                                            let args = vec![data, resume_fn];
                                                            let handler_hs = caller_hs.clone();
                                                            Box::pin(async move {
                                                                invoke_fn(
                                                                    &owned_arities,
                                                                    &owned_env,
                                                                    &args,
                                                                    dispatch,
                                                                    handler_hs,
                                                                )
                                                                .await
                                                            })
                                                        } else {
                                                            drop(resume_tx);
                                                            let args = vec![data];
                                                            let handler_hs = caller_hs.clone();
                                                            Box::pin(async move {
                                                                invoke_fn(
                                                                    &owned_arities,
                                                                    &owned_env,
                                                                    &args,
                                                                    dispatch,
                                                                    handler_hs,
                                                                )
                                                                .await
                                                            })
                                                        };

                                                        state = HandlerState::Handling(handler_fut);
                                                        continue;
                                                    }
                                                    Val::NativeFn { func, .. } => {
                                                        hs.borrow_mut().pop();
                                                        let resume_fn =
                                                            effect::make_resume_fn(resume_tx);
                                                        let result = func(&[data, resume_fn]);
                                                        match result {
                                                            Err(Val::Resume(_)) => {
                                                                hs.borrow_mut().push(ctx.clone());
                                                                state = HandlerState::Polling;
                                                                cx.waker().wake_by_ref();
                                                                return Poll::Pending;
                                                            }
                                                            other => {
                                                                hs.borrow_mut().push(ctx.clone());
                                                                return Poll::Ready(other);
                                                            }
                                                        }
                                                    }
                                                    Val::AsyncNativeFn { func, .. } => {
                                                        hs.borrow_mut().pop();
                                                        let resume_fn =
                                                            effect::make_resume_fn(resume_tx);
                                                        let func = func.clone();
                                                        let handler_fut: Pin<
                                                            Box<
                                                                dyn Future<
                                                                        Output = Result<Val, Val>,
                                                                    > + '_,
                                                            >,
                                                        > = Box::pin(func(vec![data, resume_fn]));
                                                        state = HandlerState::Handling(handler_fut);
                                                        continue;
                                                    }
                                                    other => {
                                                        drop(resume_tx);
                                                        return Poll::Ready(Err(
                                                            error::type_mismatch(
                                                                "with-effect-handler handler",
                                                                "function",
                                                                other,
                                                            ),
                                                        ));
                                                    }
                                                }
                                            }
                                            None => return Poll::Pending,
                                        }
                                    }
                                }
                            }
                            HandlerState::Handling(handler_fut) => {
                                match handler_fut.as_mut().poll(cx) {
                                    Poll::Pending => return Poll::Pending,
                                    Poll::Ready(result) => {
                                        hs.borrow_mut().push(ctx.clone());
                                        match result {
                                            Err(Val::Resume(_)) => {
                                                state = HandlerState::Polling;
                                                cx.waker().wake_by_ref();
                                                return Poll::Pending;
                                            }
                                            other => return Poll::Ready(other),
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
                .await;

                // Pop our handler context (if still on the stack).
                let mut stack = hs.borrow_mut();
                if let Some(last) = stack.last() {
                    if Rc::ptr_eq(last, &ctx) {
                        stack.pop();
                    }
                }
                drop(stack);

                result
            }

            Expr::DefMacro { name, raw_args } => {
                // raw_args contains [params, body...] — no name (already extracted).
                let arities = parse_macro_arities(raw_args)?;
                let captured_env = Rc::new(env.snapshot());
                let (is_cap_free, cap_violation) = compute_cap_status(&captured_env);
                let val = Val::Macro {
                    arities,
                    env: captured_env,
                    is_cap_free,
                    cap_violation,
                };
                env.set_root(name.clone(), val.clone());
                Ok(val)
            }

            Expr::Call {
                head,
                args,
                raw_args,
            } => {
                // Special form: (defcap name :method fn ...)
                if head == "defcap" {
                    if raw_args.len() < 3 {
                        return Err(error::arity("defcap", "at least 3", raw_args.len()));
                    }
                    let name = match &raw_args[0] {
                        Val::Sym(s) => s.clone(),
                        other => return Err(error::type_mismatch("defcap name", "symbol", other)),
                    };
                    if (raw_args.len() - 1) % 2 != 0 {
                        return Err(error::internal(
                            "defcap",
                            "method definitions must be keyword/function pairs",
                        ));
                    }

                    let mut methods = HashMap::new();
                    let mut method_names = BTreeSet::new();
                    for pair in raw_args[1..].chunks(2) {
                        let method_name = match &pair[0] {
                            Val::Keyword(k) => k.clone(),
                            other => {
                                return Err(error::type_mismatch(
                                    "defcap method name",
                                    "keyword",
                                    other,
                                ))
                            }
                        };
                        let method_expr = expr::analyze(&pair[1])?;
                        let method_val = eval_expr(&method_expr, env, dispatch).await?;
                        if !matches!(
                            method_val,
                            Val::Fn { .. } | Val::NativeFn { .. } | Val::AsyncNativeFn { .. }
                        ) {
                            return Err(error::type_mismatch(
                                "defcap method value",
                                "function",
                                &method_val,
                            ));
                        }
                        method_names.insert(method_name.clone());
                        methods.insert(method_name, method_val);
                    }

                    let schema_cid = "glia:defcap:v1".to_string();
                    let descriptor = cap_descriptor_bytes(&name, &schema_cid, &method_names);
                    let cap = make_cap(
                        name.clone(),
                        schema_cid,
                        Rc::new(GliaCapInner {
                            methods,
                            descriptor,
                        }),
                    );
                    env.set_root(name, cap.clone());
                    return Ok(cap);
                }

                // Special form: (attenuate cap [:method ...])
                if head == "attenuate" {
                    if args.len() != 2 {
                        return Err(error::arity("attenuate", "2", args.len()));
                    }
                    let cap_val = eval_expr(&args[0], env, dispatch).await?;
                    let allow_val = eval_expr(&args[1], env, dispatch).await?;
                    let mut allow_methods = parse_allow_methods(&allow_val)?;

                    let (name, schema_cid, base, nested_allow): (
                        String,
                        String,
                        Val,
                        Option<BTreeSet<String>>,
                    ) = match &cap_val {
                        Val::Cap {
                            name,
                            schema_cid,
                            inner,
                            ..
                        } => {
                            if let Some(inner_att) = inner.downcast_ref::<AttenuatedCapInner>() {
                                (
                                    name.clone(),
                                    schema_cid.clone(),
                                    inner_att.base.clone(),
                                    Some(inner_att.allow_methods.clone()),
                                )
                            } else {
                                (name.clone(), schema_cid.clone(), cap_val.clone(), None)
                            }
                        }
                        other => {
                            return Err(error::type_mismatch("attenuate first arg", "cap", other))
                        }
                    };

                    if let Some(existing) = nested_allow {
                        allow_methods = allow_methods.intersection(&existing).cloned().collect();
                    }

                    let descriptor = cap_descriptor_bytes(&name, &schema_cid, &allow_methods);
                    return Ok(make_cap(
                        name,
                        schema_cid,
                        Rc::new(AttenuatedCapInner {
                            base,
                            allow_methods,
                            descriptor,
                        }),
                    ));
                }

                // 1. Check for macro expansion
                if let Some(Val::Macro {
                    arities,
                    env: captured_env,
                    ..
                }) = env.get(head)
                {
                    let arities = arities.clone();
                    let captured_env = captured_env.clone();
                    let expanded = invoke_macro(
                        &arities,
                        &captured_env,
                        raw_args,
                        dispatch,
                        env.handler_stack.clone(),
                    )
                    .await?;
                    // Re-analyze and eval the expanded form
                    let analyzed = expr::analyze(&expanded)?;
                    return eval_expr(&analyzed, env, dispatch).await;
                }

                // 2. Check env for fn or native-fn
                if let Some(Val::Fn {
                    arities,
                    env: captured_env,
                    ..
                }) = env.get(head)
                {
                    let arities = arities.clone();
                    let captured_env = captured_env.clone();
                    let evaled_args = eval_expr_args(args, env, dispatch).await?;
                    return invoke_fn(
                        &arities,
                        &captured_env,
                        &evaled_args,
                        dispatch,
                        env.handler_stack.clone(),
                    )
                    .await;
                }
                if let Some(Val::NativeFn { func, .. }) = env.get(head) {
                    let func = func.clone();
                    let evaled_args = eval_expr_args(args, env, dispatch).await?;
                    return func(&evaled_args);
                }
                if let Some(Val::AsyncNativeFn { func, .. }) = env.get(head) {
                    let func = func.clone();
                    let evaled_args = eval_expr_args(args, env, dispatch).await?;
                    return func(evaled_args).await;
                }

                // 3. Evaluate args for remaining paths
                let evaled_args = eval_expr_args(args, env, dispatch).await?;

                // 4. HOF builtins
                if head == "map" || head == "filter" || head == "reduce" {
                    return eval_hof(head, &evaled_args, env, dispatch).await;
                }

                // 4b. cell builtin (captures Val::Cap bindings from scope)
                if head == "cell" {
                    let wasm = match evaled_args.first() {
                        Some(Val::Bytes(b)) => b.clone(),
                        Some(other) => {
                            return Err(error::type_mismatch(
                                "cell first arg (wasm)",
                                "bytes",
                                other,
                            ))
                        }
                        None => return Err(error::arity("cell", "1", 0)),
                    };
                    if evaled_args.len() > 1 {
                        return Err(error::arity("cell", "1", evaled_args.len()));
                    }
                    let caps = env.collect_caps();
                    return Ok(Val::Cell { wasm, caps });
                }

                // 5. Sync builtins
                if let Some(result) = eval_builtin(head, &evaled_args) {
                    return result;
                }

                // 6. Generic dispatch
                dispatch.call(head, &evaled_args).await
            }

            Expr::Apply { args } => {
                let evaled = eval_expr_args(args, env, dispatch).await?;
                if evaled.len() < 2 {
                    return Err(error::arity("apply", "at least 2", evaled.len()));
                }
                let func = &evaled[0];
                let last = &evaled[evaled.len() - 1];
                let trailing = match last {
                    Val::List(v) | Val::Vector(v) => v.clone(),
                    other => {
                        return Err(error::type_mismatch(
                            "apply last arg",
                            "list or vector",
                            other,
                        ))
                    }
                };
                let mut spread = evaled[1..evaled.len() - 1].to_vec();
                spread.extend(trailing);

                match func {
                    Val::Sym(fname) => {
                        if let Some(Val::Fn {
                            arities,
                            env: captured_env,
                            ..
                        }) = env.get(fname)
                        {
                            let arities = arities.clone();
                            let captured_env = captured_env.clone();
                            return invoke_fn(
                                &arities,
                                &captured_env,
                                &spread,
                                dispatch,
                                env.handler_stack.clone(),
                            )
                            .await;
                        }
                        if let Some(Val::NativeFn { func, .. }) = env.get(fname) {
                            return func.clone()(&spread);
                        }
                        if let Some(Val::AsyncNativeFn { func, .. }) = env.get(fname) {
                            return func.clone()(spread).await;
                        }
                        if let Some(result) = eval_builtin(fname, &spread) {
                            return result;
                        }
                        dispatch.call(fname, &spread).await
                    }
                    Val::Fn {
                        arities,
                        env: captured_env,
                        ..
                    } => {
                        let arities = arities.clone();
                        let captured_env = captured_env.clone();
                        invoke_fn(
                            &arities,
                            &captured_env,
                            &spread,
                            dispatch,
                            env.handler_stack.clone(),
                        )
                        .await
                    }
                    Val::NativeFn { func, .. } => func(&spread),
                    Val::AsyncNativeFn { func, .. } => func(spread).await,
                    other => Err(error::type_mismatch(
                        "apply first arg",
                        "symbol or fn",
                        other,
                    )),
                }
            }

            Expr::Vector(exprs) => {
                let mut items = Vec::with_capacity(exprs.len());
                for e in exprs {
                    items.push(eval_expr(e, env, dispatch).await?);
                }
                Ok(Val::Vector(items))
            }

            Expr::Map(pairs) => {
                let mut items = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    items.push((
                        eval_expr(k, env, dispatch).await?,
                        eval_expr(v, env, dispatch).await?,
                    ));
                }
                Ok(Val::Map(ValMap::from_pairs(items)))
            }

            Expr::Set(exprs) => {
                let mut items = Vec::with_capacity(exprs.len());
                for e in exprs {
                    items.push(eval_expr(e, env, dispatch).await?);
                }
                Ok(Val::Set(items))
            }
        }
    })
}

/// Evaluate a list of Expr args into Vec<Val>.
async fn eval_expr_args<'a, D: Dispatch>(
    args: &'a [Expr],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Vec<Val>, Val> {
    let mut result = Vec::with_capacity(args.len());
    for a in args {
        result.push(eval_expr(a, env, dispatch).await?);
    }
    Ok(result)
}

fn cap_method_and_args(args: &[Val], ctx: &'static str) -> Result<(String, Vec<Val>), Val> {
    let method = match args.first() {
        Some(Val::Keyword(k)) => k.clone(),
        Some(other) => return Err(error::type_mismatch(ctx, "keyword method", other)),
        None => return Err(error::arity(ctx, "at least 1", 0)),
    };
    Ok((method, args[1..].to_vec()))
}

async fn invoke_cap_method_value<'a, D: Dispatch>(
    method_val: Val,
    args: &[Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    match method_val {
        Val::Fn {
            arities,
            env: captured_env,
            ..
        } => {
            invoke_fn_with_handler_stack(
                &arities,
                &captured_env,
                args,
                dispatch,
                env.handler_stack.clone(),
            )
            .await
        }
        Val::NativeFn { func, .. } => func(args),
        Val::AsyncNativeFn { func, .. } => func(args.to_vec()).await,
        other => Err(error::type_mismatch("defcap method", "function", &other)),
    }
}

async fn perform_cap_value<'a, D: Dispatch>(
    cap: &Val,
    args: &[Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Val> {
    let mut current = cap.clone();
    let payload = args.to_vec();

    loop {
        let Val::Cap {
            name,
            schema_cid,
            cap_id,
            inner,
        } = &current
        else {
            return Err(error::type_mismatch("perform target", "cap", &current));
        };

        let effect_target = effect::EffectTarget::Cap {
            name: name.clone(),
            schema_cid: schema_cid.clone(),
            cap_id: *cap_id,
        };
        match perform_dispatch(
            &env.handler_stack,
            effect_target,
            Val::List(payload.clone()),
        )
        .await
        {
            Ok(value) => return Ok(value),
            Err(Val::Effect { effect_type, .. }) if effect_type == format!("cap:{name}") => {}
            Err(err) => return Err(err),
        }

        if let Some(attenuated) = inner.downcast_ref::<AttenuatedCapInner>() {
            let (method, _) = cap_method_and_args(&payload, "perform (attenuated cap)")?;
            if !attenuated.allow_methods.contains(&method) {
                return Err(error::permission_denied(
                    &format!("method :{method} denied by attenuation policy on '{name}'"),
                    None,
                ));
            }
            current = attenuated.base.clone();
            continue;
        }

        if let Some(glia_cap) = inner.downcast_ref::<GliaCapInner>() {
            let (method, method_args) = cap_method_and_args(&payload, "perform (defcap)")?;
            let method_val = glia_cap.methods.get(&method).cloned().ok_or_else(|| {
                error::permission_denied(
                    &format!("method :{method} is not available on capability '{name}'"),
                    None,
                )
            })?;
            return invoke_cap_method_value(method_val, &method_args, env, dispatch).await;
        }

        return Err(Val::Effect {
            effect_type: format!("cap:{name}"),
            data: Box::new(Val::List(payload)),
        });
    }
}

/// Top-level Expr evaluation with Recur guard.
pub fn eval_toplevel_expr<'a, D: Dispatch>(
    expr: &'a Expr,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
    Box::pin(async move {
        let result = eval_expr(expr, env, dispatch).await?;
        match result {
            Val::Recur(_) => Err(error::internal("recur", "not in tail position")),
            other => Ok(other),
        }
    })
}

/// Top-level evaluation wrapper.
///
/// Analyzes the Val into an Expr, then evaluates it.
/// Catches escaped `Val::Recur` sentinels, converting
/// them to an error ("recur not in tail position").
pub fn eval_toplevel<'a, D: Dispatch>(
    val: &'a Val,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
    Box::pin(async move {
        let analyzed = expr::analyze(val)?;
        let result = eval_expr(&analyzed, env, dispatch).await?;
        match result {
            Val::Recur(_) => Err(error::internal("recur", "not in tail position")),
            other => Ok(other),
        }
    })
}

/// Evaluate a Glia expression.
///
/// Resolution order:
/// 1. Special forms — matched by name, receive unevaluated args
/// 2. Macro expansion — if head is Val::Macro in env, expand + re-eval
/// 3. Env lookup — if head resolves to Val::Fn, invoke it
/// 4. Built-in functions — eval args, call builtin
/// 5. `apply` — special handling (re-dispatches)
/// 6. Generic path — eval args, delegate to Dispatch (capability calls)
///
/// Non-list values are self-evaluating (returned as-is), except symbols
/// which are looked up in `env` (unbound symbols pass through for Dispatch).
pub fn eval<'a, D: Dispatch>(
    expr: &'a Val,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
    Box::pin(async move {
        match expr {
            Val::List(items) if items.is_empty() => Ok(Val::Nil),
            Val::List(items) => {
                let head = match &items[0] {
                    Val::Sym(s) => s.as_str(),
                    other => return Err(error::type_mismatch("call head", "symbol", other)),
                };
                let raw_args = &items[1..];

                // --- Special forms (unevaluated args) ---
                match head {
                    "def" => return eval_def(raw_args, env, dispatch).await,
                    "if" => return eval_if(raw_args, env, dispatch).await,
                    "do" => return eval_do(raw_args, env, dispatch).await,
                    "let" => return eval_let(raw_args, env, dispatch).await,
                    "quote" => {
                        return if raw_args.len() != 1 {
                            Err(error::arity("quote", "1", raw_args.len()))
                        } else {
                            Ok(raw_args[0].clone())
                        };
                    }

                    "fn" => return eval_fn(raw_args, env),

                    "loop" => return eval_loop(raw_args, env, dispatch).await,
                    "recur" => return eval_recur(raw_args, env, dispatch).await,

                    "defmacro" => return eval_defmacro(raw_args, env).await,

                    // Reader markers — error if they escape syntax-quote
                    "unquote" => {
                        return Err(error::internal("unquote", "~ not inside syntax-quote"));
                    }
                    "splice-unquote" => {
                        return Err(error::internal(
                            "splice-unquote",
                            "~@ not inside syntax-quote",
                        ));
                    }

                    _ => {} // fall through to macro / fn / builtins / dispatch
                }

                // --- Macro expansion: if head resolves to a macro, expand + eval ---
                if let Some(Val::Macro {
                    arities,
                    env: captured_env,
                    ..
                }) = env.get(head)
                {
                    let arities = arities.clone();
                    let captured_env = captured_env.clone();
                    // Macro receives RAW (unevaluated) args, body runs in captured env
                    let expanded = invoke_macro(
                        &arities,
                        &captured_env,
                        raw_args,
                        dispatch,
                        env.handler_stack.clone(),
                    )
                    .await?;
                    // Re-evaluate the expanded form in the CALLER's env
                    return eval(&expanded, env, dispatch).await;
                }

                // --- Env lookup: if head resolves to a fn, invoke it ---
                if let Some(Val::Fn {
                    arities,
                    env: captured_env,
                    ..
                }) = env.get(head)
                {
                    let arities = arities.clone();
                    let captured_env = captured_env.clone();
                    let args = eval_args(raw_args, env, dispatch).await?;
                    return invoke_fn(
                        &arities,
                        &captured_env,
                        &args,
                        dispatch,
                        env.handler_stack.clone(),
                    )
                    .await;
                }

                // --- Built-in: apply (needs re-dispatch, so handled here) ---
                if head == "apply" {
                    let args = eval_args(raw_args, env, dispatch).await?;
                    if args.len() < 2 {
                        return Err(error::arity("apply", "at least 2", args.len()));
                    }
                    // First arg is the function (symbol or Val::Fn)
                    let func = &args[0];
                    // Last arg must be a collection; middle args are prepended
                    let last = &args[args.len() - 1];
                    let trailing = match last {
                        Val::List(v) | Val::Vector(v) => v.clone(),
                        other => {
                            return Err(error::type_mismatch(
                                "apply last arg",
                                "list or vector",
                                other,
                            ))
                        }
                    };
                    let mut spread = args[1..args.len() - 1].to_vec();
                    spread.extend(trailing);

                    // Re-dispatch: if func is a symbol, check env for Val::Fn first,
                    // then try builtins, then dispatch.
                    match func {
                        Val::Sym(fname) => {
                            if let Some(Val::Fn {
                                arities,
                                env: captured_env,
                                ..
                            }) = env.get(fname)
                            {
                                let arities = arities.clone();
                                let captured_env = captured_env.clone();
                                return invoke_fn(
                                    &arities,
                                    &captured_env,
                                    &spread,
                                    dispatch,
                                    env.handler_stack.clone(),
                                )
                                .await;
                            }
                            if let Some(result) = eval_builtin(fname, &spread) {
                                return result;
                            }
                            return dispatch.call(fname, &spread).await;
                        }
                        Val::Fn {
                            arities,
                            env: captured_env,
                            ..
                        } => {
                            let arities = arities.clone();
                            let captured_env = captured_env.clone();
                            return invoke_fn(
                                &arities,
                                &captured_env,
                                &spread,
                                dispatch,
                                env.handler_stack.clone(),
                            )
                            .await;
                        }
                        other => {
                            return Err(error::type_mismatch(
                                "apply first arg",
                                "symbol or fn",
                                other,
                            ))
                        }
                    }
                }

                // --- Built-in: cell (captures Val::Cap bindings from scope) ---
                if head == "cell" {
                    let args = eval_args(raw_args, env, dispatch).await?;
                    let wasm = match args.first() {
                        Some(Val::Bytes(b)) => b.clone(),
                        Some(other) => {
                            return Err(error::type_mismatch(
                                "cell first arg (wasm)",
                                "bytes",
                                other,
                            ))
                        }
                        None => return Err(error::arity("cell", "1", 0)),
                    };
                    if args.len() > 1 {
                        return Err(error::arity("cell", "1", args.len()));
                    }
                    // Capture all Val::Cap bindings from the lexical environment.
                    let caps = env.collect_caps();
                    return Ok(Val::Cell { wasm, caps });
                }

                // --- Higher-order builtins (need env + dispatch for fn invocation) ---
                if head == "map" || head == "filter" || head == "reduce" {
                    let args = eval_args(raw_args, env, dispatch).await?;
                    return eval_hof(head, &args, env, dispatch).await;
                }

                // --- Built-in functions ---
                let args = eval_args(raw_args, env, dispatch).await?;
                if let Some(result) = eval_builtin(head, &args) {
                    return result;
                }

                // --- Generic path: eval args, then dispatch to host ---
                dispatch.call(head, &args).await
            }
            // Symbol lookup.
            Val::Sym(s) => match env.get(s) {
                Some(v) => Ok(v.clone()),
                None => Ok(Val::Sym(s.clone())),
            },
            // Self-evaluating forms.
            other => Ok(other.clone()),
        }
    })
}

// ---------------------------------------------------------------------------
// Shared perform dispatch — stack walk
// ---------------------------------------------------------------------------

/// Walk the handler stack (newest → oldest) looking for a frame whose target
/// matches `effect_target`. Write to that frame's slot and await the oneshot.
///
/// Used by both `Expr::Perform` and (in future) `Val::List` fallback dispatch.
async fn perform_dispatch(
    handler_stack: &effect::HandlerStack,
    effect_target: effect::EffectTarget,
    data: Val,
) -> Result<Val, Val> {
    // Walk stack in reverse (newest first) to find a matching handler.
    // Unified: both keyword and cap effects use EffectTarget::matches().
    let matching_ctx = {
        let stack = handler_stack.borrow();
        stack
            .iter()
            .rev()
            .find(|ctx| ctx.borrow().target.matches(&effect_target))
            .cloned()
    };

    match matching_ctx {
        Some(ctx) => {
            let (tx, rx) = oneshot::channel();
            ctx.borrow_mut().slot.borrow_mut().pending = Some((effect_target, data, tx));
            rx.await
        }
        None => {
            // No matching handler — propagate as unhandled effect.
            let effect_type = match &effect_target {
                effect::EffectTarget::Keyword(s) => s.clone(),
                effect::EffectTarget::Cap { name, .. } => format!("cap:{name}"),
            };
            Err(Val::Effect {
                effect_type,
                data: Box::new(data),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial dispatcher that records calls and returns nil.
    /// Uses RefCell for interior mutability (Dispatch takes &self).
    struct RecordingDispatch {
        calls: RefCell<Vec<(String, Vec<Val>)>>,
    }

    impl RecordingDispatch {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Dispatch for RecordingDispatch {
        fn call<'a>(
            &'a self,
            name: &'a str,
            args: &'a [Val],
        ) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
            self.calls
                .borrow_mut()
                .push((name.to_string(), args.to_vec()));
            Box::pin(core::future::ready(Ok(Val::Nil)))
        }
    }

    /// Helper to run an async eval in a blocking context.
    fn eval_blocking(expr: &Val, env: &mut Env, dispatch: &RecordingDispatch) -> Result<Val, Val> {
        // We can use a trivial executor since our futures are purely synchronous.
        pollster_eval(eval_toplevel(expr, env, dispatch))
    }

    /// Minimal single-future poll-to-completion (no tokio needed).
    fn pollster_eval<F: Future<Output = T>, T>(mut fut: F) -> T {
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn dummy_raw_waker() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(p: *const ()) -> RawWaker {
                RawWaker::new(p, &VTABLE)
            }
            const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
            RawWaker::new(core::ptr::null(), &VTABLE)
        }

        let waker = unsafe { Waker::from_raw(dummy_raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        // SAFETY: we never move the future after pinning.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        // Loop: the effect system's state machine uses wake_by_ref() + Pending
        // to re-schedule itself after handler resume. In single-threaded sync
        // context, just re-poll immediately.
        let mut polls = 0u32;
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(val) => return val,
                Poll::Pending => {
                    polls += 1;
                    if polls > 10_000 {
                        panic!("future stuck in Pending after 10000 polls — likely deadlock");
                    }
                    continue;
                }
            }
        }
    }

    // --- Env tests ---

    #[test]
    fn env_get_set() {
        let mut env = Env::new();
        assert!(env.get("x").is_none());
        env.set("x".into(), Val::Int(42));
        assert_eq!(env.get("x"), Some(&Val::Int(42)));
    }

    #[test]
    fn env_child_scope_shadows() {
        let mut env = Env::new();
        env.set("x".into(), Val::Int(1));
        env.push_frame();
        env.set("x".into(), Val::Int(2));
        assert_eq!(env.get("x"), Some(&Val::Int(2)));
        env.pop_frame();
        assert_eq!(env.get("x"), Some(&Val::Int(1)));
    }

    #[test]
    fn env_child_sees_parent() {
        let mut env = Env::new();
        env.set("x".into(), Val::Int(1));
        env.push_frame();
        assert_eq!(env.get("x"), Some(&Val::Int(1)));
        env.pop_frame();
    }

    #[test]
    fn env_pop_root_is_noop() {
        let mut env = Env::new();
        env.set("x".into(), Val::Int(1));
        env.pop_frame(); // should not panic or lose the root
        assert_eq!(env.get("x"), Some(&Val::Int(1)));
    }

    // --- eval tests ---

    #[test]
    fn eval_self_evaluating() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();

        assert_eq!(eval_blocking(&Val::Int(42), &mut env, &d), Ok(Val::Int(42)));
        assert_eq!(
            eval_blocking(&Val::Str("hi".into()), &mut env, &d),
            Ok(Val::Str("hi".into()))
        );
        assert_eq!(eval_blocking(&Val::Nil, &mut env, &d), Ok(Val::Nil));
        assert_eq!(
            eval_blocking(&Val::Bool(true), &mut env, &d),
            Ok(Val::Bool(true))
        );
        assert!(d.calls.borrow().is_empty());
    }

    #[test]
    fn eval_symbol_lookup() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(99));

        assert_eq!(
            eval_blocking(&Val::Sym("x".into()), &mut env, &d),
            Ok(Val::Int(99))
        );
        // Unbound symbols pass through
        assert_eq!(
            eval_blocking(&Val::Sym("unknown".into()), &mut env, &d),
            Ok(Val::Sym("unknown".into()))
        );
    }

    #[test]
    fn eval_empty_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_blocking(&Val::List(vec![]), &mut env, &d),
            Ok(Val::Nil)
        );
    }

    #[test]
    fn eval_dispatches_call() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();

        let expr = Val::List(vec![Val::Sym("host".into()), Val::Sym("id".into())]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Nil));
        assert_eq!(d.calls.borrow().len(), 1);
        assert_eq!(d.calls.borrow()[0].0, "host");
        assert_eq!(d.calls.borrow()[0].1, vec![Val::Sym("id".into())]);
    }

    #[test]
    fn eval_nested_list_evaluated_first() {
        let mut env = Env::new();

        // A dispatcher that returns Val::Bytes for "ipfs" and Val::Nil for "host".
        struct TestDispatch;
        impl Dispatch for TestDispatch {
            fn call<'a>(
                &'a self,
                name: &'a str,
                _args: &'a [Val],
            ) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
                let result = match name {
                    "ipfs" => Ok(Val::Bytes(vec![1, 2, 3])),
                    "host" => Ok(Val::Nil),
                    _ => Err(error::unbound_symbol(name, None)),
                };
                Box::pin(core::future::ready(result))
            }
        }

        let d = TestDispatch;
        // (host listen "chess" (ipfs cat "bin/x.wasm"))
        let expr = Val::List(vec![
            Val::Sym("host".into()),
            Val::Sym("listen".into()),
            Val::Str("chess".into()),
            Val::List(vec![
                Val::Sym("ipfs".into()),
                Val::Sym("cat".into()),
                Val::Str("bin/x.wasm".into()),
            ]),
        ]);
        let result = pollster_eval(eval(&expr, &mut env, &d));
        assert_eq!(result, Ok(Val::Nil));
    }

    #[test]
    fn eval_non_symbol_head_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let expr = Val::List(vec![Val::Int(42)]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert!(result.is_err());
    }

    // --- Env: set_root + snapshot ---

    #[test]
    fn env_set_root_writes_outermost() {
        let mut env = Env::new();
        env.push_frame();
        env.set_root("x".into(), Val::Int(42));
        env.pop_frame();
        // x should still be visible in the root frame
        assert_eq!(env.get("x"), Some(&Val::Int(42)));
    }

    #[test]
    fn env_snapshot_merges_frames() {
        let mut env = Env::new();
        env.set("x".into(), Val::Int(1));
        env.set("y".into(), Val::Int(2));
        env.push_frame();
        env.set("x".into(), Val::Int(10)); // shadow x
        env.set("z".into(), Val::Int(3));

        let snap = env.snapshot();
        assert_eq!(snap.get("x"), Some(&Val::Int(10))); // inner wins
        assert_eq!(snap.get("y"), Some(&Val::Int(2))); // from outer
        assert_eq!(snap.get("z"), Some(&Val::Int(3))); // from inner
    }

    // --- def ---

    #[test]
    fn def_binds_in_root() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def x 42)
        let expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("x".into()),
            Val::Int(42),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Int(42)));
        assert_eq!(env.get("x"), Some(&Val::Int(42)));
    }

    #[test]
    fn def_evals_value() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def x (do 1 2 3))
        let expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("x".into()),
            Val::List(vec![
                Val::Sym("do".into()),
                Val::Int(1),
                Val::Int(2),
                Val::Int(3),
            ]),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Int(3)));
        assert_eq!(env.get("x"), Some(&Val::Int(3)));
    }

    #[test]
    fn def_non_symbol_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def 42 "oops")
        let expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Int(42),
            Val::Str("oops".into()),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(&result.unwrap_err(), "def"));
    }

    #[test]
    fn def_inside_let_writes_root() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [a 1] (def b 2))
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("a".into()), Val::Int(1)]),
            Val::List(vec![
                Val::Sym("def".into()),
                Val::Sym("b".into()),
                Val::Int(2),
            ]),
        ]);
        eval_blocking(&expr, &mut env, &d).unwrap();
        // b should be visible at root level (not just inside let)
        assert_eq!(env.get("b"), Some(&Val::Int(2)));
    }

    // --- if ---

    #[test]
    fn if_true_branch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if true "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(true),
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("yes".into()))
        );
    }

    #[test]
    fn if_false_branch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if false "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(false),
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("no".into()))
        );
    }

    #[test]
    fn if_nil_is_falsy() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if nil "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Nil,
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("no".into()))
        );
    }

    #[test]
    fn if_zero_is_truthy() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if 0 "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Int(0),
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("yes".into()))
        );
    }

    #[test]
    fn if_empty_string_truthy() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if "" "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Str("".into()),
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("yes".into()))
        );
    }

    #[test]
    fn if_no_else_returns_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if false "yes")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(false),
            Val::Str("yes".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn if_wrong_arg_count() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if)
        let expr = Val::List(vec![Val::Sym("if".into())]);
        assert!(eval_blocking(&expr, &mut env, &d).is_err());
        // (if a b c d)
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(true),
            Val::Int(1),
            Val::Int(2),
            Val::Int(3),
        ]);
        assert!(eval_blocking(&expr, &mut env, &d).is_err());
    }

    #[test]
    fn if_only_evals_taken_branch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if true (host "taken") (host "not-taken"))
        // Only "taken" branch should dispatch; "not-taken" should NOT.
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(true),
            Val::List(vec![Val::Sym("host".into()), Val::Str("taken".into())]),
            Val::List(vec![Val::Sym("host".into()), Val::Str("not-taken".into())]),
        ]);
        eval_blocking(&expr, &mut env, &d).unwrap();
        assert_eq!(d.calls.borrow().len(), 1);
        assert_eq!(d.calls.borrow()[0].1, vec![Val::Str("taken".into())]);
    }

    // --- do ---

    #[test]
    fn do_returns_last() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (do 1 2 3)
        let expr = Val::List(vec![
            Val::Sym("do".into()),
            Val::Int(1),
            Val::Int(2),
            Val::Int(3),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(3)));
    }

    #[test]
    fn do_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (do)
        let expr = Val::List(vec![Val::Sym("do".into())]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn do_single() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (do 42)
        let expr = Val::List(vec![Val::Sym("do".into()), Val::Int(42)]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(42)));
    }

    // --- let ---

    #[test]
    fn let_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [x 1] x)
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Int(1)]),
            Val::Sym("x".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn let_shadow() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        // (let [x 2] x)
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Int(2)]),
            Val::Sym("x".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(2)));
        // After let, x should be back to 1
        assert_eq!(env.get("x"), Some(&Val::Int(1)));
    }

    #[test]
    fn let_sequential_binding() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [x 1 y x] y) — y sees x from earlier binding
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![
                Val::Sym("x".into()),
                Val::Int(1),
                Val::Sym("y".into()),
                Val::Sym("x".into()),
            ]),
            Val::Sym("y".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn let_implicit_do() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [x 1] 10 20 x) — multiple body forms, returns last
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Int(1)]),
            Val::Int(10),
            Val::Int(20),
            Val::Sym("x".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn let_odd_bindings_error() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [x] x) — odd number of binding forms
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("x".into())]),
            Val::Sym("x".into()),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(&result.unwrap_err(), "pairs"));
    }

    #[test]
    fn let_non_vector_error() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let (x 1) x) — list instead of vector
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::List(vec![Val::Sym("x".into()), Val::Int(1)]),
            Val::Sym("x".into()),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(&result.unwrap_err(), "vector"));
    }

    // --- quote ---

    #[test]
    fn quote_symbol() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(99));
        // (quote x) — should NOT look up x
        let expr = Val::List(vec![Val::Sym("quote".into()), Val::Sym("x".into())]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Sym("x".into())));
    }

    #[test]
    fn quote_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (quote (+ 1 2)) — should NOT evaluate the list
        let inner = Val::List(vec![Val::Sym("+".into()), Val::Int(1), Val::Int(2)]);
        let expr = Val::List(vec![Val::Sym("quote".into()), inner.clone()]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(inner));
        assert!(d.calls.borrow().is_empty()); // no dispatch happened
    }

    // --- fn ---

    #[test]
    fn fn_single_arity_call() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn [x] x))
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![Val::Sym("x".into())]),
                Val::Sym("x".into()),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        // (f 42)
        let call_expr = Val::List(vec![Val::Sym("f".into()), Val::Int(42)]);
        let result = eval_blocking(&call_expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Int(42)));
    }

    #[test]
    fn fn_multi_arity() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn ([x] x) ([x y] y)))
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::List(vec![
                    Val::Vector(vec![Val::Sym("x".into())]),
                    Val::Sym("x".into()),
                ]),
                Val::List(vec![
                    Val::Vector(vec![Val::Sym("x".into()), Val::Sym("y".into())]),
                    Val::Sym("y".into()),
                ]),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        // (f 1) → 1
        let call1 = Val::List(vec![Val::Sym("f".into()), Val::Int(1)]);
        assert_eq!(eval_blocking(&call1, &mut env, &d), Ok(Val::Int(1)));
        // (f 1 2) → 2
        let call2 = Val::List(vec![Val::Sym("f".into()), Val::Int(1), Val::Int(2)]);
        assert_eq!(eval_blocking(&call2, &mut env, &d), Ok(Val::Int(2)));
    }

    #[test]
    fn fn_variadic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn [x & rest] rest))
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![
                    Val::Sym("x".into()),
                    Val::Sym("&".into()),
                    Val::Sym("rest".into()),
                ]),
                Val::Sym("rest".into()),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        // (f 1 2 3) → (2 3)
        let call = Val::List(vec![
            Val::Sym("f".into()),
            Val::Int(1),
            Val::Int(2),
            Val::Int(3),
        ]);
        assert_eq!(
            eval_blocking(&call, &mut env, &d),
            Ok(Val::List(vec![Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn fn_closure_captures_env() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def x 10)
        let def_x = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("x".into()),
            Val::Int(10),
        ]);
        eval_blocking(&def_x, &mut env, &d).unwrap();
        // (def f (fn [] x))
        let def_f = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![]),
                Val::Sym("x".into()),
            ]),
        ]);
        eval_blocking(&def_f, &mut env, &d).unwrap();
        // (def x 20) — rebind x
        let def_x2 = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("x".into()),
            Val::Int(20),
        ]);
        eval_blocking(&def_x2, &mut env, &d).unwrap();
        // (f) → 10, not 20 (captured at definition time)
        let call = Val::List(vec![Val::Sym("f".into())]);
        assert_eq!(eval_blocking(&call, &mut env, &d), Ok(Val::Int(10)));
    }

    #[test]
    fn fn_closure_empty_free_vars_captures_nothing() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();

        let closure = eval_str("(fn [] 1)", &mut env, &d).unwrap();
        let Val::Fn {
            env: captured_env, ..
        } = closure
        else {
            panic!("expected function");
        };
        assert_eq!(captured_env.bindings().len(), 0);
    }

    #[test]
    fn fn_closure_single_capture_is_slim() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(7));
        env.set("y".into(), Val::Int(9));

        let closure = eval_str("(fn [] x)", &mut env, &d).unwrap();
        let Val::Fn {
            env: captured_env, ..
        } = closure
        else {
            panic!("expected function");
        };
        assert_eq!(captured_env.bindings().len(), 1);
        assert_eq!(captured_env.get("x"), Some(&Val::Int(7)));
        assert!(captured_env.get("y").is_none());
    }

    #[test]
    fn fn_closure_multi_arity_union_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        env.set("y".into(), Val::Int(2));
        env.set("z".into(), Val::Int(3));

        let closure = eval_str("(fn ([a] x) ([a b] y))", &mut env, &d).unwrap();
        let Val::Fn {
            env: captured_env, ..
        } = closure
        else {
            panic!("expected function");
        };
        assert_eq!(captured_env.bindings().len(), 2);
        assert_eq!(captured_env.get("x"), Some(&Val::Int(1)));
        assert_eq!(captured_env.get("y"), Some(&Val::Int(2)));
        assert!(captured_env.get("z").is_none());
    }

    #[test]
    fn fn_closure_over_closure_still_works_with_slim_env() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let inner = eval_str("(let [x 42 outer (fn [] (fn [] x))] (outer))", &mut env, &d).unwrap();
        let Val::Fn {
            env: captured_env, ..
        } = &inner
        else {
            panic!("expected function");
        };
        assert_eq!(captured_env.bindings().len(), 1);
        assert_eq!(captured_env.get("x"), Some(&Val::Int(42)));
        env.set("inner".into(), inner);
        assert_eq!(
            eval_str("(inner)", &mut env, &d),
            Ok(Val::Int(42)),
            "nested closure should remain functional"
        );
    }

    #[test]
    fn fn_closure_identity_preservation_with_slim_envs() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();

        let f = eval_str("(fn [] 1)", &mut env, &d).unwrap();
        let f_clone = f.clone();
        let g = eval_str("(fn [] 1)", &mut env, &d).unwrap();

        assert_eq!(f, f_clone, "cloned closure should preserve Rc identity");
        assert_ne!(f, g, "separate evaluations should produce distinct Rc envs");
    }

    #[test]
    fn raw_fn_path_keeps_full_snapshot() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        env.set("y".into(), Val::Int(2));

        let raw = Val::List(vec![
            Val::Sym("fn".into()),
            Val::Vector(vec![]),
            Val::Sym("x".into()),
        ]);
        let closure = pollster_eval(eval(&raw, &mut env, &d)).unwrap();
        let Val::Fn {
            env: captured_env, ..
        } = closure
        else {
            panic!("expected function");
        };
        assert_eq!(captured_env.get("x"), Some(&Val::Int(1)));
        assert_eq!(captured_env.get("y"), Some(&Val::Int(2)));
    }

    fn fn_cap_status(value: Val) -> (bool, Option<String>) {
        let Val::Fn {
            is_cap_free,
            cap_violation,
            ..
        } = value
        else {
            panic!("expected fn value");
        };
        (is_cap_free, cap_violation)
    }

    fn macro_cap_status(value: Val) -> (bool, Option<String>) {
        let Val::Macro {
            is_cap_free,
            cap_violation,
            ..
        } = value
        else {
            panic!("expected macro value");
        };
        (is_cap_free, cap_violation)
    }

    #[test]
    fn fn_cap_status_no_captures() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let status = fn_cap_status(eval_str("(fn [] 1)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_int_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        let status = fn_cap_status(eval_str("(fn [] x)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_cap_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        let status = fn_cap_status(eval_str("(fn [] db)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn fn_cap_status_cell_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "c".into(),
            Val::Cell {
                wasm: vec![],
                caps: vec![],
            },
        );
        let status = fn_cap_status(eval_str("(fn [] c)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("c".into())));
    }

    #[test]
    fn fn_cap_status_native_fn_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "n".into(),
            Val::NativeFn {
                name: "n".into(),
                func: Rc::new(|_| Ok(Val::Nil)),
            },
        );
        let status = fn_cap_status(eval_str("(fn [] n)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("n".into())));
    }

    #[test]
    fn fn_cap_status_capture_cap_free_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let helper = eval_str("(fn [] 1)", &mut env, &d).unwrap();
        env.set("helper".into(), helper);
        let status = fn_cap_status(eval_str("(fn [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_capture_cap_bearing_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        let helper = eval_str("(fn [] db)", &mut env, &d).unwrap();
        env.set("helper".into(), helper);
        let status = fn_cap_status(eval_str("(fn [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("helper".into())));
    }

    #[test]
    fn fn_cap_status_capture_cap_free_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(defmacro m [] 42)", &mut env, &d).unwrap();
        let status = fn_cap_status(eval_str("(fn [] m)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_capture_cap_bearing_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        eval_str("(defmacro m [] db)", &mut env, &d).unwrap();
        let status = fn_cap_status(eval_str("(fn [] m)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("m".into())));
    }

    #[test]
    fn fn_cap_violation_is_deterministic_by_binding_name() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("z_cap".into(), make_cap("z", "cid:z", Rc::new(())));
        env.set("a_cap".into(), make_cap("a", "cid:a", Rc::new(())));
        let status = fn_cap_status(eval_str("(fn [] (list z_cap a_cap))", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("a_cap".into())));
    }

    #[test]
    fn macro_cap_status_no_captures() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let status = macro_cap_status(eval_str("(defmacro m [] 1)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_int_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        let status = macro_cap_status(eval_str("(defmacro m [] x)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_cap_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        let status = macro_cap_status(eval_str("(defmacro m [] db)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn macro_cap_status_cell_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "c".into(),
            Val::Cell {
                wasm: vec![],
                caps: vec![],
            },
        );
        let status = macro_cap_status(eval_str("(defmacro m [] c)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("c".into())));
    }

    #[test]
    fn macro_cap_status_native_fn_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "n".into(),
            Val::NativeFn {
                name: "n".into(),
                func: Rc::new(|_| Ok(Val::Nil)),
            },
        );
        let status = macro_cap_status(eval_str("(defmacro m [] n)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("n".into())));
    }

    #[test]
    fn macro_cap_status_capture_cap_free_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let helper = eval_str("(fn [] 1)", &mut env, &d).unwrap();
        env.set("helper".into(), helper);
        let status = macro_cap_status(eval_str("(defmacro m [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_capture_cap_bearing_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        let helper = eval_str("(fn [] db)", &mut env, &d).unwrap();
        env.set("helper".into(), helper);
        let status = macro_cap_status(eval_str("(defmacro m [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn macro_cap_status_capture_cap_free_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(defmacro helper [] 42)", &mut env, &d).unwrap();
        let status = macro_cap_status(eval_str("(defmacro m [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_capture_cap_bearing_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        eval_str("(defmacro helper [] db)", &mut env, &d).unwrap();
        let status = macro_cap_status(eval_str("(defmacro m [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn closure_hash_and_eq_ignore_cap_status_fields() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let shared = Rc::new(Env::new());
        let left = Val::Fn {
            arities: vec![],
            env: shared.clone(),
            is_cap_free: true,
            cap_violation: None,
        };
        let right = Val::Fn {
            arities: vec![],
            env: shared,
            is_cap_free: false,
            cap_violation: Some("db".into()),
        };

        assert_eq!(left, right);
        let mut lh = DefaultHasher::new();
        left.hash(&mut lh);
        let mut rh = DefaultHasher::new();
        right.hash(&mut rh);
        assert_eq!(lh.finish(), rh.finish());
    }

    #[test]
    fn raw_fn_path_cap_status_scans_full_snapshot() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        env.set("x".into(), Val::Int(1));

        let raw = Val::List(vec![
            Val::Sym("fn".into()),
            Val::Vector(vec![]),
            Val::Sym("x".into()),
        ]);
        let status = fn_cap_status(pollster_eval(eval(&raw, &mut env, &d)).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn fn_arity_mismatch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn [x y] x))
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![Val::Sym("x".into()), Val::Sym("y".into())]),
                Val::Sym("x".into()),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        // (f 1) — wrong arity
        let call = Val::List(vec![Val::Sym("f".into()), Val::Int(1)]);
        let err = eval_blocking(&call, &mut env, &d).unwrap_err();
        assert_eq!(error::type_tag(&err), Some(error::tag::ARITY), "got: {err}");
    }

    #[test]
    fn fn_duplicate_arity_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (fn ([x] x) ([y] y)) — two 1-arg arities
        let expr = Val::List(vec![
            Val::Sym("fn".into()),
            Val::List(vec![
                Val::Vector(vec![Val::Sym("x".into())]),
                Val::Sym("x".into()),
            ]),
            Val::List(vec![
                Val::Vector(vec![Val::Sym("y".into())]),
                Val::Sym("y".into()),
            ]),
        ]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert!(err_contains(&err, "duplicate arity"), "got: {err}");
    }

    #[test]
    fn fn_implicit_do_body() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn [x] 1 2 x)) — body has multiple forms, returns last
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![Val::Sym("x".into())]),
                Val::Int(1),
                Val::Int(2),
                Val::Sym("x".into()),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        let call = Val::List(vec![Val::Sym("f".into()), Val::Int(99)]);
        assert_eq!(eval_blocking(&call, &mut env, &d), Ok(Val::Int(99)));
    }

    #[test]
    fn fn_no_params_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (fn) — no params at all
        let expr = Val::List(vec![Val::Sym("fn".into())]);
        assert!(eval_blocking(&expr, &mut env, &d).is_err());
    }

    // --- loop / recur ---

    #[test]
    fn loop_returns_non_recur() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [x 42] x)
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Int(42)]),
            Val::Sym("x".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(42)));
    }

    #[test]
    fn loop_recur_once() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [x true] (if x (recur false) "done"))
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Bool(true)]),
            Val::List(vec![
                Val::Sym("if".into()),
                Val::Sym("x".into()),
                Val::List(vec![Val::Sym("recur".into()), Val::Bool(false)]),
                Val::Str("done".into()),
            ]),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("done".into()))
        );
    }

    #[test]
    fn loop_recur_multiple_bindings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [a 1 b 2] (if a (recur false 3) b))
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![
                Val::Sym("a".into()),
                Val::Int(1),
                Val::Sym("b".into()),
                Val::Int(2),
            ]),
            Val::List(vec![
                Val::Sym("if".into()),
                Val::Sym("a".into()),
                Val::List(vec![
                    Val::Sym("recur".into()),
                    Val::Bool(false),
                    Val::Int(3),
                ]),
                Val::Sym("b".into()),
            ]),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(3)));
    }

    #[test]
    fn loop_sequential_bindings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [a 1 b a] b) — b sees a=1
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![
                Val::Sym("a".into()),
                Val::Int(1),
                Val::Sym("b".into()),
                Val::Sym("a".into()),
            ]),
            Val::Sym("b".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn recur_wrong_arity() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [x 1 y 2] (recur 3))
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![
                Val::Sym("x".into()),
                Val::Int(1),
                Val::Sym("y".into()),
                Val::Int(2),
            ]),
            Val::List(vec![Val::Sym("recur".into()), Val::Int(3)]),
        ]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert_eq!(error::type_tag(&err), Some(error::tag::ARITY), "got: {err}");
    }

    #[test]
    fn recur_outside_loop() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (recur 1) at top level
        let expr = Val::List(vec![Val::Sym("recur".into()), Val::Int(1)]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        // Top-level recur is an internal-tagged error referencing recur.
        assert_eq!(
            error::type_tag(&err),
            Some(error::tag::INTERNAL),
            "got: {err}"
        );
        assert!(
            error::message(&err).unwrap().contains("recur"),
            "got: {err}"
        );
    }

    #[test]
    fn loop_non_vector_bindings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop (x 1) x) — list instead of vector
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::List(vec![Val::Sym("x".into()), Val::Int(1)]),
            Val::Sym("x".into()),
        ]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert!(err_contains(&err, "vector"), "got: {err}");
    }

    #[test]
    fn loop_odd_bindings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [x] x) — odd number of binding forms
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![Val::Sym("x".into())]),
            Val::Sym("x".into()),
        ]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert!(err_contains(&err, "pairs"), "got: {err}");
    }

    // =========================================================================
    // Built-in function tests
    // =========================================================================

    /// Helper: parse + eval a string expression.
    fn eval_str(input: &str, env: &mut Env, d: &RecordingDispatch) -> Result<Val, Val> {
        let expr = crate::read(input).map_err(|e| error::parse(None, e.to_string()))?;
        eval_blocking(&expr, env, d)
    }

    /// Check if an error Val contains a substring in its :message field or Display output.
    fn err_contains(err: &Val, needle: &str) -> bool {
        // Check :message field in map
        if let Val::Map(m) = err {
            for (k, v) in m.iter() {
                if let (Val::Keyword(key), Val::Str(msg)) = (k, v) {
                    if key == "message" && msg.contains(needle) {
                        return true;
                    }
                }
            }
        }
        // Fallback: check Display output
        format!("{err}").contains(needle)
    }

    // --- list ---

    #[test]
    fn builtin_list_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(list)", &mut env, &d), Ok(Val::List(vec![])));
    }

    #[test]
    fn builtin_list_with_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(list 1 2 3)", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    // --- cons ---

    #[test]
    fn builtin_cons_onto_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(cons 1 (list 2 3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_cons_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(cons 1)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_cons_non_collection() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(cons 1 2)", &mut env, &d).is_err());
    }

    // --- first ---

    #[test]
    fn builtin_first_of_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(first (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(1))
        );
    }

    #[test]
    fn builtin_first_of_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(first (list))", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn builtin_first_of_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(first nil)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn builtin_first_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(first 42)", &mut env, &d).is_err());
    }

    // --- rest ---

    #[test]
    fn builtin_rest_of_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(rest (list 1 2 3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_rest_of_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(rest (list))", &mut env, &d),
            Ok(Val::List(vec![]))
        );
    }

    #[test]
    fn builtin_rest_of_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(rest nil)", &mut env, &d), Ok(Val::List(vec![])));
    }

    #[test]
    fn builtin_rest_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(rest 42)", &mut env, &d).is_err());
    }

    // --- count ---

    #[test]
    fn builtin_count_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(count (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn builtin_count_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(count nil)", &mut env, &d), Ok(Val::Int(0)));
    }

    #[test]
    fn builtin_count_string_chars() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Unicode: each emoji is one char
        assert_eq!(
            eval_str(r#"(count "hello")"#, &mut env, &d),
            Ok(Val::Int(5))
        );
    }

    #[test]
    fn builtin_count_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(count 42)", &mut env, &d).is_err());
    }

    // --- vec ---

    #[test]
    fn builtin_vec_from_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(vec (list 1 2))", &mut env, &d),
            Ok(Val::Vector(vec![Val::Int(1), Val::Int(2)]))
        );
    }

    #[test]
    fn builtin_vec_from_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(vec nil)", &mut env, &d), Ok(Val::Vector(vec![])));
    }

    #[test]
    fn builtin_vec_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(vec 42)", &mut env, &d).is_err());
    }

    // --- get ---

    #[test]
    fn builtin_get_map() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(get {:a 1 :b 2} :b)", &mut env, &d),
            Ok(Val::Int(2))
        );
    }

    #[test]
    fn builtin_get_map_missing() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(get {:a 1} :z)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn builtin_get_vector() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(get [10 20 30] 1)", &mut env, &d),
            Ok(Val::Int(20))
        );
    }

    #[test]
    fn builtin_get_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(get nil :a)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn builtin_get_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(get 42 0)", &mut env, &d).is_err());
    }

    // --- assoc ---

    #[test]
    fn builtin_assoc_add_key() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(assoc {:a 1} :b 2)", &mut env, &d),
            Ok(Val::Map(ValMap::from_pairs(vec![
                (Val::Keyword("a".into()), Val::Int(1)),
                (Val::Keyword("b".into()), Val::Int(2)),
            ])))
        );
    }

    #[test]
    fn builtin_assoc_update_key() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(assoc {:a 1} :a 99)", &mut env, &d),
            Ok(Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("a".into()),
                Val::Int(99)
            )])))
        );
    }

    #[test]
    fn builtin_assoc_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Even number of args (map + 1 key, no value)
        assert!(eval_str("(assoc {:a 1} :b)", &mut env, &d).is_err());
    }

    // --- conj ---

    #[test]
    fn builtin_conj_vector_appends() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(conj [1 2] 3)", &mut env, &d),
            Ok(Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_conj_list_prepends() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(conj (list 2 3) 1)", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_conj_map() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(conj {:a 1} [:b 2])", &mut env, &d),
            Ok(Val::Map(ValMap::from_pairs(vec![
                (Val::Keyword("a".into()), Val::Int(1)),
                (Val::Keyword("b".into()), Val::Int(2)),
            ])))
        );
    }

    #[test]
    fn builtin_conj_too_few_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(conj [1])", &mut env, &d).is_err());
    }

    // --- Arithmetic ---

    #[test]
    fn builtin_add_ints() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(+ 1 2 3)", &mut env, &d), Ok(Val::Int(6)));
    }

    #[test]
    fn builtin_add_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(+)", &mut env, &d), Ok(Val::Int(0)));
    }

    #[test]
    fn builtin_add_float_promotion() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(+ 1 2.0)", &mut env, &d), Ok(Val::Float(3.0)));
    }

    #[test]
    fn builtin_add_non_number() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str(r#"(+ 1 "a")"#, &mut env, &d).is_err());
    }

    #[test]
    fn builtin_sub_two() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(- 10 3)", &mut env, &d), Ok(Val::Int(7)));
    }

    #[test]
    fn builtin_sub_negate() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(- 5)", &mut env, &d), Ok(Val::Int(-5)));
    }

    #[test]
    fn builtin_sub_empty_error() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(-)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_mul_ints() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(* 2 3 4)", &mut env, &d), Ok(Val::Int(24)));
    }

    #[test]
    fn builtin_mul_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(*)", &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn builtin_div_ints() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(/ 10 3)", &mut env, &d), Ok(Val::Int(3)));
    }

    #[test]
    fn builtin_div_by_zero() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(/ 10 0)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_div_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(/ 1)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_mod_ints() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(mod 10 3)", &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn builtin_mod_by_zero() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(mod 10 0)", &mut env, &d).is_err());
    }

    // --- Comparison ---

    #[test]
    fn builtin_eq_true() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(= 1 1)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_eq_false() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(= 1 2)", &mut env, &d), Ok(Val::Bool(false)));
    }

    #[test]
    fn builtin_eq_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(= 1)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_lt_true() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(< 1 2)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_lt_false() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(< 2 1)", &mut env, &d), Ok(Val::Bool(false)));
    }

    #[test]
    fn builtin_gt_true() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(> 2 1)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_le_equal() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(<= 2 2)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_ge_equal() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(>= 2 2)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_comparison_mixed_numeric() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(< 1 2.5)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_comparison_non_number() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str(r#"(< 1 "a")"#, &mut env, &d).is_err());
    }

    // --- gensym ---

    #[test]
    fn builtin_gensym() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let r1 = eval_str("(gensym)", &mut env, &d).unwrap();
        let r2 = eval_str("(gensym)", &mut env, &d).unwrap();
        // Each gensym returns a unique symbol
        match (&r1, &r2) {
            (Val::Sym(s1), Val::Sym(s2)) => {
                assert!(s1.starts_with("G__"));
                assert!(s2.starts_with("G__"));
                assert_ne!(s1, s2);
            }
            _ => panic!("gensym should return Sym, got {r1} and {r2}"),
        }
    }

    #[test]
    fn builtin_gensym_no_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(gensym 1)", &mut env, &d).is_err());
    }

    // --- apply ---

    #[test]
    fn builtin_apply_builtin_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(apply + (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(6))
        );
    }

    #[test]
    fn builtin_apply_user_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def f (fn [x y] (+ x y)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(apply f (list 3 4))", &mut env, &d),
            Ok(Val::Int(7))
        );
    }

    #[test]
    fn builtin_apply_with_middle_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (apply + 1 2 (list 3)) → (+ 1 2 3) → 6
        assert_eq!(
            eval_str("(apply + 1 2 (list 3))", &mut env, &d),
            Ok(Val::Int(6))
        );
    }

    #[test]
    fn builtin_apply_too_few_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(apply +)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_apply_non_collection_last() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(apply + 1 2)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_apply_fn_value() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // apply with a fn value (not symbol)
        eval_str("(def f (fn [x] (+ x 1)))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(apply f [10])", &mut env, &d), Ok(Val::Int(11)));
    }

    // --- Integration: builtins with special forms ---

    #[test]
    fn builtin_in_let() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(let [x (+ 1 2)] (* x 10))", &mut env, &d),
            Ok(Val::Int(30))
        );
    }

    #[test]
    fn builtin_in_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(add 3 4)", &mut env, &d), Ok(Val::Int(7)));
    }

    #[test]
    fn builtin_nested() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(+ (* 2 3) (- 10 4))", &mut env, &d),
            Ok(Val::Int(12))
        );
    }

    // =========================================================================
    // defmacro tests
    // =========================================================================

    #[test]
    fn defmacro_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Define a macro that returns a constant form
        eval_str("(defmacro m [] 42)", &mut env, &d).unwrap();
        assert_eq!(eval_str("(m)", &mut env, &d), Ok(Val::Int(42)));
    }

    #[test]
    fn defmacro_receives_unevaluated_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Macro that receives a form and quotes it (returns it without eval)
        // (defmacro identity-form [x] x) — returns the raw form
        eval_str("(defmacro identity-form [x] x)", &mut env, &d).unwrap();
        // (identity-form 42) → eval(42) → 42
        assert_eq!(
            eval_str("(identity-form 42)", &mut env, &d),
            Ok(Val::Int(42))
        );
    }

    #[test]
    fn defmacro_expansion_is_re_evaluated() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Macro that constructs a (+ 1 2) form using list and quote
        eval_str(r#"(defmacro add12 [] (list (quote +) 1 2))"#, &mut env, &d).unwrap();
        // (add12) → expands to (+ 1 2) → evaluates to 3
        assert_eq!(eval_str("(add12)", &mut env, &d), Ok(Val::Int(3)));
    }

    #[test]
    fn defmacro_stored_in_root() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Define macro inside a let — should still be in root
        eval_str("(let [x 1] (defmacro m [] 99))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(m)", &mut env, &d), Ok(Val::Int(99)));
    }

    #[test]
    fn defmacro_no_name_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(defmacro)", &mut env, &d).is_err());
    }

    #[test]
    fn defmacro_no_params_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(defmacro m)", &mut env, &d).is_err());
    }

    #[test]
    fn defmacro_non_symbol_name_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(defmacro 42 [] nil)", &mut env, &d).is_err());
    }

    #[test]
    fn defmacro_variadic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Macro with variadic args — wraps everything in a list call
        eval_str(
            "(defmacro wrap [& forms] (cons (quote list) forms))",
            &mut env,
            &d,
        )
        .unwrap();
        // (wrap 1 2 3) → expands to (list 1 2 3) → (1 2 3)
        assert_eq!(
            eval_str("(wrap 1 2 3)", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    // --- Integration: defmacro + builtins ---

    #[test]
    fn defmacro_uses_builtins_to_construct_forms() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // A "when" macro: (when test body...) → (if test (do body...) nil)
        eval_str(
            r#"(defmacro when [test & body]
                (list (quote if) test (cons (quote do) body) nil))"#,
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            eval_str("(when true (+ 1 2))", &mut env, &d),
            Ok(Val::Int(3))
        );
        assert_eq!(eval_str("(when false (+ 1 2))", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn defmacro_unless_integration() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (unless test body...) → (if test nil (do body...))
        eval_str(
            r#"(defmacro unless [test & body]
                (list (quote if) test nil (cons (quote do) body)))"#,
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            eval_str("(unless false 42)", &mut env, &d),
            Ok(Val::Int(42))
        );
        assert_eq!(eval_str("(unless true 42)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn defmacro_with_gensym() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Macro that uses gensym to avoid name collisions
        // This just tests that gensym can be called from a macro body
        eval_str("(defmacro test-gensym [] (do (gensym) 42))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(test-gensym)", &mut env, &d), Ok(Val::Int(42)));
    }

    // --- concat builtin tests ---

    #[test]
    fn builtin_concat_two_lists() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(concat (list 1 2) (list 3 4))", &mut env, &d),
            Ok(Val::List(vec![
                Val::Int(1),
                Val::Int(2),
                Val::Int(3),
                Val::Int(4),
            ]))
        );
    }

    #[test]
    fn builtin_concat_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(concat)", &mut env, &d), Ok(Val::List(vec![])));
    }

    #[test]
    fn builtin_concat_with_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(concat (list 1) nil (list 2))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2)]))
        );
    }

    #[test]
    fn builtin_concat_with_vector() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(concat [1 2] (list 3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_concat_non_seq_error() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(concat 42)", &mut env, &d).is_err());
    }

    // --- Syntax-quote integration tests ---

    #[test]
    fn syntax_quote_when_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str(
            "(defmacro when [test & body] `(if ~test (do ~@body) nil))",
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(eval_str("(when true 1 2 3)", &mut env, &d), Ok(Val::Int(3)));
        assert_eq!(eval_str("(when false 1 2 3)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn syntax_quote_simple_expansion() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Syntax-quote in a let produces a data structure
        assert_eq!(
            eval_str("(let [x 42] `(+ ~x 1))", &mut env, &d),
            Ok(Val::List(vec![
                Val::Sym("+".into()),
                Val::Int(42),
                Val::Int(1),
            ]))
        );
    }

    #[test]
    fn syntax_quote_splice_expansion() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(let [xs (list 1 2 3)] `(+ ~@xs))", &mut env, &d,),
            Ok(Val::List(vec![
                Val::Sym("+".into()),
                Val::Int(1),
                Val::Int(2),
                Val::Int(3),
            ]))
        );
    }

    #[test]
    fn syntax_quote_unless_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str(
            "(defmacro unless [test & body] `(if ~test nil (do ~@body)))",
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            eval_str("(unless false 1 2 3)", &mut env, &d),
            Ok(Val::Int(3))
        );
        assert_eq!(eval_str("(unless true 1 2 3)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn syntax_quote_preserves_keywords() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Keywords are self-evaluating — should pass through syntax-quote
        assert_eq!(
            eval_str("`(:a ~(+ 1 2))", &mut env, &d),
            Ok(Val::List(vec![Val::Keyword("a".into()), Val::Int(3)]))
        );
    }

    #[test]
    fn unquote_outside_syntax_quote_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(unquote x)", &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(
            &result.unwrap_err(),
            "not inside syntax-quote"
        ));
    }

    #[test]
    fn splice_unquote_outside_syntax_quote_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(splice-unquote x)", &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(
            &result.unwrap_err(),
            "not inside syntax-quote"
        ));
    }

    // Prelude tests
    // =========================================================================

    /// Helper: load the prelude then parse + eval a string expression.
    fn prelude_eval(input: &str) -> Result<Val, Val> {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Load prelude forms into the environment
        let prelude_forms =
            crate::read_many(crate::PRELUDE).map_err(|e| format!("prelude parse: {e}"))?;
        for form in &prelude_forms {
            eval_blocking(form, &mut env, &d)?;
        }
        // Now eval the test expression
        eval_str(input, &mut env, &d)
    }

    #[test]
    fn prelude_not_true() {
        assert_eq!(prelude_eval("(not true)"), Ok(Val::Bool(false)));
    }

    #[test]
    fn prelude_not_false() {
        assert_eq!(prelude_eval("(not false)"), Ok(Val::Bool(true)));
    }

    #[test]
    fn prelude_not_nil() {
        assert_eq!(prelude_eval("(not nil)"), Ok(Val::Bool(true)));
    }

    #[test]
    fn prelude_not_truthy() {
        // Non-nil, non-false values are truthy → not returns false
        assert_eq!(prelude_eval("(not 42)"), Ok(Val::Bool(false)));
    }

    #[test]
    fn prelude_when_true() {
        assert_eq!(prelude_eval("(when true 1 2 3)"), Ok(Val::Int(3)));
    }

    #[test]
    fn prelude_when_false() {
        assert_eq!(prelude_eval("(when false 1 2 3)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_when_not_false() {
        assert_eq!(prelude_eval("(when-not false 42)"), Ok(Val::Int(42)));
    }

    #[test]
    fn prelude_when_not_true() {
        assert_eq!(prelude_eval("(when-not true 42)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_and_empty() {
        assert_eq!(prelude_eval("(and)"), Ok(Val::Bool(true)));
    }

    #[test]
    fn prelude_and_single() {
        assert_eq!(prelude_eval("(and 42)"), Ok(Val::Int(42)));
    }

    #[test]
    fn prelude_and_two_truthy() {
        assert_eq!(prelude_eval("(and 1 2)"), Ok(Val::Int(2)));
    }

    #[test]
    fn prelude_and_short_circuit() {
        assert_eq!(prelude_eval("(and false 2)"), Ok(Val::Bool(false)));
    }

    #[test]
    fn prelude_and_nil_short_circuit() {
        assert_eq!(prelude_eval("(and nil 2)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_or_empty() {
        assert_eq!(prelude_eval("(or)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_or_single() {
        assert_eq!(prelude_eval("(or 42)"), Ok(Val::Int(42)));
    }

    #[test]
    fn prelude_or_first_truthy() {
        assert_eq!(prelude_eval("(or 1 2)"), Ok(Val::Int(1)));
    }

    #[test]
    fn prelude_or_skip_nil() {
        assert_eq!(prelude_eval("(or nil 2)"), Ok(Val::Int(2)));
    }

    #[test]
    fn prelude_or_skip_false_nil() {
        assert_eq!(prelude_eval("(or false nil 3)"), Ok(Val::Int(3)));
    }

    #[test]
    fn prelude_cond_basic() {
        assert_eq!(prelude_eval("(cond false 1 true 2)"), Ok(Val::Int(2)));
    }

    #[test]
    fn prelude_cond_default() {
        assert_eq!(prelude_eval("(cond false 1 42)"), Ok(Val::Int(42)));
    }

    #[test]
    fn prelude_cond_empty() {
        assert_eq!(prelude_eval("(cond)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_cond_first_match() {
        assert_eq!(prelude_eval("(cond true 1 true 2)"), Ok(Val::Int(1)));
    }

    #[test]
    fn prelude_defn_basic() {
        assert_eq!(
            prelude_eval("(do (defn add [a b] (+ a b)) (add 1 2))"),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn prelude_defn_multi_body() {
        assert_eq!(
            prelude_eval("(do (defn f [x] 1 2 (+ x 10)) (f 5))"),
            Ok(Val::Int(15))
        );
    }

    // =========================================================================
    // fn recur tests (#225)
    // =========================================================================

    #[test]
    fn fn_recur_factorial() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Define factorial with recur
        eval_str(
            "(def factorial (fn [n acc] (if (= n 0) acc (recur (- n 1) (* acc n)))))",
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(eval_str("(factorial 5 1)", &mut env, &d), Ok(Val::Int(120)));
    }

    #[test]
    fn fn_recur_countdown() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str(
            r#"(def countdown (fn [n] (if (= n 0) "done" (recur (- n 1)))))"#,
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            eval_str("(countdown 100)", &mut env, &d),
            Ok(Val::Str("done".into()))
        );
    }

    #[test]
    fn fn_recur_no_recur_regression() {
        // Normal fn without recur must still work
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(add 3 4)", &mut env, &d), Ok(Val::Int(7)));
    }

    #[test]
    fn fn_recur_wrong_arity() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def f (fn [a b] (recur 1)))", &mut env, &d).unwrap();
        let err = eval_str("(f 1 2)", &mut env, &d).unwrap_err();
        assert!(err_contains(&err, "expected 2"), "got: {err}");
    }

    #[test]
    fn fn_recur_variadic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Variadic fn that sums via recur: acc + first of rest, recur with rest
        eval_str(
            "(def sum-all (fn [acc & nums] (if (= (count nums) 0) acc (recur (+ acc (first nums)) (rest nums)))))",
            &mut env,
            &d,
        )
        .unwrap();
        // sum-all 0 1 2 3 → 6
        // Note: recur with variadic expects fixed_params + 1 args (the rest becomes a list)
        assert_eq!(eval_str("(sum-all 0 1 2 3)", &mut env, &d), Ok(Val::Int(6)));
    }

    #[test]
    fn fn_recur_single_iteration() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def once (fn [x] (if x (recur false) 42)))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(once true)", &mut env, &d), Ok(Val::Int(42)));
    }

    // =========================================================================
    // Stdlib tests (#202)
    // =========================================================================

    #[test]
    fn stdlib_type_int() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(type 42)", &mut env, &d),
            Ok(Val::Keyword("int".into()))
        );
    }

    #[test]
    fn stdlib_type_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(type nil)", &mut env, &d),
            Ok(Val::Keyword("nil".into()))
        );
    }

    #[test]
    fn stdlib_type_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(type (fn [x] x))", &mut env, &d),
            Ok(Val::Keyword("fn".into()))
        );
    }

    #[test]
    fn stdlib_nil_pred() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(nil? nil)", &mut env, &d), Ok(Val::Bool(true)));
        assert_eq!(eval_str("(nil? 0)", &mut env, &d), Ok(Val::Bool(false)));
    }

    #[test]
    fn stdlib_some_pred() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(some? nil)", &mut env, &d), Ok(Val::Bool(false)));
        assert_eq!(eval_str("(some? 0)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn stdlib_empty_pred() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(empty? nil)", &mut env, &d), Ok(Val::Bool(true)));
        assert_eq!(
            eval_str("(empty? (list))", &mut env, &d),
            Ok(Val::Bool(true))
        );
        assert_eq!(
            eval_str("(empty? (list 1))", &mut env, &d),
            Ok(Val::Bool(false))
        );
    }

    #[test]
    fn stdlib_contains_map() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(contains? {:a 1 :b 2} :a)", &mut env, &d),
            Ok(Val::Bool(true))
        );
        assert_eq!(
            eval_str("(contains? {:a 1} :z)", &mut env, &d),
            Ok(Val::Bool(false))
        );
    }

    #[test]
    fn stdlib_str_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(str)", &mut env, &d), Ok(Val::Str("".into())));
    }

    #[test]
    fn stdlib_str_concat() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str(r#"(str "hello" " " "world")"#, &mut env, &d),
            Ok(Val::Str("hello world".into()))
        );
    }

    #[test]
    fn stdlib_str_nil_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str(r#"(str "a" nil "b")"#, &mut env, &d),
            Ok(Val::Str("ab".into()))
        );
    }

    #[test]
    fn stdlib_name_keyword() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(name :foo)", &mut env, &d),
            Ok(Val::Str("foo".into()))
        );
    }

    #[test]
    fn stdlib_name_symbol() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(name 'bar)", &mut env, &d),
            Ok(Val::Str("bar".into()))
        );
    }

    #[test]
    fn stdlib_println_returns_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str(r#"(println "test")"#, &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn stdlib_map_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def inc (fn [x] (+ x 1)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(map inc (list 1 2 3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(2), Val::Int(3), Val::Int(4)]))
        );
    }

    #[test]
    fn stdlib_map_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def f (fn [x] x))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(map f (list))", &mut env, &d),
            Ok(Val::List(vec![]))
        );
    }

    #[test]
    fn stdlib_filter_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def pos? (fn [x] (> x 0)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(filter pos? (list -1 0 1 2 -3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2)]))
        );
    }

    #[test]
    fn stdlib_reduce_with_init() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(reduce add 0 (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(6))
        );
    }

    #[test]
    fn stdlib_reduce_no_init() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(reduce add (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(6))
        );
    }

    #[test]
    fn stdlib_reduce_empty_no_init_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def f (fn [a b] a))", &mut env, &d).unwrap();
        assert!(eval_str("(reduce f (list))", &mut env, &d).is_err());
    }

    #[test]
    fn stdlib_reduce_empty_with_init() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(reduce add 100 (list))", &mut env, &d),
            Ok(Val::Int(100))
        );
    }

    // =========================================================================
    // Effect system tests (#205)
    // =========================================================================

    /// Helper: load prelude then eval — needed for try/throw macros
    fn effects_eval(input: &str) -> Result<Val, Val> {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let prelude_forms = crate::read_many(crate::PRELUDE)
            .map_err(|e| error::parse(Some("prelude.glia"), e.to_string()))?;
        for form in &prelude_forms {
            eval_blocking(form, &mut env, &d)?;
        }
        eval_str(input, &mut env, &d)
    }

    // --- perform / with-handler primitives ---

    #[test]
    fn perform_without_handler_propagates() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(perform :fail 42)", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn with_handler_catches_effect() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :fail (fn [error] (+ error 1)) (perform :fail 42))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(43)));
    }

    #[test]
    fn with_handler_passes_through_on_no_effect() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str(
                "(with-effect-handler :fail (fn [error] 0) (+ 1 2))",
                &mut env,
                &d
            ),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn with_handler_unmatched_effect_propagates() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :other (fn [error] 0) (perform :fail 42))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn nested_handlers() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Inner handler catches :fail, outer catches :other
        assert_eq!(
            eval_str(
                "(with-effect-handler :other (fn [error] 99) (with-effect-handler :fail (fn [error] (+ error 10)) (perform :fail 5)))",
                &mut env,
                &d
            ),
            Ok(Val::Int(15))
        );
    }

    // --- try / throw macros (prelude) ---

    #[test]
    fn throw_basic() {
        let result = effects_eval("(throw 42)");
        assert!(result.is_err());
    }

    #[test]
    fn try_ok() {
        // No throw → try returns the body's value directly.
        assert_eq!(effects_eval("(try (+ 1 2))"), Ok(Val::Int(3)));
    }

    #[test]
    fn try_err() {
        // Wildcard catch binds the thrown value verbatim.
        let result = effects_eval(r#"(try (throw {:type :test}) (catch _ e e))"#).unwrap();
        // Plain-map throw isn't catchable by tag (no :glia.error/type),
        // but wildcard sees the map verbatim.
        if let Val::Map(m) = &result {
            assert_eq!(
                m.get(&Val::Keyword("type".into())),
                Some(&Val::Keyword("test".into()))
            );
        } else {
            panic!("expected map, got {result:?}");
        }
    }

    #[test]
    fn try_catch_string() {
        // Strings flow through wildcard catch verbatim.
        let result = effects_eval(r#"(try (throw "just a string") (catch _ e e))"#).unwrap();
        assert_eq!(result, Val::Str("just a string".into()));
    }

    #[test]
    fn nested_try() {
        // Inner catch handles, outer never sees the throw.
        let result =
            effects_eval("(try (try (throw 1) (catch _ e e)) (catch _ e (+ e 100)))").unwrap();
        assert_eq!(result, Val::Int(1));
    }

    // --- ex-info ---

    #[test]
    fn ex_info_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(r#"(ex-info "bad input" {:type :invalid})"#, &mut env, &d).unwrap();
        if let Val::Map(m) = &result {
            assert_eq!(
                m.get(&Val::Keyword("message".into())),
                Some(&Val::Str("bad input".into()))
            );
            assert_eq!(
                m.get(&Val::Keyword("type".into())),
                Some(&Val::Keyword("invalid".into()))
            );
        } else {
            panic!("expected map, got {result:?}");
        }
    }

    #[test]
    fn ex_info_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(ex-info)", &mut env, &d).is_err());
    }

    // --- or-else ---

    #[test]
    fn or_else_ok() {
        assert_eq!(effects_eval("(or-else (+ 1 2) 0)"), Ok(Val::Int(3)));
    }

    #[test]
    fn or_else_err() {
        assert_eq!(effects_eval("(or-else (throw 42) 0)"), Ok(Val::Int(0)));
    }

    // --- guard ---

    #[test]
    fn guard_pass() {
        assert_eq!(effects_eval("(guard true {:type :fail})"), Ok(Val::Nil));
    }

    #[test]
    fn guard_fail() {
        // ex-info now stamps :glia.error/type from :type, so a guard
        // failure is catchable by the user's tag.
        let result = effects_eval(
            r#"(try (guard false (ex-info "nope" {:type :fail}))
                    (catch :fail e (get e :glia.error/message)))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Str("nope".into()));
    }

    // --- existing error format ---

    #[test]
    fn internal_error_is_structured() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Division by zero should produce a structured error
        let err = eval_str("(/ 1 0)", &mut env, &d).unwrap_err();
        assert!(err_contains(&err, "division by zero"));
    }

    // --- effect edge cases ---

    #[test]
    fn perform_non_keyword_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(r#"(perform 42 "data")"#, &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(&result.unwrap_err(), "keyword"));
    }

    #[test]
    fn perform_nil_data() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :test (fn [data] data) (perform :test nil))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Nil));
    }

    #[test]
    fn perform_in_loop() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :done (fn [data] data) (loop [i 0] (if (= i 3) (perform :done i) (recur (+ i 1)))))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn handler_missing_key() {
        // with-effect-handler requires a keyword or cap target
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(with-effect-handler (perform :test 42))", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn handler_not_function() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            r#"(with-effect-handler :test 42 (perform :test "data"))"#,
            &mut env,
            &d,
        );
        assert!(result.is_err());
        assert!(err_contains(&result.unwrap_err(), "function"));
    }

    #[test]
    fn handler_multi_body() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :test (fn [data] data) (def x 1) (perform :test (+ x 1)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(2)));
    }

    #[test]
    fn handler_throws_effect() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :outer (fn [error] (+ error 100)) (with-effect-handler :fail (fn [error] (perform :outer error)) (perform :fail 5)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(105)));
    }

    #[test]
    fn ex_info_non_string_msg() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(ex-info 42 {})", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn ex_info_non_map_data() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(r#"(ex-info "msg" [1 2])"#, &mut env, &d);
        assert!(result.is_err());
    }

    // --- prelude macro edge cases ---

    #[test]
    fn try_multiple_body() {
        // Multi-form bodies must wrap in `do` under the new shape
        // (try takes one EXPR + zero or more catch clauses).
        assert_eq!(effects_eval("(try (do 1 2 3))"), Ok(Val::Int(3)));
    }

    #[test]
    fn throw_nil() {
        let result = effects_eval("(try (throw nil) (catch _ e e))").unwrap();
        assert_eq!(result, Val::Nil);
    }

    #[test]
    fn throw_int() {
        let result = effects_eval("(try (throw 42) (catch _ e e))").unwrap();
        assert_eq!(result, Val::Int(42));
    }

    #[test]
    fn throw_vector() {
        let result = effects_eval("(try (throw [1 2 3]) (catch _ e e))").unwrap();
        assert_eq!(
            result,
            Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)])
        );
    }

    #[test]
    fn guard_truthy_int() {
        assert_eq!(effects_eval("(guard 42 {:type :fail})"), Ok(Val::Nil));
    }

    #[test]
    fn guard_truthy_string() {
        assert_eq!(effects_eval(r#"(guard "hi" {:type :fail})"#), Ok(Val::Nil));
    }

    #[test]
    fn or_else_nested() {
        assert_eq!(
            effects_eval("(or-else (or-else (throw 1) (throw 2)) 3)"),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn try_deeply_nested() {
        // Each layer catches via wildcard; thrown value bubbles up
        // through the catches, still equal to 1.
        let result =
            effects_eval("(try (try (try (throw 1) (catch _ e e)) (catch _ e e)) (catch _ e e))")
                .unwrap();
        assert_eq!(result, Val::Int(1));
    }

    #[test]
    fn guard_with_ex_info() {
        // The thrown ex-info has both :glia.error/message (canonical)
        // and :message (back-compat) populated.
        let err =
            effects_eval(r#"(try (guard false (ex-info "nope" {:type :fail})) (catch _ e e))"#)
                .unwrap();
        if let Val::Map(m) = &err {
            assert_eq!(
                m.get(&Val::Keyword("glia.error/message".into())),
                Some(&Val::Str("nope".into()))
            );
            assert_eq!(
                m.get(&Val::Keyword("message".into())),
                Some(&Val::Str("nope".into()))
            );
            assert_eq!(
                m.get(&Val::Keyword("glia.error/type".into())),
                Some(&Val::Keyword("fail".into()))
            );
        } else {
            panic!("expected map, got {err:?}");
        }
    }

    // =========================================================================
    // Resume / continuation tests (#247)
    // =========================================================================

    #[test]
    fn resume_basic() {
        // Handler resumes with 42, body continues: (+ 10 42) = 52
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] (resume 42)) (+ 10 (perform :foo 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(52)));
    }

    #[test]
    fn resume_with_data() {
        // Handler receives data and resumes with data + 1
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :inc (fn [data resume] (resume (+ data 1))) (perform :inc 41))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(42)));
    }

    #[test]
    fn abort_1arg_handler() {
        // 1-arg handler = abort semantics (backward compat)
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data] 99) (+ 10 (perform :foo 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(99)));
    }

    #[test]
    fn abort_2arg_handler_no_resume() {
        // 2-arg handler that doesn't call resume = abort
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] 99) (+ 10 (perform :foo 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(99)));
    }

    #[test]
    fn resume_oneshot_violation() {
        // Calling resume twice should error
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] (resume 1) (resume 2)) (perform :foo 0))",
            &mut env,
            &d,
        );
        // The second resume should error (one-shot violated)
        // But the first resume short-circuits via Err(Val::Resume), so (resume 2) is never reached.
        // Actually, Err(Val::Resume) propagates up, so the handler returns Err(Val::Resume(1)).
        // with-handler catches Resume and resumes the body. Body returns 1. Result: Ok(1).
        assert_eq!(result, Ok(Val::Int(1)));
    }

    #[test]
    fn resume_nested_handlers() {
        // Inner handler resumes, outer handler not triggered
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :outer (fn [data] 0) (with-effect-handler :inner (fn [data resume] (resume 42)) (+ 10 (perform :inner 0))))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(52)));
    }

    #[test]
    fn resume_different_value_types() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Resume with nil
        assert_eq!(
            eval_str(
                "(with-effect-handler :foo (fn [data resume] (resume nil)) (perform :foo 0))",
                &mut env,
                &d,
            ),
            Ok(Val::Nil)
        );
        // Resume with string
        let result = eval_str(
            r#"(with-effect-handler :foo (fn [data resume] (resume "hello")) (perform :foo 0))"#,
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Str("hello".into())));
    }

    #[test]
    fn resume_try_throw_interaction() {
        // try/throw still compose with the resume state machine —
        // wildcard catch sees the thrown value verbatim.
        assert_eq!(
            effects_eval("(try (throw 42) (catch _ e e))"),
            Ok(Val::Int(42))
        );
    }

    #[test]
    fn resume_in_loop() {
        // perform inside a loop body, handler resumes, loop continues
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :step (fn [data resume] (resume (+ data 1))) (loop [i 0] (if (= i 3) i (recur (perform :step i)))))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn resume_multiple_sequential_performs() {
        // Body performs twice — each gets its own resume
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :inc (fn [data resume] (resume (+ data 10))) (+ (perform :inc 1) (perform :inc 2)))",
            &mut env,
            &d,
        );
        // (perform :inc 1) → resume(11), (perform :inc 2) → resume(12), total = 23
        assert_eq!(result, Ok(Val::Int(23)));
    }

    #[test]
    fn perform_without_handler_still_errors() {
        // No handler context → unhandled effect
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(perform :foo 42)", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn resume_unmatched_effect_propagates() {
        // Handler for :bar doesn't match :foo
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :bar (fn [data resume] (resume 0)) (perform :foo 42))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn resume_body_no_effect_passes_through() {
        // Body doesn't perform — result passes through
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] (resume 0)) (+ 1 2))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn resume_handler_map_eval_before_push() {
        // Handler closures don't see the current handler context
        // (they're evaluated before the context is pushed)
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // This test just verifies the handler works — the ordering guarantee
        // is architectural (tested in the spike).
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] (resume 100)) (perform :foo 0))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(100)));
    }

    // =========================================================================
    // G1 — resumable-effects semantics lock-in
    //
    // These tests pin down the observable guarantees the resumable-effects
    // model depends on. They assert existing behavior; they must not require
    // any runtime change.
    // =========================================================================

    #[test]
    fn abort_without_resume_skips_body_after_perform() {
        // A handler that returns WITHOUT calling `resume` aborts the suspended
        // body: the code *after* the `perform` never runs. We make that
        // observable with a second, distinct effect that would only fire if
        // execution continued past the first `perform`.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :probe (fn [d] :probe-ran)
               (with-effect-handler :abort (fn [d] :aborted)
                 (do (perform :abort 0) (perform :probe 0))))",
            &mut env,
            &d,
        );
        // If the body were resumed, (perform :probe 0) would run and the result
        // would be :probe-ran. Because the :abort handler never resumes, the
        // do-block is discarded and we get the handler's value instead.
        assert_eq!(result, Ok(Val::Keyword("aborted".into())));
    }

    #[test]
    fn resume_continues_at_exact_perform_site_in_nested_expr() {
        // `resume` returns control to the precise position of the `perform`
        // inside a larger expression: the surrounding arithmetic sees the
        // resumed value in place. (+ 1 (* 10 (perform :x 0))) with resume 5
        // must evaluate as (+ 1 (* 10 5)) = 51.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :x (fn [d resume] (resume 5)) (+ 1 (* 10 (perform :x 0))))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(51)));
    }

    #[test]
    fn handler_reperform_same_effect_forwards_to_next_outer_handler() {
        // Handler forwarding: a handler frame is popped before it runs, so a
        // handler that re-performs the *same* effect reaches the NEXT outer
        // handler rather than recursing into itself (which would loop forever).
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :log (fn [d] :outer)
               (with-effect-handler :log (fn [d] (perform :log d))
                 (perform :log 0)))",
            &mut env,
            &d,
        );
        // Inner handler catches :log, re-performs :log; because its own frame
        // is already popped, the re-perform lands on the outer handler → :outer.
        assert_eq!(result, Ok(Val::Keyword("outer".into())));
    }

    #[test]
    fn async_native_handler_resumes_body() {
        // An async native handler that calls the provided `resume` continuation
        // must resume the suspended body with the sent value, exactly like a
        // synchronous handler. (+ 10 (perform :inc 41)) with resume(41+1) = 52.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "async-resume".into(),
            Val::AsyncNativeFn {
                name: "async-resume".into(),
                func: Rc::new(|args: Vec<Val>| {
                    let data = args[0].clone();
                    let resume = args[1].clone();
                    Box::pin(async move {
                        if let Val::NativeFn { func, .. } = &resume {
                            let next = match data {
                                Val::Int(n) => Val::Int(n + 1),
                                other => other,
                            };
                            // Returns Err(Val::Resume(..)); the handler state
                            // machine translates that into a body resume.
                            func(&[next])
                        } else {
                            Err(Val::from("async-resume: bad resume".to_string()))
                        }
                    })
                }),
            },
        );
        let result = eval_str(
            "(with-effect-handler :inc async-resume (+ 10 (perform :inc 41)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(52)));
    }

    #[test]
    fn native_fn_display() {
        let nf = Val::NativeFn {
            name: "test".into(),
            func: Rc::new(|_: &[Val]| Ok(Val::Nil)),
        };
        assert_eq!(format!("{nf}"), "#<native-fn test>");
    }

    #[test]
    fn native_fn_equality() {
        let func: crate::NativeFnImpl = Rc::new(|_: &[Val]| Ok(Val::Nil));
        let a = Val::NativeFn {
            name: "test".into(),
            func: func.clone(),
        };
        let b = Val::NativeFn {
            name: "test".into(),
            func: func.clone(),
        };
        assert_eq!(a, b); // same Rc → equal
        let c = Val::NativeFn {
            name: "test".into(),
            func: Rc::new(|_: &[Val]| Ok(Val::Nil)),
        };
        assert_ne!(a, c); // different Rc → not equal
    }

    #[test]
    fn try_resume_macro() {
        // try-resume catches error and resumes
        let result = effects_eval("(try-resume (fn [err resume] (resume 42)) (throw :oops))");
        assert_eq!(result, Ok(Val::Int(42)));
    }

    #[test]
    fn try_resume_macro_abort() {
        // try-resume with recovery fn that doesn't resume → abort
        let result = effects_eval("(try-resume (fn [err resume] 99) (throw :oops))");
        assert_eq!(result, Ok(Val::Int(99)));
    }

    #[test]
    fn try_resume_macro_no_error() {
        // try-resume with no error — body result passes through
        let result = effects_eval("(try-resume (fn [error resume] 0) (+ 1 2))");
        assert_eq!(result, Ok(Val::Int(3)));
    }

    // =========================================================================
    // try / catch — multi-clause dispatch + re-throw semantics
    // =========================================================================

    #[test]
    fn catch_multiple_clauses_first_match_wins() {
        // Three catches; only :foo matches the thrown :foo error.
        let result = effects_eval(
            r#"(try (throw (ex-info "boom" {:type :foo}))
                 (catch :bar e :took-bar)
                 (catch :foo e :took-foo)
                 (catch _    e :took-wild))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("took-foo".into()));
    }

    #[test]
    fn catch_non_matching_falls_through_to_wildcard() {
        let result = effects_eval(
            r#"(try (throw (ex-info "boom" {:type :unknown}))
                 (catch :bar e :took-bar)
                 (catch _    e :took-wild))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("took-wild".into()));
    }

    #[test]
    fn catch_non_matching_no_wildcard_rethrows_to_outer_try() {
        // Inner try has only :bar; the :foo throw propagates to outer try
        // which catches via wildcard.
        let result = effects_eval(
            r#"(try (try (throw (ex-info "boom" {:type :foo}))
                     (catch :bar e :inner-bar))
                 (catch _ e (get e :glia.error/type)))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("foo".into()));
    }

    #[test]
    fn rethrow_inside_catch_body_propagates_to_outer_try() {
        // Inner catch matches and re-throws a different error; outer try
        // catches the re-throw. The inner handler must NOT loop on its own
        // re-throw (commitment 2 of the eng review: popped-handler-skip).
        let result = effects_eval(
            r#"(try (try (throw (ex-info "first" {:type :first}))
                     (catch :first e (throw (ex-info "second" {:type :second}))))
                 (catch :second e (get e :glia.error/type))
                 (catch _       e :wrong))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("second".into()));
    }

    #[test]
    fn unhandled_throw_escapes_as_glia_exception_effect() {
        // No try in scope — throw escapes as Val::Effect carrier with
        // effect_type = "glia.exception". Outer callers (kernel, MCP,
        // shell) rely on this contract.
        let err = effects_eval("(throw (ex-info \"escape\" {:type :foo}))").unwrap_err();
        match &err {
            Val::Effect { effect_type, data } => {
                assert_eq!(effect_type, error::EXCEPTION_EFFECT);
                // The data is the inner structured error map.
                assert_eq!(error::type_tag(data), Some("foo"));
            }
            other => panic!("expected Val::Effect, got {other:?}"),
        }
        // unwrap_thrown peels the carrier for outer callers.
        let inner = error::unwrap_thrown(&err).expect("should peel");
        assert_eq!(error::type_tag(inner), Some("foo"));
    }

    #[test]
    fn rethrow_with_no_outer_try_escapes_as_effect() {
        // Single try, only matches :a. Throwing :b means the dispatcher
        // re-throws; with no outer try, the re-throw escapes as
        // Val::Effect — same contract as a direct unhandled throw.
        let err = effects_eval(
            r#"(try (throw (ex-info "x" {:type :b}))
                 (catch :a e :ignored))"#,
        )
        .unwrap_err();
        let inner = error::unwrap_thrown(&err).expect("should peel");
        assert_eq!(error::type_tag(inner), Some("b"));
    }

    // =========================================================================
    // match — pattern matching tests
    // =========================================================================

    #[test]
    fn match_literal_first_clause() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 42 42 :yes _ :no)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_literal_second_clause() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 99 42 :a 99 :b _ :c)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("b".into())));
    }

    #[test]
    fn match_wildcard_default() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 7 42 :no _ :yes)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_no_clause_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 7 42 :no)", &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(&result.unwrap_err(), "no clause matched"));
    }

    #[test]
    fn match_bind_visible_in_body() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 42 x (+ x 1))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(43)));
    }

    #[test]
    fn match_nil_literal() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match nil nil :yes _ :no)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_keyword_literal() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match :ok :ok :yes _ :no)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_vector_pattern() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match [1 2] [a b] (+ a b) _ 0)", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn match_vector_wrong_length() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match [1 2 3] [a b] :two _ :other)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("other".into())));
    }

    #[test]
    fn match_map_pattern() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            r#"(match {:name "Alice" :age 30} {:name name} name _ "unknown")"#,
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Str("Alice".into())));
    }

    #[test]
    fn match_evaluated_scrutinee() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match (+ 1 2) 3 :yes _ :no)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_with_effect_normal_return() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(match (+ 1 2) result result (effect :fail error) :caught)",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn match_with_effect_abort() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(match (perform :fail 42) result result (effect :fail error) (+ error 1))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(43)));
    }

    #[test]
    fn match_with_effect_resume() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(match (+ 10 (perform :inc 5)) result result (effect :inc data resume) (resume (+ data 10)))",
            &mut env,
            &d,
        );
        // perform :inc 5 → handler resumes with 15 → body evaluates (+ 10 15) = 25
        assert_eq!(result, Ok(Val::Int(25)));
    }

    #[test]
    fn match_effect_unmatched_propagates() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(match (perform :foo 42) result result (effect :bar data) data)",
            &mut env,
            &d,
        );
        // :foo doesn't match :bar, propagates out — no outer handler → error
        assert!(result.is_err());
    }

    #[test]
    fn match_nested_pattern() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match [1 [2 3]] [a [b c]] (+ a b c) _ 0)", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(6)));
    }

    #[test]
    fn match_odd_clauses_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 42 :a)", &mut env, &d);
        assert!(result.is_err());
    }

    // =========================================================================
    // Destructuring tests
    // =========================================================================

    #[test]
    fn let_vector_destructure() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [[a b] [1 2]] (+ a b))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn let_map_destructure() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(r#"(let [{:name name} {:name "Alice"}] name)"#, &mut env, &d);
        assert_eq!(result, Ok(Val::Str("Alice".into())));
    }

    #[test]
    fn let_destructure_mismatch_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [[a b] 42] (+ a b))", &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(&result.unwrap_err(), "destructuring failed"));
    }

    #[test]
    fn let_nested_destructure() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [[a [b c]] [1 [2 3]]] (+ a b c))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(6)));
    }

    #[test]
    fn let_vector_rest() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [[a & rest] [1 2 3]] rest)", &mut env, &d);
        assert_eq!(result, Ok(Val::List(vec![Val::Int(2), Val::Int(3)])));
    }

    #[test]
    fn let_simple_still_works() {
        // Ensure simple let bindings are unaffected
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [x 1 y 2] (+ x y))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn loop_destructure_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Destructure in loop, recur re-matches
        let result = eval_str(
            "(loop [[a b] [0 0]] (if (= a 3) b (recur [(+ a 1) (+ b 10)])))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(30)));
    }

    #[test]
    fn loop_destructure_recur_mismatch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // recur with non-vector when loop expects [a b]
        let result = eval_str("(loop [[a b] [0 0]] (recur 42))", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn loop_simple_still_works() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(loop [i 0] (if (= i 5) i (recur (+ i 1))))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(5)));
    }

    // -----------------------------------------------------------------------
    // Cap-targeted effect handler tests
    // -----------------------------------------------------------------------

    /// Helper: create a capability with a unique instance identity.
    fn make_test_cap(name: &str, marker: i32) -> Val {
        make_cap(name, format!("test-cid-{name}"), Rc::new(marker))
    }

    #[test]
    fn perform_cap_basic() {
        // Cap-targeted perform dispatches to the correct handler.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("executor", 1);
        env.set("my-cap".into(), cap);
        // Handler receives data (a list of [:method args...]) and returns it.
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data resume] (resume data)) (perform my-cap :run 42))",
            &mut env,
            &d,
        );
        assert_eq!(
            result,
            Ok(Val::List(vec![Val::Keyword("run".into()), Val::Int(42)]))
        );
    }

    #[test]
    fn perform_cap_different_cid_no_match() {
        // Different test caps have different instance identities, so they do not match.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap1 = make_test_cap("executor", 1);
        let cap2 = make_test_cap("ipfs", 2);
        env.set("cap1".into(), cap1);
        env.set("cap2".into(), cap2);
        // Handler installed for cap1 (executor CID), perform on cap2 (ipfs CID) — no match.
        let result = eval_str(
            "(with-effect-handler cap1 (fn [data] :handled) (perform cap2 :run 0))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn perform_cap_same_cid_different_id_no_match() {
        // Same schema CID, different cap instances — does NOT match.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap1 = make_test_cap("executor", 1);
        let cap2 = make_test_cap("executor", 2);
        env.set("cap1".into(), cap1);
        env.set("cap2".into(), cap2);
        // Handler installed for cap1, perform on cap2 — no match due to cap_id mismatch.
        let result = eval_str(
            "(with-effect-handler cap1 (fn [data] :handled) (perform cap2 :run 0))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn perform_cap_same_instance_matches() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap1 = make_test_cap("executor", 1);
        let cap2 = cap1.clone();
        env.set("cap1".into(), cap1);
        env.set("cap2".into(), cap2);
        let result = eval_str(
            "(with-effect-handler cap1 (fn [data] :handled) (perform cap2 :run 0))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("handled".into())));
    }

    #[test]
    fn unhandled_cap_effect_fails_closed_with_structured_carrier() {
        // An unhandled capability-targeted effect must fail CLOSED and surface a
        // structured effect carrier (Val::Effect), never a plain string. This is
        // what lets outer callers pattern-match / unwrap the failure instead of
        // string-scraping.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("executor", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str("(perform my-cap :run 42)", &mut env, &d);
        match result {
            Err(Val::Effect { effect_type, data }) => {
                assert_eq!(effect_type, "cap:executor");
                // The carrier retains the effect payload as structured data.
                assert!(matches!(*data, Val::List(_)));
            }
            other => panic!("expected structured Val::Effect carrier, got {other:?}"),
        }
    }

    #[test]
    fn defcap_define_and_perform() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do (defcap directory :lookup (fn [name] name))
                 (perform directory :lookup \"service\"))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Str("service".into())));
    }

    #[test]
    fn defcap_perform_hits_cap_handler_before_method_table() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do (defcap directory :lookup (fn [name] :backend))
                 (with-effect-handler directory
                   (fn [data] :handled)
                   (perform directory :lookup \"service\")))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("handled".into())));
    }

    #[test]
    fn defcap_unknown_method_denied() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do (defcap directory :lookup (fn [name] name))
                 (perform directory :announce \"x\"))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
        assert!(err_contains(&result.unwrap_err(), "not available"));
    }

    #[test]
    fn attenuate_allow_and_deny() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("svc", 1);
        env.set("svc".into(), cap.clone());
        let ok = eval_str(
            "(with-effect-handler svc (fn [data] :ok)
               (let [svc-ro (attenuate svc [:run])]
                 (perform svc-ro :run 1)))",
            &mut env,
            &d,
        );
        assert_eq!(ok, Ok(Val::Keyword("ok".into())));

        let denied = eval_str(
            "(with-effect-handler svc (fn [data] :ok)
               (let [svc-ro (attenuate svc [:run])]
                 (perform svc-ro :write 1)))",
            &mut env,
            &d,
        );
        assert!(denied.is_err());
        assert!(err_contains(&denied.unwrap_err(), "denied"));
    }

    #[test]
    fn attenuate_nested_intersection() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("svc", 1);
        env.set("svc".into(), cap);
        let denied = eval_str(
            "(with-effect-handler svc (fn [data] :ok)
               (let [a1 (attenuate svc [:run])
                     a2 (attenuate a1 [:write])]
                 (perform a2 :run 1)))",
            &mut env,
            &d,
        );
        assert!(denied.is_err());
        assert!(err_contains(&denied.unwrap_err(), "denied"));
    }

    #[test]
    fn fn_invocation_uses_caller_handler_stack_not_definition_stack() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do
               (def f
                 (with-effect-handler :log (fn [msg] :inner)
                   (fn [msg] (perform :log msg))))
               (f \"x\"))",
            &mut env,
            &d,
        );
        assert!(matches!(
            result,
            Err(Val::Effect { ref effect_type, .. }) if effect_type == "log"
        ));
    }

    #[test]
    fn macro_invocation_uses_caller_handler_stack_not_definition_stack() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do
               (with-effect-handler :log (fn [msg] :inner)
                 (defmacro m [x] (list (quote perform) :log x)))
               (m \"x\"))",
            &mut env,
            &d,
        );
        assert!(matches!(
            result,
            Err(Val::Effect { ref effect_type, .. }) if effect_type == "log"
        ));
    }

    #[test]
    fn perform_cap_no_handler() {
        // No handler installed for cap → unhandled effect error.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("ipfs", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str("(perform my-cap :cat \"/foo\")", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn perform_keyword_still_works() {
        // Existing keyword performs are unchanged.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :fail (fn [data] (+ data 1)) (perform :fail 42))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(43)));
    }

    #[test]
    fn with_effect_handler_non_cap_target_errors() {
        // Non-Cap first arg → error.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler 42 (fn [data] data) :body)",
            &mut env,
            &d,
        );
        assert!(result.is_err());
        if let Err(err) = &result {
            assert!(err_contains(err, "cap"));
        }
    }

    #[test]
    fn with_effect_handler_non_fn_handler_errors() {
        // Non-Fn second arg → error at perform time (handler is still a value).
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("x", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap 42 (perform my-cap :m 0))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn effect_handler_cap_shadows_outer() {
        // Inner handler for same cap wins.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data] :outer) (with-effect-handler my-cap (fn [data] :inner) (perform my-cap :m 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("inner".into())));
    }

    #[test]
    fn effect_handler_cap_attenuation_forward() {
        // Inner handler delegates to outer via perform on same cap.
        // Pop-before-handle makes this work: inner is popped, perform hits outer.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data resume] (resume :forwarded)) (with-effect-handler my-cap (fn [data resume] (perform my-cap :delegated 0)) (perform my-cap :m 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("forwarded".into())));
    }

    #[test]
    fn effect_handler_cap_attenuation_block() {
        // Inner handler blocks disallowed method — returns error without forwarding.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data resume] (resume :full-authority)) (with-effect-handler my-cap (fn [data] :blocked) (perform my-cap :m 0)))",
            &mut env,
            &d,
        );
        // Inner handler aborts (1-arg, no resume) → returns :blocked, body is abandoned.
        assert_eq!(result, Ok(Val::Keyword("blocked".into())));
    }

    #[test]
    fn mixed_stack_walk() {
        // Keyword handler + cap handler on same stack, correct dispatch.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        // Install keyword handler for :fail, then cap handler for my-cap.
        // Keyword perform should hit keyword handler; cap perform should hit cap handler.
        let result = eval_str(
            "(with-effect-handler :fail (fn [data] :keyword-handled) (with-effect-handler my-cap (fn [data] :cap-handled) (perform my-cap :m 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("cap-handled".into())));
    }

    #[test]
    fn mixed_stack_keyword_through_cap() {
        // Cap handler is on the stack but keyword perform goes to keyword handler.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler :fail (fn [data] :keyword-handled) (with-effect-handler my-cap (fn [data] :cap-handled) (perform :fail 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("keyword-handled".into())));
    }

    #[test]
    fn perform_cap_resume_value() {
        // Cap handler resumes with a transformed value.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("math", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data resume] (resume 100)) (+ 1 (perform my-cap :compute 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(101)));
    }

    #[test]
    fn perform_target_must_be_keyword_or_cap() {
        // Passing a string as target should error.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(perform \"not-valid\" 42)", &mut env, &d);
        assert!(result.is_err());
        if let Err(err) = &result {
            assert!(err_contains(err, "keyword or cap"));
        }
    }

    #[test]
    fn async_native_fn_basic() {
        // AsyncNativeFn should be callable and its result awaited.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "afn".into(),
            Val::AsyncNativeFn {
                name: "afn".into(),
                func: Rc::new(|args: Vec<Val>| {
                    Box::pin(core::future::ready(Ok(Val::Int(
                        if let Val::Int(n) = &args[0] {
                            n + 100
                        } else {
                            -1
                        },
                    ))))
                }),
            },
        );
        let result = eval_str("(afn 5)", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(105)));
    }

    #[test]
    fn async_native_fn_error() {
        // AsyncNativeFn returning Err should propagate as an error.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "fail-async".into(),
            Val::AsyncNativeFn {
                name: "fail-async".into(),
                func: Rc::new(|_args: Vec<Val>| {
                    Box::pin(core::future::ready(Err(Val::from(
                        "async boom".to_string(),
                    ))))
                }),
            },
        );
        let result = eval_str("(fail-async 1)", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn async_native_fn_in_effect_handler() {
        // AsyncNativeFn used as a cap handler should work correctly.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("svc", 1);
        env.set("my-cap".into(), cap.clone());
        env.set(
            "async-handler".into(),
            Val::AsyncNativeFn {
                name: "async-handler".into(),
                func: Rc::new(|_args: Vec<Val>| {
                    // Handler returns 999 directly.
                    Box::pin(core::future::ready(Ok(Val::Int(999))))
                }),
            },
        );
        let result = eval_str(
            "(with-effect-handler my-cap async-handler (perform my-cap :ping 1))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(999)));
    }

    #[test]
    fn handler_depth_limit() {
        // Exceeding MAX_HANDLER_DEPTH should error.
        // We pre-fill the handler stack to near the limit, then one more push should fail.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("x", 1);
        env.set("my-cap".into(), cap.clone());

        // Pre-fill handler stack to the limit.
        let cap_target = match &cap {
            Val::Cap {
                name,
                schema_cid,
                cap_id,
                ..
            } => effect::EffectTarget::Cap {
                name: name.clone(),
                schema_cid: schema_cid.clone(),
                cap_id: *cap_id,
            },
            _ => unreachable!(),
        };
        for _ in 0..effect::MAX_HANDLER_DEPTH {
            let ctx = Rc::new(RefCell::new(effect::HandlerContext {
                slot: Rc::new(RefCell::new(effect::EffectSlot::new())),
                target: cap_target.clone(),
            }));
            env.handler_stack.borrow_mut().push(ctx);
        }

        // One more with-effect-handler should hit the depth limit.
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data] data) :body)",
            &mut env,
            &d,
        );
        assert!(result.is_err());
        if let Err(err) = &result {
            assert!(err_contains(err, "depth limit"));
        }
    }
}
