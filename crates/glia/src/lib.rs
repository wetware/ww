//! Glia — Clojure-inspired language for wetware.
//!
//! Provides a rich EDN-like data literal language used as both the wetware
//! shell language and configuration format (`.glia` files).
//!
//! # Supported types
//!
//! | Syntax | Val variant | Example |
//! |--------|------------|---------|
//! | `nil` | `Nil` | `nil` |
//! | `true` / `false` | `Bool` | `true` |
//! | integers | `Int` | `42`, `-7` |
//! | floats | `Float` | `3.14`, `1e10` |
//! | `"strings"` | `Str` | `"hello"` |
//! | bare words | `Sym` | `foo`, `bar/baz` |
//! | `:keywords` | `Keyword` | `:port` |
//! | `(lists)` | `List` | `(a b c)` |
//! | `[vectors]` | `Vector` | `[1 2 3]` |
//! | `{maps}` | `Map` | `{:a 1 :b 2}` |
//! | `#{sets}` | `Set` | `#{:a :b}` |
//!
//! Commas are whitespace. Line comments start with `;`.

pub mod effect;
pub mod error;
pub mod eval;
pub mod expr;
pub mod oneshot;
pub mod pattern;
pub mod valmap;

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
pub use valmap::ValMap;

/// Crate version from Cargo.toml (compile-time).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Git short hash at build time, with `+dirty` suffix if worktree was dirty.
/// Falls back to `"unknown"` when built outside a git repo.
pub const GIT_COMMIT: &str = env!("GIT_COMMIT");

/// Shell banner line: `glia v0.1.0 (48c5498)`.
/// Call this at REPL startup for debuggability.
pub fn banner() -> String {
    format!("glia v{VERSION} ({GIT_COMMIT})")
}

/// Process-local monotonic counter for capability instance identity.
static CAP_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh capability instance identifier.
pub fn next_cap_id() -> u64 {
    CAP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Internal representation for Glia-native capability servers created by `defcap`.
#[derive(Clone)]
pub struct GliaCapInner {
    pub methods: HashMap<String, Val>,
    pub descriptor: Vec<u8>,
}

/// Internal representation for attenuated capabilities created by `attenuate`.
#[derive(Clone)]
pub struct AttenuatedCapInner {
    pub base: Val,
    pub allow_methods: BTreeSet<String>,
    pub descriptor: Vec<u8>,
}

/// Construct a capability value with a fresh instance identity.
pub fn make_cap(
    name: impl Into<String>,
    schema_cid: impl Into<String>,
    inner: Rc<dyn std::any::Any>,
) -> Val {
    Val::Cap {
        name: name.into(),
        schema_cid: schema_cid.into(),
        cap_id: next_cap_id(),
        inner,
    }
}

#[cfg(test)]
mod banner_tests {
    use super::*;

    #[test]
    fn banner_format() {
        let b = banner();
        assert!(
            b.starts_with("glia v"),
            "banner should start with 'glia v', got: {b}"
        );
        assert!(
            b.contains('('),
            "banner should contain commit hash in parens, got: {b}"
        );
        assert!(
            b.contains(')'),
            "banner should contain closing paren, got: {b}"
        );
    }

    #[test]
    fn version_is_semver() {
        assert!(
            VERSION.split('.').count() >= 2,
            "VERSION should be semver, got: {VERSION}"
        );
    }

    #[test]
    fn git_commit_not_empty() {
        assert!(!GIT_COMMIT.is_empty(), "GIT_COMMIT should not be empty");
    }
}

// ---------------------------------------------------------------------------
// Value type
// ---------------------------------------------------------------------------

/// One arity of a function: parameter names, optional variadic rest param, and body forms.
#[derive(Debug, Clone)]
pub struct FnArity {
    /// Positional parameter names.
    pub params: Vec<String>,
    /// If present, the name after `&` that collects remaining args as a List.
    pub variadic: Option<String>,
    /// Body forms — either raw Val (macro-produced) or pre-analyzed Expr.
    pub body: expr::FnBody,
}

/// Shared pointer to a native (Rust-side) function callable from Glia.
pub type NativeFnImpl = std::rc::Rc<dyn Fn(&[Val]) -> Result<Val, Val>>;

/// Shared pointer to an async native function callable from Glia.
/// Returns a boxed future that resolves to `Result<Val, Val>`.
/// The future is `'static` (no borrows from args — clone what you need).
pub type AsyncNativeFnImpl = std::rc::Rc<
    dyn Fn(Vec<Val>) -> core::pin::Pin<Box<dyn core::future::Future<Output = Result<Val, Val>>>>,
>;

/// A Clojure-like value.
#[derive(Clone)]
pub enum Val {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Sym(String),
    Keyword(String),
    List(Vec<Val>),
    Vector(Vec<Val>),
    Map(ValMap),
    Set(Vec<Val>),
    /// Opaque binary data — a runtime value, not parseable from text.
    /// Produced by evaluating expressions like `(ipfs cat "...")`.
    Bytes(Vec<u8>),
    /// A closure: one or more arities + captured environment snapshot.
    ///
    /// The env is `Rc`-shared to avoid infinite recursion when cloning closures
    /// that capture their own scope (e.g., `defn` puts the function in the env,
    /// then `fn` captures that env). Cloning an `Rc<Env>` is O(1).
    Fn {
        arities: Vec<FnArity>,
        env: std::rc::Rc<eval::Env>,
        is_cap_free: bool,
        cap_violation: Option<String>,
    },
    /// Internal sentinel returned by `recur` — never escapes `loop`.
    Recur(Vec<Val>),
    /// A macro: like a fn but receives unevaluated args and its result is re-evaluated.
    Macro {
        arities: Vec<FnArity>,
        env: std::rc::Rc<eval::Env>,
        is_cap_free: bool,
        cap_violation: Option<String>,
    },
    /// Internal sentinel returned by `perform` — caught by `with-handler`.
    /// Propagates up the eval stack until a matching handler is found.
    /// Used as fallback when no handler context exists (unhandled effect).
    Effect {
        effect_type: String,
        data: Box<Val>,
    },
    /// A Rust-side function callable from Glia. Used for `resume` and future
    /// stdlib builtins. The closure is behind Rc for Clone support.
    NativeFn {
        name: String,
        func: NativeFnImpl,
    },
    /// An async Rust-side function callable from Glia. Used for cap handlers
    /// that make async RPC calls. Takes owned args (Vec<Val>) so the future
    /// can be `'static`.
    AsyncNativeFn {
        name: String,
        func: AsyncNativeFnImpl,
    },
    /// Internal sentinel returned by `resume` — short-circuits the handler's
    /// eval chain. Propagates via Err like Effect and Recur. Must NOT be caught
    /// by nested `with-handler` — always re-propagated.
    Resume(Box<Val>),
    /// An opaque capability reference — a value the script can pass around but
    /// not inspect. The kernel creates these (e.g. executor, host) and dispatch
    /// handlers downcast them back to typed Cap'n Proto clients.
    ///
    /// `name` is the display label (e.g. "executor").
    /// `schema_cid` is the content-addressed type identity: CIDv1(raw, BLAKE3(canonical schema)).
    /// `cap_id` is a unique instance identity used for authority matching.
    /// `inner` is the type-erased capability, downcasted by the kernel.
    Cap {
        name: String,
        schema_cid: String,
        cap_id: u64,
        inner: std::rc::Rc<dyn std::any::Any>,
    },
    /// A cell definition: WASM binary + optional schema + captured capabilities.
    ///
    /// Created by the `cell` function, which bundles wasm/schema bytes with
    /// all `Val::Cap` bindings from its lexical scope. When the cell is
    /// registered via `(perform host :listen ...)`, the host extracts wasm,
    /// schema, and caps — injecting the caps into spawned children's membranes.
    Cell {
        wasm: Vec<u8>,
        schema: Option<Vec<u8>>,
        caps: Vec<(String, Val)>,
    },
}

impl core::fmt::Debug for Val {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Val::Nil => write!(f, "Nil"),
            Val::Bool(b) => f.debug_tuple("Bool").field(b).finish(),
            Val::Int(n) => f.debug_tuple("Int").field(n).finish(),
            Val::Float(n) => f.debug_tuple("Float").field(n).finish(),
            Val::Str(s) => f.debug_tuple("Str").field(s).finish(),
            Val::Sym(s) => f.debug_tuple("Sym").field(s).finish(),
            Val::Keyword(s) => f.debug_tuple("Keyword").field(s).finish(),
            Val::List(v) => f.debug_tuple("List").field(v).finish(),
            Val::Vector(v) => f.debug_tuple("Vector").field(v).finish(),
            Val::Map(m) => f.debug_tuple("Map").field(m).finish(),
            Val::Set(v) => f.debug_tuple("Set").field(v).finish(),
            Val::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Val::Fn { arities, .. } => write!(f, "Fn({} arities)", arities.len()),
            Val::Recur(v) => f.debug_tuple("Recur").field(v).finish(),
            Val::Macro { arities, .. } => write!(f, "Macro({} arities)", arities.len()),
            Val::Effect { effect_type, data } => f
                .debug_struct("Effect")
                .field("effect_type", effect_type)
                .field("data", data)
                .finish(),
            Val::NativeFn { name, .. } => write!(f, "NativeFn({name})"),
            Val::AsyncNativeFn { name, .. } => write!(f, "AsyncNativeFn({name})"),
            Val::Resume(v) => f.debug_tuple("Resume").field(v).finish(),
            Val::Cap { name, .. } => write!(f, "Cap({name})"),
            Val::Cell { wasm, schema, caps } => write!(
                f,
                "Cell({} bytes, schema={}, {} caps)",
                wasm.len(),
                schema
                    .as_ref()
                    .map_or("none".to_string(), |s| format!("{} bytes", s.len())),
                caps.len()
            ),
        }
    }
}

/// Extract (method_keyword, rest_args) from a capability dispatch payload.
///
/// Zero-allocation: returns borrows into the original `Val::List`.
/// Used by capability handlers in kernel and caps crates.
pub fn extract_method(data: &Val) -> Result<(&str, &[Val]), Val> {
    let items = match data {
        Val::List(items) => items.as_slice(),
        _ => return Err(Val::from("cap handler: expected list data")),
    };
    let method = match items.first() {
        Some(Val::Keyword(s)) => s.as_str(),
        _ => {
            return Err(Val::from(
                "cap handler: first arg must be a keyword method (e.g. :id, :run)",
            ))
        }
    };
    Ok((method, &items[1..]))
}

/// Convert a string error into a structured error value.
///
/// Enables incremental migration from `Result<Val, String>` to `Result<Val, Val>`:
/// existing `Err(format!(...))` sites auto-convert via the `?` operator.
impl From<String> for Val {
    fn from(s: String) -> Self {
        Val::Map(ValMap::from_pairs(vec![
            (Val::Keyword("type".into()), Val::Keyword("internal".into())),
            (Val::Keyword("message".into()), Val::Str(s)),
        ]))
    }
}

impl From<&str> for Val {
    fn from(s: &str) -> Self {
        Val::from(s.to_string())
    }
}

/// Val implements Error so it can be used with `?` in functions returning `Box<dyn Error>`.
impl std::error::Error for Val {}

impl PartialEq for Val {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Val::Nil, Val::Nil) => true,
            (Val::Bool(a), Val::Bool(b)) => a == b,
            (Val::Int(a), Val::Int(b)) => a == b,
            (Val::Float(a), Val::Float(b)) => a.to_bits() == b.to_bits(),
            (Val::Str(a), Val::Str(b)) => a == b,
            (Val::Sym(a), Val::Sym(b)) => a == b,
            (Val::Keyword(a), Val::Keyword(b)) => a == b,
            (Val::List(a), Val::List(b)) => a == b,
            (Val::Vector(a), Val::Vector(b)) => a == b,
            (Val::Map(a), Val::Map(b)) => a == b,
            (Val::Set(a), Val::Set(b)) => a == b,
            (Val::Bytes(a), Val::Bytes(b)) => a == b,
            // Closures and macros: identity equality via Rc pointer comparison.
            // Same Rc allocation = same closure instance.
            (Val::Fn { env: a, .. }, Val::Fn { env: b, .. }) => std::rc::Rc::ptr_eq(a, b),
            (Val::Macro { env: a, .. }, Val::Macro { env: b, .. }) => std::rc::Rc::ptr_eq(a, b),
            // Closures, native fns, and macros are never equal (identity semantics).
            (Val::NativeFn { func: a, .. }, Val::NativeFn { func: b, .. }) => {
                std::rc::Rc::ptr_eq(a, b)
            }
            (Val::AsyncNativeFn { func: a, .. }, Val::AsyncNativeFn { func: b, .. }) => {
                std::rc::Rc::ptr_eq(a, b)
            }
            // Caps match by instance identity.
            (Val::Cap { cap_id: a, .. }, Val::Cap { cap_id: b, .. }) => a == b,
            // Cells are equal if wasm and schema match (caps are opaque).
            (
                Val::Cell {
                    wasm: wa,
                    schema: sa,
                    ..
                },
                Val::Cell {
                    wasm: wb,
                    schema: sb,
                    ..
                },
            ) => wa == wb && sa == sb,
            // Recur, Effect, and Resume are internal sentinels — never equal.
            (Val::Recur(_), _) | (_, Val::Recur(_)) => false,
            (Val::Effect { .. }, _) | (_, Val::Effect { .. }) => false,
            (Val::Resume(_), _) | (_, Val::Resume(_)) => false,
            _ => false,
        }
    }
}

/// Eq is safe because PartialEq is reflexive for all variants:
/// - Floats use to_bits() (reflexive; NOTE: 0.0 != -0.0, deliberate deviation from Clojure)
/// - Fn/Macro use Rc::ptr_eq (reflexive: same pointer = true)
/// - Sentinels (Recur/Effect/Resume) return false (these are never used as map keys)
impl Eq for Val {}

impl std::hash::Hash for Val {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
        match self {
            Val::Nil => {}
            Val::Bool(b) => b.hash(state),
            Val::Int(n) => n.hash(state),
            // Consistent with PartialEq: 0.0 != -0.0 (bitwise comparison).
            Val::Float(n) => n.to_bits().hash(state),
            Val::Str(s) => s.hash(state),
            Val::Sym(s) => s.hash(state),
            Val::Keyword(s) => s.hash(state),
            Val::List(items) => items.hash(state),
            Val::Vector(items) => items.hash(state),
            Val::Map(m) => m.hash(state),
            Val::Set(items) => items.hash(state),
            Val::Bytes(b) => b.hash(state),
            // Identity hash via Rc pointer (consistent with Rc::ptr_eq PartialEq).
            Val::Fn { env, .. } => (std::rc::Rc::as_ptr(env) as *const () as usize).hash(state),
            Val::Macro { env, .. } => (std::rc::Rc::as_ptr(env) as *const () as usize).hash(state),
            Val::NativeFn { func, .. } => {
                (std::rc::Rc::as_ptr(func) as *const () as usize).hash(state)
            }
            Val::AsyncNativeFn { func, .. } => {
                (std::rc::Rc::as_ptr(func) as *const () as usize).hash(state)
            }
            Val::Cap { cap_id, .. } => cap_id.hash(state),
            Val::Cell { wasm, schema, .. } => {
                wasm.hash(state);
                schema.hash(state);
            }
            // Sentinels: hash by discriminant only (already done above).
            Val::Recur(_) | Val::Effect { .. } | Val::Resume(_) => {}
        }
    }
}

impl core::fmt::Display for Val {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Val::Nil => write!(f, "nil"),
            Val::Bool(b) => write!(f, "{b}"),
            Val::Int(n) => write!(f, "{n}"),
            Val::Float(n) => {
                // Ensure floats always have a decimal point
                if n.fract() == 0.0 && n.is_finite() {
                    write!(f, "{n:.1}")
                } else {
                    write!(f, "{n}")
                }
            }
            Val::Str(s) => write!(f, "\"{s}\""),
            Val::Sym(s) => write!(f, "{s}"),
            Val::Keyword(s) => write!(f, ":{s}"),
            Val::List(items) => fmt_seq(f, "(", ")", items),
            Val::Vector(items) => fmt_seq(f, "[", "]", items),
            Val::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{k} {v}")?;
                }
                write!(f, "}}")
            }
            Val::Set(items) => fmt_seq(f, "#{", "}", items),
            Val::Bytes(b) => write!(f, "<{} bytes>", b.len()),
            Val::Recur(_) => write!(f, "#<recur>"),
            Val::Fn { arities, .. } => {
                write!(f, "#<fn [{}]>", fmt_arity_desc(arities))
            }
            Val::Macro { arities, .. } => {
                write!(f, "#<macro [{}]>", fmt_arity_desc(arities))
            }
            Val::Effect { effect_type, data } => {
                write!(f, "#<effect :{effect_type} {data}>")
            }
            Val::NativeFn { name, .. } => write!(f, "#<native-fn {name}>"),
            Val::AsyncNativeFn { name, .. } => write!(f, "#<async-native-fn {name}>"),
            Val::Cap { name, .. } => write!(f, "#<cap {name}>"),
            Val::Cell { wasm, schema, caps } => {
                write!(f, "#<cell {} bytes", wasm.len())?;
                if let Some(s) = schema {
                    write!(f, ", schema {} bytes", s.len())?;
                }
                if !caps.is_empty() {
                    let names: Vec<&str> = caps.iter().map(|(n, _)| n.as_str()).collect();
                    write!(f, ", caps [{}]", names.join(" "))?;
                }
                write!(f, ">")
            }
            Val::Resume(val) => write!(f, "#<resume {val}>"),
        }
    }
}

fn fmt_arity_desc(arities: &[FnArity]) -> String {
    arities
        .iter()
        .map(|a| {
            let n = a.params.len();
            if a.variadic.is_some() {
                format!("{n}+")
            } else {
                n.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn fmt_seq(
    f: &mut core::fmt::Formatter<'_>,
    open: &str,
    close: &str,
    items: &[Val],
) -> core::fmt::Result {
    write!(f, "{open}")?;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            write!(f, " ")?;
        }
        write!(f, "{item}")?;
    }
    write!(f, "{close}")
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Read a single form from `input`.
///
/// Returns an error if the input is empty, malformed, or contains trailing
/// tokens after the first complete form.
pub fn read(input: &str) -> Result<Val, String> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err("empty input".into());
    }
    let (val, rest) = parse_tokens(&tokens)?;
    if !rest.is_empty() {
        return Err("unexpected tokens after expression".into());
    }
    Ok(val)
}

/// Read all top-level forms from `input`.
///
/// Useful for config files that contain a single data literal or multiple
/// sequential forms.
pub fn read_many(input: &str) -> Result<Vec<Val>, String> {
    let tokens = tokenize(input)?;
    let mut results = Vec::new();
    let mut rest = tokens.as_slice();
    while !rest.is_empty() {
        let (val, remaining) = parse_tokens(rest)?;
        results.push(val);
        rest = remaining;
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Prelude
// ---------------------------------------------------------------------------

/// The Glia prelude: standard derived forms (when, and, or, defn, cond, not).
pub const PRELUDE: &str = include_str!("prelude.glia");

/// Load the prelude macros into the given environment.
///
/// Parses and evaluates each form in `prelude.glia`. This should be called
/// once at boot before init.d or shell evaluation. Prelude errors are fatal.
pub async fn load_prelude<D: eval::Dispatch>(env: &mut eval::Env, dispatch: &mut D) {
    let forms = read_many(PRELUDE).expect("prelude: parse error");
    for form in &forms {
        eval::eval_toplevel(form, env, dispatch)
            .await
            .expect("prelude: eval error");
    }
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

/// Token types produced by the tokenizer.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Open,          // (
    Close,         // )
    VecOpen,       // [
    VecClose,      // ]
    MapOpen,       // {
    MapClose,      // }
    SetOpen,       // #{
    Quote,         // '
    Backtick,      // `
    Unquote,       // ~
    SpliceUnquote, // ~@
    Atom(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            // Whitespace (commas are whitespace in Clojure)
            ' ' | '\t' | '\r' | '\n' | ',' => {
                chars.next();
            }
            '(' => {
                tokens.push(Token::Open);
                chars.next();
            }
            ')' => {
                tokens.push(Token::Close);
                chars.next();
            }
            '[' => {
                tokens.push(Token::VecOpen);
                chars.next();
            }
            ']' => {
                tokens.push(Token::VecClose);
                chars.next();
            }
            '{' => {
                tokens.push(Token::MapOpen);
                chars.next();
            }
            '}' => {
                tokens.push(Token::MapClose);
                chars.next();
            }
            '\'' => {
                tokens.push(Token::Quote);
                chars.next();
            }
            '`' => {
                tokens.push(Token::Backtick);
                chars.next();
            }
            '~' => {
                chars.next();
                if chars.peek() == Some(&'@') {
                    chars.next();
                    tokens.push(Token::SpliceUnquote);
                } else {
                    tokens.push(Token::Unquote);
                }
            }
            '#' => {
                chars.next();
                match chars.peek() {
                    Some('{') => {
                        chars.next();
                        tokens.push(Token::SetOpen);
                    }
                    _ => return Err("unexpected character after #".into()),
                }
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('\\') => s.push('\\'),
                            Some('"') => s.push('"'),
                            Some(esc) => {
                                s.push('\\');
                                s.push(esc);
                            }
                            None => return Err("unterminated string escape".into()),
                        },
                        Some('"') => break,
                        Some(ch) => s.push(ch),
                        None => return Err("unterminated string".into()),
                    }
                }
                tokens.push(Token::Atom(format!("\"{s}\"")));
            }
            ';' => {
                // Line comment — skip to end of line
                while chars.peek().is_some_and(|&c| c != '\n') {
                    chars.next();
                }
            }
            _ => {
                let mut atom = String::new();
                while let Some(&c) = chars.peek() {
                    if matches!(
                        c,
                        ' ' | '\t'
                            | '\r'
                            | '\n'
                            | ','
                            | '('
                            | ')'
                            | '['
                            | ']'
                            | '{'
                            | '}'
                            | '\''
                            | '"'
                            | ';'
                            | '`'
                            | '~'
                    ) {
                        break;
                    }
                    atom.push(chars.next().expect("peek succeeded"));
                }
                tokens.push(Token::Atom(atom));
            }
        }
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse tokens in normal mode: backtick triggers syntax-quote transformation.
fn parse_tokens(tokens: &[Token]) -> Result<(Val, &[Token]), String> {
    if tokens.is_empty() {
        return Err("unexpected end of input".into());
    }
    match &tokens[0] {
        Token::Open => parse_seq(&tokens[1..], Token::Close, Val::List, false),
        Token::VecOpen => parse_seq(&tokens[1..], Token::VecClose, Val::Vector, false),
        Token::MapOpen => parse_map(&tokens[1..]),
        Token::SetOpen => parse_set(&tokens[1..]),
        Token::Quote => {
            let (inner, rest) = parse_tokens(&tokens[1..])?;
            Ok((Val::List(vec![Val::Sym("quote".into()), inner]), rest))
        }
        Token::Backtick => {
            // Parse the inner form in "raw" mode so nested backticks produce
            // (syntax-quote ...) marker forms instead of being eagerly expanded.
            let (inner, rest) = parse_tokens_raw(&tokens[1..])?;
            let transformed = transform_syntax_quote(&inner, 0)?;
            Ok((transformed, rest))
        }
        Token::Unquote => {
            let (inner, rest) = parse_tokens(&tokens[1..])?;
            Ok((Val::List(vec![Val::Sym("unquote".into()), inner]), rest))
        }
        Token::SpliceUnquote => {
            let (inner, rest) = parse_tokens(&tokens[1..])?;
            Ok((
                Val::List(vec![Val::Sym("splice-unquote".into()), inner]),
                rest,
            ))
        }
        Token::Close => Err("unexpected )".into()),
        Token::VecClose => Err("unexpected ]".into()),
        Token::MapClose => Err("unexpected }".into()),
        Token::Atom(a) => Ok((parse_atom(a), &tokens[1..])),
    }
}

/// Parse tokens in "raw" mode: backtick, unquote, and splice-unquote produce
/// marker forms `(syntax-quote ...)`, `(unquote ...)`, `(splice-unquote ...)`
/// without triggering syntax-quote transformation. This allows
/// `transform_syntax_quote` to see the full nested structure and track depth.
fn parse_tokens_raw(tokens: &[Token]) -> Result<(Val, &[Token]), String> {
    if tokens.is_empty() {
        return Err("unexpected end of input".into());
    }
    match &tokens[0] {
        Token::Open => parse_seq(&tokens[1..], Token::Close, Val::List, true),
        Token::VecOpen => parse_seq(&tokens[1..], Token::VecClose, Val::Vector, true),
        Token::MapOpen => parse_map_raw(&tokens[1..]),
        Token::SetOpen => parse_set_raw(&tokens[1..]),
        Token::Quote => {
            let (inner, rest) = parse_tokens_raw(&tokens[1..])?;
            Ok((Val::List(vec![Val::Sym("quote".into()), inner]), rest))
        }
        Token::Backtick => {
            let (inner, rest) = parse_tokens_raw(&tokens[1..])?;
            Ok((
                Val::List(vec![Val::Sym("syntax-quote".into()), inner]),
                rest,
            ))
        }
        Token::Unquote => {
            let (inner, rest) = parse_tokens_raw(&tokens[1..])?;
            Ok((Val::List(vec![Val::Sym("unquote".into()), inner]), rest))
        }
        Token::SpliceUnquote => {
            let (inner, rest) = parse_tokens_raw(&tokens[1..])?;
            Ok((
                Val::List(vec![Val::Sym("splice-unquote".into()), inner]),
                rest,
            ))
        }
        Token::Close => Err("unexpected )".into()),
        Token::VecClose => Err("unexpected ]".into()),
        Token::MapClose => Err("unexpected }".into()),
        Token::Atom(a) => Ok((parse_atom(a), &tokens[1..])),
    }
}

fn parse_seq<F>(
    tokens: &[Token],
    close: Token,
    wrap: F,
    raw: bool,
) -> Result<(Val, &[Token]), String>
where
    F: FnOnce(Vec<Val>) -> Val,
{
    let mut items = Vec::new();
    let mut rest = tokens;
    loop {
        if rest.is_empty() {
            return Err(format!("unclosed {}", close_name(&close)));
        }
        if rest[0] == close {
            return Ok((wrap(items), &rest[1..]));
        }
        let (val, new_rest) = if raw {
            parse_tokens_raw(rest)?
        } else {
            parse_tokens(rest)?
        };
        items.push(val);
        rest = new_rest;
    }
}

fn parse_map(tokens: &[Token]) -> Result<(Val, &[Token]), String> {
    parse_map_inner(tokens, false)
}

fn parse_map_raw(tokens: &[Token]) -> Result<(Val, &[Token]), String> {
    parse_map_inner(tokens, true)
}

fn parse_map_inner(tokens: &[Token], raw: bool) -> Result<(Val, &[Token]), String> {
    let mut pairs = Vec::new();
    let mut rest = tokens;
    loop {
        if rest.is_empty() {
            return Err("unclosed map".into());
        }
        if rest[0] == Token::MapClose {
            return Ok((Val::Map(ValMap::from_pairs(pairs)), &rest[1..]));
        }
        let (key, after_key) = if raw {
            parse_tokens_raw(rest)?
        } else {
            parse_tokens(rest)?
        };
        if after_key.is_empty() || after_key[0] == Token::MapClose {
            return Err("map must have an even number of elements".into());
        }
        let (val, after_val) = if raw {
            parse_tokens_raw(after_key)?
        } else {
            parse_tokens(after_key)?
        };
        pairs.push((key, val));
        rest = after_val;
    }
}

fn parse_set(tokens: &[Token]) -> Result<(Val, &[Token]), String> {
    parse_set_inner(tokens, false)
}

fn parse_set_raw(tokens: &[Token]) -> Result<(Val, &[Token]), String> {
    parse_set_inner(tokens, true)
}

fn parse_set_inner(tokens: &[Token], raw: bool) -> Result<(Val, &[Token]), String> {
    let mut items: Vec<Val> = Vec::new();
    let mut rest = tokens;
    loop {
        if rest.is_empty() {
            return Err("unclosed set".into());
        }
        if rest[0] == Token::MapClose {
            return Ok((Val::Set(items), &rest[1..]));
        }
        let (val, new_rest) = if raw {
            parse_tokens_raw(rest)?
        } else {
            parse_tokens(rest)?
        };
        // Check for duplicates (linear scan — fine for config-sized data)
        if items.iter().any(|existing| existing == &val) {
            return Err(format!("duplicate set element: {val}"));
        }
        items.push(val);
        rest = new_rest;
    }
}

fn close_name(token: &Token) -> &'static str {
    match token {
        Token::Close => "list",
        Token::VecClose => "vector",
        Token::MapClose => "map/set",
        _ => "collection",
    }
}

// ---------------------------------------------------------------------------
// Syntax-quote transformer
// ---------------------------------------------------------------------------

/// Check whether `val` is an `(unquote expr)` marker form.
fn is_unquote(val: &Val) -> bool {
    matches!(val, Val::List(items) if items.len() == 2 && matches!(&items[0], Val::Sym(s) if s == "unquote"))
}

/// Check whether `val` is a `(splice-unquote expr)` marker form.
fn is_splice_unquote(val: &Val) -> bool {
    matches!(val, Val::List(items) if items.len() == 2 && matches!(&items[0], Val::Sym(s) if s == "splice-unquote"))
}

/// Check whether `val` is a `(syntax-quote expr)` marker form.
fn is_syntax_quote(val: &Val) -> bool {
    matches!(val, Val::List(items) if items.len() == 2 && matches!(&items[0], Val::Sym(s) if s == "syntax-quote"))
}

/// Transform a syntax-quoted form into explicit `list`/`concat`/`quote` calls.
///
/// The reader converts `` `form `` by parsing `form` in raw mode (which
/// produces `(syntax-quote ...)`, `(unquote ...)`, and `(splice-unquote ...)`
/// marker sub-forms) and then calling this function to produce the expansion.
///
/// `depth` tracks the nesting level of syntax-quotes:
/// - Backtick (syntax-quote) increments depth
/// - Tilde (unquote) and tilde-at (splice-unquote) decrement depth
/// - Only at depth 0 do unquote/splice-unquote actually resolve
/// - At deeper levels they are preserved as literal forms
fn transform_syntax_quote(val: &Val, depth: usize) -> Result<Val, String> {
    match val {
        // ~expr
        _ if is_unquote(val) => {
            if let Val::List(items) = val {
                if depth == 0 {
                    // depth 0 → resolve (pass through for runtime evaluation)
                    Ok(items[1].clone())
                } else {
                    // depth > 0: preserve as literal (unquote <recurse at depth-1>)
                    let inner_transformed = transform_syntax_quote(&items[1], depth - 1)?;
                    Ok(Val::List(vec![
                        Val::Sym("concat".into()),
                        Val::List(vec![
                            Val::Sym("list".into()),
                            Val::List(vec![Val::Sym("quote".into()), Val::Sym("unquote".into())]),
                        ]),
                        Val::List(vec![Val::Sym("list".into()), inner_transformed]),
                    ]))
                }
            } else {
                unreachable!()
            }
        }

        // ~@expr
        _ if is_splice_unquote(val) => {
            if depth == 0 {
                Err("splice-unquote (~@) not inside list".into())
            } else {
                // depth > 0: preserve as literal (splice-unquote <recurse at depth-1>)
                if let Val::List(items) = val {
                    let inner_transformed = transform_syntax_quote(&items[1], depth - 1)?;
                    Ok(Val::List(vec![
                        Val::Sym("concat".into()),
                        Val::List(vec![
                            Val::Sym("list".into()),
                            Val::List(vec![
                                Val::Sym("quote".into()),
                                Val::Sym("splice-unquote".into()),
                            ]),
                        ]),
                        Val::List(vec![Val::Sym("list".into()), inner_transformed]),
                    ]))
                } else {
                    unreachable!()
                }
            }
        }

        // Nested syntax-quote: `expr inside a syntax-quote → increment depth
        _ if is_syntax_quote(val) => {
            if let Val::List(items) = val {
                let inner_transformed = transform_syntax_quote(&items[1], depth + 1)?;
                // Preserve as (syntax-quote <recursed>)
                Ok(Val::List(vec![
                    Val::Sym("concat".into()),
                    Val::List(vec![
                        Val::Sym("list".into()),
                        Val::List(vec![
                            Val::Sym("quote".into()),
                            Val::Sym("syntax-quote".into()),
                        ]),
                    ]),
                    Val::List(vec![Val::Sym("list".into()), inner_transformed]),
                ]))
            } else {
                unreachable!()
            }
        }

        // (quote expr) inside syntax-quote → preserve as literal, don't recurse
        Val::List(items)
            if items.len() == 2 && matches!(&items[0], Val::Sym(s) if s == "quote") =>
        {
            // Return (concat (list (quote quote)) (list (quote expr)))
            // which evaluates to the literal (quote expr)
            Ok(Val::List(vec![
                Val::Sym("concat".into()),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![Val::Sym("quote".into()), Val::Sym("quote".into())]),
                ]),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![Val::Sym("quote".into()), items[1].clone()]),
                ]),
            ]))
        }

        // (a ~b ~@c d) → (concat (list (quote a)) (list b) c (list (quote d)))
        Val::List(items) => {
            if items.is_empty() {
                return Ok(Val::List(vec![Val::Sym("list".into())]));
            }
            let mut segments = Vec::new();
            for item in items {
                if is_unquote(item) {
                    if let Val::List(inner) = item {
                        if depth == 0 {
                            // ~x at depth 0 → (list x) — resolve
                            segments
                                .push(Val::List(vec![Val::Sym("list".into()), inner[1].clone()]));
                        } else {
                            // ~x at depth > 0 → preserve as literal
                            let inner_transformed = transform_syntax_quote(&inner[1], depth - 1)?;
                            segments.push(Val::List(vec![
                                Val::Sym("list".into()),
                                Val::List(vec![
                                    Val::Sym("concat".into()),
                                    Val::List(vec![
                                        Val::Sym("list".into()),
                                        Val::List(vec![
                                            Val::Sym("quote".into()),
                                            Val::Sym("unquote".into()),
                                        ]),
                                    ]),
                                    Val::List(vec![Val::Sym("list".into()), inner_transformed]),
                                ]),
                            ]));
                        }
                    }
                } else if is_splice_unquote(item) {
                    if let Val::List(inner) = item {
                        if depth == 0 {
                            // ~@x at depth 0 → x (concat will flatten)
                            segments.push(inner[1].clone());
                        } else {
                            // ~@x at depth > 0 → preserve as literal
                            let inner_transformed = transform_syntax_quote(&inner[1], depth - 1)?;
                            segments.push(Val::List(vec![
                                Val::Sym("list".into()),
                                Val::List(vec![
                                    Val::Sym("concat".into()),
                                    Val::List(vec![
                                        Val::Sym("list".into()),
                                        Val::List(vec![
                                            Val::Sym("quote".into()),
                                            Val::Sym("splice-unquote".into()),
                                        ]),
                                    ]),
                                    Val::List(vec![Val::Sym("list".into()), inner_transformed]),
                                ]),
                            ]));
                        }
                    }
                } else if is_syntax_quote(item) {
                    if let Val::List(inner) = item {
                        // Nested backtick in a list element → increment depth
                        let inner_transformed = transform_syntax_quote(&inner[1], depth + 1)?;
                        segments.push(Val::List(vec![
                            Val::Sym("list".into()),
                            Val::List(vec![
                                Val::Sym("concat".into()),
                                Val::List(vec![
                                    Val::Sym("list".into()),
                                    Val::List(vec![
                                        Val::Sym("quote".into()),
                                        Val::Sym("syntax-quote".into()),
                                    ]),
                                ]),
                                Val::List(vec![Val::Sym("list".into()), inner_transformed]),
                            ]),
                        ]));
                    }
                } else {
                    // Recurse and wrap in (list ...)
                    let quoted = transform_syntax_quote(item, depth)?;
                    segments.push(Val::List(vec![Val::Sym("list".into()), quoted]));
                }
            }
            let mut result = vec![Val::Sym("concat".into())];
            result.extend(segments);
            Ok(Val::List(result))
        }

        // [a ~b] → (vec (concat ...))
        Val::Vector(items) => {
            let as_list = transform_syntax_quote(&Val::List(items.clone()), depth)?;
            Ok(Val::List(vec![Val::Sym("vec".into()), as_list]))
        }

        // Symbols → (quote sym)
        Val::Sym(_) => Ok(Val::List(vec![Val::Sym("quote".into()), val.clone()])),

        // Self-evaluating: nil, bool, int, float, str, keyword → as-is
        Val::Nil | Val::Bool(_) | Val::Int(_) | Val::Float(_) | Val::Str(_) | Val::Keyword(_) => {
            Ok(val.clone())
        }

        // Map: recursively transform keys and values, reconstruct with assoc.
        // `{:a 1 :b ~x}` becomes `(assoc {} :a 1 :b x)`
        Val::Map(m) => {
            let mut assoc_args = vec![Val::Sym("assoc".into()), Val::Map(ValMap::new())];
            for (k, v) in m.iter() {
                let tk = transform_syntax_quote(k, depth)?;
                let tv = transform_syntax_quote(v, depth)?;
                assoc_args.push(tk);
                assoc_args.push(tv);
            }
            Ok(Val::List(assoc_args))
        }
        Val::Set(_) => Err("syntax-quote of sets not yet supported".into()),

        // Fn/Macro/Recur/Bytes — shouldn't appear in parsed forms
        other => Err(format!("syntax-quote: unexpected value {other}")),
    }
}

fn parse_atom(s: &str) -> Val {
    // String literal
    if s.starts_with('"') {
        let inner = &s[1..s.len() - 1];
        return Val::Str(inner.to_string());
    }

    // Keyword
    if let Some(kw) = s.strip_prefix(':') {
        return Val::Keyword(kw.to_string());
    }

    // Reserved words
    match s {
        "nil" => return Val::Nil,
        "true" => return Val::Bool(true),
        "false" => return Val::Bool(false),
        _ => {}
    }

    // Integer
    if let Ok(n) = s.parse::<i64>() {
        return Val::Int(n);
    }

    // Float
    if let Ok(n) = s.parse::<f64>() {
        // Only parse as float if the token looks numeric (not something like "Infinity")
        if s.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+' || c == '.') {
            return Val::Float(n);
        }
    }

    // Symbol (fallback)
    Val::Sym(s.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- tokenizer ---

    #[test]
    fn tokenize_symbol() {
        let tokens = tokenize("hello").unwrap();
        assert_eq!(tokens, vec![Token::Atom("hello".into())]);
    }

    #[test]
    fn tokenize_parens() {
        let tokens = tokenize("(foo bar)").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Open,
                Token::Atom("foo".into()),
                Token::Atom("bar".into()),
                Token::Close
            ]
        );
    }

    #[test]
    fn tokenize_string() {
        let tokens = tokenize("\"hello world\"").unwrap();
        assert_eq!(tokens, vec![Token::Atom("\"hello world\"".into())]);
    }

    #[test]
    fn tokenize_string_with_escape() {
        let tokens = tokenize(r#""hello \"world\"""#).unwrap();
        assert_eq!(tokens, vec![Token::Atom("\"hello \"world\"\"".into())]);
    }

    #[test]
    fn tokenize_unterminated_string() {
        assert!(tokenize("\"hello").is_err());
    }

    #[test]
    fn tokenize_unterminated_escape() {
        assert!(tokenize(r#""hello\"#).is_err());
    }

    #[test]
    fn tokenize_comment() {
        let tokens = tokenize("; this is a comment").unwrap();
        assert!(tokens.is_empty());
    }

    #[test]
    fn tokenize_comment_then_code() {
        let tokens = tokenize("; comment\nhello").unwrap();
        assert_eq!(tokens, vec![Token::Atom("hello".into())]);
    }

    #[test]
    fn tokenize_nested() {
        let tokens = tokenize("(host (id))").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Open,
                Token::Atom("host".into()),
                Token::Open,
                Token::Atom("id".into()),
                Token::Close,
                Token::Close
            ]
        );
    }

    #[test]
    fn tokenize_whitespace_variants() {
        let tokens = tokenize("  a\tb\r\nc  ").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Atom("a".into()),
                Token::Atom("b".into()),
                Token::Atom("c".into())
            ]
        );
    }

    #[test]
    fn tokenize_empty() {
        let tokens = tokenize("").unwrap();
        assert!(tokens.is_empty());
    }

    #[test]
    fn tokenize_only_whitespace() {
        let tokens = tokenize("   \t\n  ").unwrap();
        assert!(tokens.is_empty());
    }

    #[test]
    fn tokenize_commas_as_whitespace() {
        let tokens = tokenize("[1, 2, 3]").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::VecOpen,
                Token::Atom("1".into()),
                Token::Atom("2".into()),
                Token::Atom("3".into()),
                Token::VecClose
            ]
        );
    }

    #[test]
    fn tokenize_brackets() {
        let tokens = tokenize("[a b]").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::VecOpen,
                Token::Atom("a".into()),
                Token::Atom("b".into()),
                Token::VecClose
            ]
        );
    }

    #[test]
    fn tokenize_braces() {
        let tokens = tokenize("{:a 1}").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::MapOpen,
                Token::Atom(":a".into()),
                Token::Atom("1".into()),
                Token::MapClose
            ]
        );
    }

    #[test]
    fn tokenize_set() {
        let tokens = tokenize("#{:a :b}").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::SetOpen,
                Token::Atom(":a".into()),
                Token::Atom(":b".into()),
                Token::MapClose
            ]
        );
    }

    #[test]
    fn tokenize_hash_error() {
        assert!(tokenize("#x").is_err());
    }

    // --- parser: atoms ---

    #[test]
    fn parse_symbol() {
        match read("hello").unwrap() {
            Val::Sym(s) => assert_eq!(s, "hello"),
            other => panic!("expected Sym, got {other:?}"),
        }
    }

    #[test]
    fn parse_string() {
        match read("\"hello\"").unwrap() {
            Val::Str(s) => assert_eq!(s, "hello"),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn parse_nil() {
        assert!(matches!(read("nil").unwrap(), Val::Nil));
    }

    #[test]
    fn parse_true() {
        assert!(matches!(read("true").unwrap(), Val::Bool(true)));
    }

    #[test]
    fn parse_false() {
        assert!(matches!(read("false").unwrap(), Val::Bool(false)));
    }

    #[test]
    fn parse_keyword() {
        match read(":port").unwrap() {
            Val::Keyword(k) => assert_eq!(k, "port"),
            other => panic!("expected Keyword, got {other:?}"),
        }
    }

    #[test]
    fn parse_keyword_with_hyphen() {
        match read(":key-file").unwrap() {
            Val::Keyword(k) => assert_eq!(k, "key-file"),
            other => panic!("expected Keyword, got {other:?}"),
        }
    }

    #[test]
    fn parse_integer() {
        assert_eq!(read("42").unwrap(), Val::Int(42));
    }

    #[test]
    fn parse_negative_integer() {
        assert_eq!(read("-7").unwrap(), Val::Int(-7));
    }

    #[test]
    fn parse_zero() {
        assert_eq!(read("0").unwrap(), Val::Int(0));
    }

    #[test]
    fn parse_float() {
        assert_eq!(read("2.5").unwrap(), Val::Float(2.5));
    }

    #[test]
    fn parse_negative_float() {
        assert_eq!(read("-0.5").unwrap(), Val::Float(-0.5));
    }

    #[test]
    fn parse_scientific_notation() {
        assert_eq!(read("1e10").unwrap(), Val::Float(1e10));
    }

    #[test]
    fn parse_scientific_negative_exp() {
        assert_eq!(read("1.5e-3").unwrap(), Val::Float(1.5e-3));
    }

    // --- parser: collections ---

    #[test]
    fn parse_list() {
        match read("(a b c)").unwrap() {
            Val::List(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(&items[0], Val::Sym(s) if s == "a"));
                assert!(matches!(&items[1], Val::Sym(s) if s == "b"));
                assert!(matches!(&items[2], Val::Sym(s) if s == "c"));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_list() {
        match read("()").unwrap() {
            Val::List(items) => assert!(items.is_empty()),
            other => panic!("expected empty List, got {other:?}"),
        }
    }

    #[test]
    fn parse_nested_list() {
        match read("(host (id))").unwrap() {
            Val::List(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(&items[0], Val::Sym(s) if s == "host"));
                match &items[1] {
                    Val::List(inner) => {
                        assert_eq!(inner.len(), 1);
                        assert!(matches!(&inner[0], Val::Sym(s) if s == "id"));
                    }
                    other => panic!("expected inner List, got {other:?}"),
                }
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn parse_vector() {
        match read("[1 2 3]").unwrap() {
            Val::Vector(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Val::Int(1));
                assert_eq!(items[1], Val::Int(2));
                assert_eq!(items[2], Val::Int(3));
            }
            other => panic!("expected Vector, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_vector() {
        match read("[]").unwrap() {
            Val::Vector(items) => assert!(items.is_empty()),
            other => panic!("expected empty Vector, got {other:?}"),
        }
    }

    #[test]
    fn parse_vector_commas() {
        // Commas are whitespace
        match read("[1, 2, 3]").unwrap() {
            Val::Vector(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Val::Int(1));
            }
            other => panic!("expected Vector, got {other:?}"),
        }
    }

    #[test]
    fn parse_map() {
        match read("{:a 1 :b 2}").unwrap() {
            Val::Map(m) => {
                assert_eq!(m.len(), 2);
                assert_eq!(m.get(&Val::Keyword("a".into())), Some(&Val::Int(1)));
                assert_eq!(m.get(&Val::Keyword("b".into())), Some(&Val::Int(2)));
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_map() {
        match read("{}").unwrap() {
            Val::Map(m) => assert!(m.is_empty()),
            other => panic!("expected empty Map, got {other:?}"),
        }
    }

    #[test]
    fn parse_map_odd_elements() {
        assert!(read("{:a 1 :b}").is_err());
    }

    #[test]
    fn parse_set() {
        match read("#{:a :b :c}").unwrap() {
            Val::Set(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Val::Keyword("a".into()));
                assert_eq!(items[1], Val::Keyword("b".into()));
                assert_eq!(items[2], Val::Keyword("c".into()));
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn parse_empty_set() {
        match read("#{}").unwrap() {
            Val::Set(items) => assert!(items.is_empty()),
            other => panic!("expected empty Set, got {other:?}"),
        }
    }

    #[test]
    fn parse_set_duplicates() {
        assert!(read("#{:a :b :a}").is_err());
    }

    // --- parser: mixed/nested ---

    #[test]
    fn parse_mixed_types() {
        match read("(echo \"hello\" nil)").unwrap() {
            Val::List(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(&items[0], Val::Sym(s) if s == "echo"));
                assert!(matches!(&items[1], Val::Str(s) if s == "hello"));
                assert!(matches!(&items[2], Val::Nil));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn parse_nested_config() {
        let input = r#"{:images ["a" "b"] :flags #{:verbose}}"#;
        match read(input).unwrap() {
            Val::Map(m) => {
                assert_eq!(m.len(), 2);
                match m.get(&Val::Keyword("images".into())) {
                    Some(Val::Vector(v)) => {
                        assert_eq!(v.len(), 2);
                        assert_eq!(v[0], Val::Str("a".into()));
                        assert_eq!(v[1], Val::Str("b".into()));
                    }
                    other => panic!("expected Vector for :images, got {other:?}"),
                }
                match m.get(&Val::Keyword("flags".into())) {
                    Some(Val::Set(s)) => {
                        assert_eq!(s.len(), 1);
                        assert_eq!(s[0], Val::Keyword("verbose".into()));
                    }
                    other => panic!("expected Set for :flags, got {other:?}"),
                }
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn parse_config_example() {
        let input = r#"
;; wetware node configuration
{:port     2025
 :key-file "~/.ww/key"
 :images   ["images/my-app" "images/shell"]}
"#;
        match read(input).unwrap() {
            Val::Map(m) => {
                assert_eq!(m.len(), 3);
                assert_eq!(m.get(&Val::Keyword("port".into())), Some(&Val::Int(2025)));
                assert_eq!(
                    m.get(&Val::Keyword("key-file".into())),
                    Some(&Val::Str("~/.ww/key".into()))
                );
                match m.get(&Val::Keyword("images".into())) {
                    Some(Val::Vector(v)) => assert_eq!(v.len(), 2),
                    other => panic!("expected Vector, got {other:?}"),
                }
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    // --- parser: errors ---

    #[test]
    fn parse_unclosed_paren() {
        assert!(read("(a b").is_err());
    }

    #[test]
    fn parse_unexpected_close_paren() {
        assert!(read(")").is_err());
    }

    #[test]
    fn parse_unexpected_close_bracket() {
        assert!(read("]").is_err());
    }

    #[test]
    fn parse_unexpected_close_brace() {
        assert!(read("}").is_err());
    }

    #[test]
    fn parse_trailing_tokens() {
        assert!(read("a b").is_err());
    }

    #[test]
    fn parse_empty_input() {
        assert!(read("").is_err());
    }

    // --- read_many ---

    #[test]
    fn read_many_single() {
        let vals = read_many("42").unwrap();
        assert_eq!(vals.len(), 1);
        assert_eq!(vals[0], Val::Int(42));
    }

    #[test]
    fn read_many_multiple() {
        let vals = read_many("(a) (b) (c)").unwrap();
        assert_eq!(vals.len(), 3);
    }

    #[test]
    fn read_many_empty() {
        let vals = read_many("").unwrap();
        assert!(vals.is_empty());
    }

    #[test]
    fn read_many_whitespace_only() {
        let vals = read_many("  ; just a comment\n  ").unwrap();
        assert!(vals.is_empty());
    }

    #[test]
    fn read_many_initd_script() {
        let script = r#"
; Chess init.d script
(host listen "chess" (ipfs cat "bin/chess-demo.wasm"))
(routing provide (routing hash "ww.chess.v1"))
(executor run (ipfs cat "bin/chess-demo.wasm")
  :env {"WW_NS" "ww.chess.v1"})
"#;
        let forms = read_many(script).unwrap();
        assert_eq!(forms.len(), 3);

        // First form: (host listen "chess" (ipfs cat "bin/chess-demo.wasm"))
        match &forms[0] {
            Val::List(items) => {
                assert_eq!(items.len(), 4);
                assert_eq!(items[0], Val::Sym("host".into()));
                assert_eq!(items[1], Val::Sym("listen".into()));
                assert_eq!(items[2], Val::Str("chess".into()));
                // Nested list: (ipfs cat "bin/chess-demo.wasm")
                match &items[3] {
                    Val::List(inner) => {
                        assert_eq!(inner.len(), 3);
                        assert_eq!(inner[0], Val::Sym("ipfs".into()));
                        assert_eq!(inner[1], Val::Sym("cat".into()));
                        assert_eq!(inner[2], Val::Str("bin/chess-demo.wasm".into()));
                    }
                    other => panic!("expected nested list, got {other}"),
                }
            }
            other => panic!("expected list, got {other}"),
        }

        // Third form has :env keyword and a map
        match &forms[2] {
            Val::List(items) => {
                assert_eq!(items[0], Val::Sym("executor".into()));
                assert_eq!(items[1], Val::Sym("run".into()));
                // items[2] is nested (ipfs cat ...)
                assert_eq!(items[3], Val::Keyword("env".into()));
                match &items[4] {
                    Val::Map(pairs) => {
                        assert_eq!(pairs.len(), 1);
                    }
                    other => panic!("expected map, got {other}"),
                }
            }
            other => panic!("expected list, got {other}"),
        }
    }

    // --- Display ---

    #[test]
    fn display_sym() {
        assert_eq!(format!("{}", Val::Sym("foo".into())), "foo");
    }

    #[test]
    fn display_str() {
        assert_eq!(format!("{}", Val::Str("bar".into())), "\"bar\"");
    }

    #[test]
    fn display_nil() {
        assert_eq!(format!("{}", Val::Nil), "nil");
    }

    #[test]
    fn display_bool() {
        assert_eq!(format!("{}", Val::Bool(true)), "true");
        assert_eq!(format!("{}", Val::Bool(false)), "false");
    }

    #[test]
    fn display_int() {
        assert_eq!(format!("{}", Val::Int(42)), "42");
        assert_eq!(format!("{}", Val::Int(-7)), "-7");
    }

    #[test]
    fn display_float() {
        assert_eq!(format!("{}", Val::Float(2.5)), "2.5");
        assert_eq!(format!("{}", Val::Float(1.0)), "1.0");
    }

    #[test]
    fn display_keyword() {
        assert_eq!(format!("{}", Val::Keyword("port".into())), ":port");
    }

    #[test]
    fn display_list() {
        let v = Val::List(vec![Val::Sym("host".into()), Val::Str("addr".into())]);
        assert_eq!(format!("{v}"), "(host \"addr\")");
    }

    #[test]
    fn display_empty_list() {
        assert_eq!(format!("{}", Val::List(vec![])), "()");
    }

    #[test]
    fn display_vector() {
        let v = Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]);
        assert_eq!(format!("{v}"), "[1 2 3]");
    }

    #[test]
    fn display_map() {
        let v = Val::Map(ValMap::from_pairs(vec![
            (Val::Keyword("a".into()), Val::Int(1)),
            (Val::Keyword("b".into()), Val::Int(2)),
        ]));
        let rendered = format!("{v}");
        assert!(rendered == "{:a 1 :b 2}" || rendered == "{:b 2 :a 1}");
    }

    #[test]
    fn display_set() {
        let v = Val::Set(vec![Val::Keyword("a".into()), Val::Keyword("b".into())]);
        assert_eq!(format!("{v}"), "#{:a :b}");
    }

    #[test]
    fn display_nested() {
        let v = Val::List(vec![
            Val::Sym("a".into()),
            Val::List(vec![Val::Sym("b".into()), Val::Nil]),
        ]);
        assert_eq!(format!("{v}"), "(a (b nil))");
    }

    // --- round-trip ---

    #[test]
    fn roundtrip_simple() {
        let input = "(executor echo \"hello world\")";
        let val = read(input).unwrap();
        let output = format!("{val}");
        assert_eq!(output, input);
    }

    #[test]
    fn roundtrip_nested() {
        let input = "(session host id)";
        let val = read(input).unwrap();
        let output = format!("{val}");
        assert_eq!(output, input);
    }

    #[test]
    fn roundtrip_vector() {
        let input = "[1 2 3]";
        let val = read(input).unwrap();
        assert_eq!(format!("{val}"), input);
    }

    #[test]
    fn roundtrip_map() {
        // im::HashMap doesn't preserve insertion order, so check
        // that the round-trip produces an equivalent map, not identical text.
        let input = "{:a 1 :b 2}";
        let val = read(input).unwrap();
        let output = format!("{val}");
        let reparsed = read(&output).unwrap();
        assert_eq!(val, reparsed);
    }

    #[test]
    fn roundtrip_set() {
        let input = "#{:x :y}";
        let val = read(input).unwrap();
        assert_eq!(format!("{val}"), input);
    }

    #[test]
    fn roundtrip_keyword() {
        let input = ":my-key";
        let val = read(input).unwrap();
        assert_eq!(format!("{val}"), input);
    }

    #[test]
    fn roundtrip_bool() {
        assert_eq!(format!("{}", read("true").unwrap()), "true");
        assert_eq!(format!("{}", read("false").unwrap()), "false");
    }

    // --- session prefix resolution (ported from kernel) ---

    #[test]
    fn parse_session_prefixed() {
        match read("(session::host id)").unwrap() {
            Val::List(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(&items[0], Val::Sym(s) if s == "session::host"));
                assert!(matches!(&items[1], Val::Sym(s) if s == "id"));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    // --- init.d service declaration parsing ---

    /// Helper matching the kernel's `map_get_str` — extract a string value
    /// for a keyword key from a glia Map.
    fn map_get_str<'a>(m: &'a ValMap, key: &str) -> Option<&'a str> {
        m.get(&Val::Keyword(key.into())).and_then(|v| match v {
            Val::Str(s) => Some(s.as_str()),
            _ => None,
        })
    }

    #[test]
    fn parse_initd_service_declaration() {
        // Exact format used by examples/chess/etc/init.d/chess.glia
        let input = r#"{:protocol  "chess"
 :handler   "bin/chess-handler.wasm"
 :namespace "ww.chess.v1"}"#;

        let val = read(input).unwrap();
        let pairs = match &val {
            Val::Map(pairs) => pairs,
            other => panic!("expected Map, got {other:?}"),
        };

        assert_eq!(pairs.len(), 3);
        assert_eq!(map_get_str(pairs, "protocol"), Some("chess"));
        assert_eq!(
            map_get_str(pairs, "handler"),
            Some("bin/chess-handler.wasm")
        );
        assert_eq!(map_get_str(pairs, "namespace"), Some("ww.chess.v1"));
    }

    #[test]
    fn initd_missing_key_returns_none() {
        let val = read(r#"{:protocol "chess"}"#).unwrap();
        let pairs = match &val {
            Val::Map(pairs) => pairs,
            other => panic!("expected Map, got {other:?}"),
        };
        assert_eq!(map_get_str(pairs, "protocol"), Some("chess"));
        assert_eq!(map_get_str(pairs, "handler"), None);
        assert_eq!(map_get_str(pairs, "namespace"), None);
    }

    #[test]
    fn initd_wrong_value_type_returns_none() {
        // :handler with a keyword value instead of a string
        let val = read(r#"{:protocol "chess" :handler :not-a-string}"#).unwrap();
        let pairs = match &val {
            Val::Map(pairs) => pairs,
            other => panic!("expected Map, got {other:?}"),
        };
        assert_eq!(map_get_str(pairs, "protocol"), Some("chess"));
        assert_eq!(map_get_str(pairs, "handler"), None);
    }

    // --- read_many error paths (exercises init.d SysV recovery) ---

    #[test]
    fn read_many_unclosed_paren() {
        assert!(read_many("(a b").is_err());
    }

    #[test]
    fn read_many_unbalanced_bracket() {
        assert!(read_many("[1 2 3").is_err());
    }

    #[test]
    fn read_many_unexpected_close() {
        assert!(read_many(")").is_err());
    }

    #[test]
    fn read_many_malformed_mid_script() {
        // First form is valid, second is malformed — entire parse fails.
        assert!(read_many("(host id) (executor echo").is_err());
    }

    #[test]
    fn read_many_unclosed_string() {
        assert!(read_many("(host listen \"unterminated)").is_err());
    }

    #[test]
    fn read_many_valid_forms_before_error_still_fails() {
        // Verifies read_many is atomic: partial success is still Err.
        let result = read_many("(a) (b) (c");
        assert!(result.is_err());
    }

    // --- Val::Bytes ---

    #[test]
    fn display_bytes_empty() {
        assert_eq!(format!("{}", Val::Bytes(vec![])), "<0 bytes>");
    }

    #[test]
    fn display_bytes_nonempty() {
        assert_eq!(format!("{}", Val::Bytes(vec![1, 2, 3])), "<3 bytes>");
    }

    #[test]
    fn partial_eq_bytes() {
        assert_eq!(Val::Bytes(vec![1, 2]), Val::Bytes(vec![1, 2]));
        assert_ne!(Val::Bytes(vec![1, 2]), Val::Bytes(vec![1, 3]));
        assert_ne!(Val::Bytes(vec![1, 2]), Val::Nil);
    }

    // --- quote reader sugar ---

    #[test]
    fn tokenize_quote_symbol() {
        let tokens = tokenize("'foo").unwrap();
        assert_eq!(tokens, vec![Token::Quote, Token::Atom("foo".into())]);
    }

    #[test]
    fn quote_symbol() {
        let val = read("'foo").unwrap();
        assert_eq!(
            val,
            Val::List(vec![Val::Sym("quote".into()), Val::Sym("foo".into())])
        );
    }

    #[test]
    fn quote_list() {
        let val = read("'(+ 1 2)").unwrap();
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("quote".into()),
                Val::List(vec![Val::Sym("+".into()), Val::Int(1), Val::Int(2),]),
            ])
        );
    }

    #[test]
    fn quote_nested() {
        let val = read("''x").unwrap();
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("quote".into()),
                Val::List(vec![Val::Sym("quote".into()), Val::Sym("x".into()),]),
            ])
        );
    }

    #[test]
    fn quote_integer() {
        let val = read("'42").unwrap();
        assert_eq!(val, Val::List(vec![Val::Sym("quote".into()), Val::Int(42)]));
    }

    #[test]
    fn quote_vector() {
        let val = read("'[1 2 3]").unwrap();
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("quote".into()),
                Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]),
            ])
        );
    }

    #[test]
    fn quote_display_roundtrip() {
        // Quote sugar parses to (quote ...) and displays as (quote ...)
        let val = read("'foo").unwrap();
        assert_eq!(format!("{val}"), "(quote foo)");

        let val2 = read("'(+ 1 2)").unwrap();
        assert_eq!(format!("{val2}"), "(quote (+ 1 2))");
    }

    #[test]
    fn quote_eof_error() {
        assert!(read("'").is_err());
    }

    // --- Syntax-quote tokenizer tests ---

    #[test]
    fn tokenize_backtick() {
        let tokens = tokenize("`foo").unwrap();
        assert_eq!(tokens, vec![Token::Backtick, Token::Atom("foo".into())]);
    }

    #[test]
    fn tokenize_unquote() {
        let tokens = tokenize("~foo").unwrap();
        assert_eq!(tokens, vec![Token::Unquote, Token::Atom("foo".into())]);
    }

    #[test]
    fn tokenize_splice_unquote() {
        let tokens = tokenize("~@foo").unwrap();
        assert_eq!(
            tokens,
            vec![Token::SpliceUnquote, Token::Atom("foo".into())]
        );
    }

    #[test]
    fn tokenize_tilde_in_list() {
        let tokens = tokenize("(a ~b)").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Open,
                Token::Atom("a".into()),
                Token::Unquote,
                Token::Atom("b".into()),
                Token::Close,
            ]
        );
    }

    #[test]
    fn tokenize_splice_in_list() {
        let tokens = tokenize("(a ~@b)").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Open,
                Token::Atom("a".into()),
                Token::SpliceUnquote,
                Token::Atom("b".into()),
                Token::Close,
            ]
        );
    }

    #[test]
    fn tokenize_at_in_symbol() {
        // @ is NOT an atom boundary — @foo is a valid symbol
        let tokens = tokenize("@foo").unwrap();
        assert_eq!(tokens, vec![Token::Atom("@foo".into())]);
    }

    // --- Syntax-quote parser tests ---

    #[test]
    fn syntax_quote_symbol() {
        // `x → (quote x)
        let val = read("`x").unwrap();
        assert_eq!(
            val,
            Val::List(vec![Val::Sym("quote".into()), Val::Sym("x".into())])
        );
    }

    #[test]
    fn syntax_quote_self_eval_int() {
        // `42 → 42
        let val = read("`42").unwrap();
        assert_eq!(val, Val::Int(42));
    }

    #[test]
    fn syntax_quote_self_eval_nil() {
        // `nil → nil
        let val = read("`nil").unwrap();
        assert_eq!(val, Val::Nil);
    }

    #[test]
    fn syntax_quote_self_eval_keyword() {
        // `:foo → :foo
        let val = read("`:foo").unwrap();
        assert_eq!(val, Val::Keyword("foo".into()));
    }

    #[test]
    fn syntax_quote_list() {
        // `(a b) → (concat (list (quote a)) (list (quote b)))
        let val = read("`(a b)").unwrap();
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("concat".into()),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![Val::Sym("quote".into()), Val::Sym("a".into())]),
                ]),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![Val::Sym("quote".into()), Val::Sym("b".into())]),
                ]),
            ])
        );
    }

    #[test]
    fn syntax_quote_unquote() {
        // `(a ~b) → (concat (list (quote a)) (list b))
        let val = read("`(a ~b)").unwrap();
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("concat".into()),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![Val::Sym("quote".into()), Val::Sym("a".into())]),
                ]),
                Val::List(vec![Val::Sym("list".into()), Val::Sym("b".into())]),
            ])
        );
    }

    #[test]
    fn syntax_quote_splice() {
        // `(a ~@b) → (concat (list (quote a)) b)
        let val = read("`(a ~@b)").unwrap();
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("concat".into()),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![Val::Sym("quote".into()), Val::Sym("a".into())]),
                ]),
                Val::Sym("b".into()),
            ])
        );
    }

    #[test]
    fn syntax_quote_vector() {
        // `[a ~b] → (vec (concat (list (quote a)) (list b)))
        let val = read("`[a ~b]").unwrap();
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("vec".into()),
                Val::List(vec![
                    Val::Sym("concat".into()),
                    Val::List(vec![
                        Val::Sym("list".into()),
                        Val::List(vec![Val::Sym("quote".into()), Val::Sym("a".into())]),
                    ]),
                    Val::List(vec![Val::Sym("list".into()), Val::Sym("b".into())]),
                ]),
            ])
        );
    }

    #[test]
    fn syntax_quote_nested_list() {
        // `(a (b ~c)) → (concat (list (quote a)) (list (concat (list (quote b)) (list c))))
        let val = read("`(a (b ~c))").unwrap();
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("concat".into()),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![Val::Sym("quote".into()), Val::Sym("a".into())]),
                ]),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![
                        Val::Sym("concat".into()),
                        Val::List(vec![
                            Val::Sym("list".into()),
                            Val::List(vec![Val::Sym("quote".into()), Val::Sym("b".into()),]),
                        ]),
                        Val::List(vec![Val::Sym("list".into()), Val::Sym("c".into())]),
                    ]),
                ]),
            ])
        );
    }

    #[test]
    fn syntax_quote_only_unquote() {
        // `~x → x (syntax-quoting an unquote is identity)
        let val = read("`~x").unwrap();
        assert_eq!(val, Val::Sym("x".into()));
    }

    #[test]
    fn syntax_quote_empty_list() {
        // `() → (list)
        let val = read("`()").unwrap();
        assert_eq!(val, Val::List(vec![Val::Sym("list".into())]));
    }

    #[test]
    fn syntax_quote_eof_error() {
        assert!(read("`").is_err());
    }

    #[test]
    fn syntax_quote_splice_top_level_error() {
        // `~@x at top level → error
        assert!(read("`~@x").is_err());
    }

    #[test]
    fn syntax_quote_preserves_inner_quote() {
        // `'(unquote x) should produce (quote (unquote x)) as a literal,
        // NOT treat the inner (unquote x) as a real unquote.
        // The reader parses '(unquote x) as (quote (unquote x)).
        // Inside syntax-quote, (quote ...) should be preserved as-is.
        let val = read("`'(unquote x)").unwrap();
        // Should produce: (concat (list (quote quote)) (list (quote (unquote x))))
        assert_eq!(
            val,
            Val::List(vec![
                Val::Sym("concat".into()),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![Val::Sym("quote".into()), Val::Sym("quote".into()),]),
                ]),
                Val::List(vec![
                    Val::Sym("list".into()),
                    Val::List(vec![
                        Val::Sym("quote".into()),
                        Val::List(vec![Val::Sym("unquote".into()), Val::Sym("x".into()),]),
                    ]),
                ]),
            ])
        );
    }

    #[test]
    fn unquote_eof_error() {
        assert!(read("~").is_err());
    }

    #[test]
    fn splice_unquote_eof_error() {
        assert!(read("~@").is_err());
    }

    // --- Nested syntax-quote depth tracking tests (#234) ---

    #[test]
    fn nested_syntax_quote_preserves_inner() {
        // `(a `(b ~c)) — the inner ~c should NOT be resolved by the outer backtick.
        // The inner backtick produces a (syntax-quote ...) marker form, which
        // increments depth. The ~c inside it is at depth 1, so it's preserved.
        let val = read("`(a `(b ~c))").unwrap();
        let display = format!("{val}");
        // The inner ~c must NOT be resolved — it should appear as (unquote c) in output
        assert!(
            display.contains("unquote"),
            "inner ~c should be preserved as literal unquote, got: {display}"
        );
        assert!(
            display.contains("syntax-quote"),
            "inner backtick should be preserved as syntax-quote, got: {display}"
        );
    }

    #[test]
    fn nested_syntax_quote_outer_unquote() {
        // `(a ~b `(c ~d)) — ~b resolves (outer, depth 0), ~d does not (inner, depth 1)
        let val = read("`(a ~b `(c ~d))").unwrap();
        let display = format!("{val}");
        // The inner backtick should be preserved as syntax-quote
        assert!(
            display.contains("syntax-quote"),
            "inner backtick should be preserved, got: {display}"
        );
    }

    #[test]
    fn double_unquote_no_panic() {
        // `(a `(b ~~c)) — nested double unquote should not panic.
        let result = read("`(a `(b ~~c))");
        assert!(
            result.is_ok(),
            "double unquote should not panic: {result:?}"
        );
    }

    #[test]
    fn quote_inside_syntax_quote() {
        // `(a '(unquote b)) — the quoted unquote should be preserved literally.
        let val = read("`(a '(unquote b))").unwrap();
        let display = format!("{val}");
        assert!(
            display.contains("quote"),
            "quoted form should be preserved, got: {display}"
        );
    }

    #[test]
    fn syntax_quote_depth_overflow() {
        // Very deeply nested backticks shouldn't panic (test with 3-4 levels)
        let result = read("`(a `(b `(c ~d)))");
        assert!(
            result.is_ok(),
            "3-level nested syntax-quote should not panic: {result:?}"
        );

        let result = read("`(a `(b `(c `(d ~e))))");
        assert!(
            result.is_ok(),
            "4-level nested syntax-quote should not panic: {result:?}"
        );
    }

    #[test]
    fn nested_splice_unquote_preserved() {
        // `(a `(b ~@c)) — ~@c at depth 1 should be preserved as literal splice-unquote
        let val = read("`(a `(b ~@c))").unwrap();
        let display = format!("{val}");
        assert!(
            display.contains("splice-unquote"),
            "inner ~@c should be preserved as literal splice-unquote, got: {display}"
        );
    }

    #[test]
    fn nested_syntax_quote_common_macro_pattern() {
        // `(let [x 1] `(+ ~x 2)) — common macro pattern, must work
        let result = read("`(let [x 1] `(+ ~x 2))");
        assert!(
            result.is_ok(),
            "common macro pattern should not panic: {result:?}"
        );
        let val = result.unwrap();
        let display = format!("{val}");
        assert!(
            display.contains("syntax-quote"),
            "inner backtick should be preserved, got: {display}"
        );
    }

    #[test]
    fn nested_syntax_quote_symbol_preserved() {
        // `(a `b) — inner backtick on a symbol should preserve as syntax-quote form
        let val = read("`(a `b)").unwrap();
        let display = format!("{val}");
        assert!(
            display.contains("syntax-quote"),
            "inner backtick on symbol should be preserved, got: {display}"
        );
    }

    // --- syntax-quote map tests ---

    #[test]
    fn syntax_quote_empty_map() {
        let result = read("`{}");
        assert!(
            result.is_ok(),
            "syntax-quote of empty map should parse without error: {result:?}"
        );
    }

    #[test]
    fn syntax_quote_map_literal() {
        // `{:a 1} should produce an assoc-based form
        let val = read("`{:a 1}").unwrap();
        let display = format!("{val}");
        assert!(
            display.contains("assoc"),
            "syntax-quoted map should produce assoc form, got: {display}"
        );
    }

    #[test]
    fn syntax_quote_map_with_unquote() {
        // `{:a ~x} should parse and produce an assoc form with unquote expansion
        let val = read("`{:a ~x}").unwrap();
        let display = format!("{val}");
        assert!(
            display.contains("assoc"),
            "syntax-quoted map with unquote should produce assoc form, got: {display}"
        );
        // The unquoted symbol x should appear in the output (not wrapped in unquote)
        assert!(
            display.contains('x'),
            "unquoted symbol should appear in expansion, got: {display}"
        );
    }
}
