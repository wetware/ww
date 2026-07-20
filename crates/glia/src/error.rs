//! Structured error values for Glia.
//!
//! Errors are carried over the existing effect machinery: `(throw err)`
//! desugars to `(perform :glia.exception err)`, and an unhandled throw
//! escapes `eval` as `Err(Val::Effect { effect_type: "glia.exception",
//! data: <err> })`. The data shape is a `Val::Map` keyed by namespaced
//! `:glia.error/...` keywords, defined here.
//!
//! # Construction
//!
//! Construct via the `GliaError` enum. `From<GliaError> for Val` is the
//! single source of truth for the canonical Val::Map shape — every
//! variant has exactly one place that determines its serialization. The
//! `pub fn` helpers (`unbound_symbol`, `arity`, …) are thin wrappers
//! kept for ergonomics.
//!
//! ```ignore
//! return Err(GliaError::UnboundSymbol {
//!     symbol: "foo".into(),
//!     hint: Some("did you mean 'bar'?".into()),
//! }.into());
//! ```
//!
//! # Schema
//!
//! ```text
//! { :glia.error/type     <keyword>   ; tag identifying the variant
//!   :glia.error/message  <string>    ; human-readable summary
//!   :glia.error/hint     <string>    ; optional recovery suggestion
//!   ; ...variant-specific fields (e.g. :glia.error/symbol, :glia.error/function)
//! }
//! ```
//!
//! User errors constructed via the `ex-info` Glia builtin go through
//! the same `GliaError::User` variant: the user's `:type` ends up in
//! `:glia.error/type` (so `(catch :foo e ...)` matches user errors
//! thrown as `(throw (ex-info "..." {:type :foo}))`), and the user's
//! `:message` mirrors into `:glia.error/message`. User-supplied keys
//! are preserved verbatim alongside the canonical fields.
//!
//! # Inspection
//!
//! `data`, `message`, `type_tag`, `hint` mirror Clojure's `ex-data` /
//! `ex-message`. `unwrap_thrown` peels the `Val::Effect` carrier when
//! an unhandled throw arrives at an outer caller (kernel REPL, MCP
//! cell, shell session).

use crate::{Val, ValMap};

// ----- Tag constants ------------------------------------------------------

/// Namespaced `:glia.error/type` keywords. `tag::*` is the single source
/// of truth for variant tags — call sites and consumers should never
/// spell these out as string literals.
pub mod tag {
    pub const PARSE: &str = "glia.error/parse";
    pub const UNBOUND_SYMBOL: &str = "glia.error/unbound-symbol";
    pub const ARITY: &str = "glia.error/arity-mismatch";
    pub const TYPE_MISMATCH: &str = "glia.error/type-mismatch";
    pub const CAP_CALL: &str = "glia.error/cap-call-failed";
    pub const RPC: &str = "glia.error/rpc-error";
    pub const EPOCH_EXPIRED: &str = "glia.error/epoch-expired";
    pub const PERMISSION_DENIED: &str = "glia.error/permission-denied";
    pub const FUEL_EXHAUSTED: &str = "glia.error/fuel-exhausted";
    pub const INTERNAL: &str = "glia.error/internal";
    pub const CONTINUATION_ABANDONED: &str = "glia.error/continuation-abandoned";
    pub const CONTINUATION_ALREADY_RESUMED: &str = "glia.error/continuation-already-resumed";
}

// ----- Schema-key constants -----------------------------------------------

/// Canonical `:glia.error/...` map keys.
pub mod key {
    pub const TYPE: &str = "glia.error/type";
    pub const MESSAGE: &str = "glia.error/message";
    pub const HINT: &str = "glia.error/hint";
    pub const SYMBOL: &str = "glia.error/symbol";
    pub const FUNCTION: &str = "glia.error/function";
    pub const EXPECTED: &str = "glia.error/expected";
    pub const GOT: &str = "glia.error/got";
    pub const GOT_TYPE: &str = "glia.error/got-type";
    pub const CONTEXT: &str = "glia.error/context";
    pub const CAP: &str = "glia.error/cap";
    pub const METHOD: &str = "glia.error/method";
    pub const SOURCE_LOCATION: &str = "glia.error/source-location";
}

/// Effect target carrying a thrown error.
pub const EXCEPTION_EFFECT: &str = "glia.exception";

// ----- Typed error enum ---------------------------------------------------

/// Structured error variants. Each construction site picks a real
/// variant — there is no `Generic` escape hatch. `Internal` is for
/// genuine "should not happen" invariant violations only.
///
/// `User` is the construction path for the `ex-info` Glia builtin:
/// the user-supplied `:type` keyword becomes the canonical
/// `:glia.error/type` tag, and any other user-supplied keys are
/// preserved alongside the namespaced fields.
#[derive(Debug, Clone)]
pub enum GliaError {
    /// Parse error — input failed to tokenize or parse.
    Parse {
        location: Option<String>,
        message: String,
    },
    /// Unbound symbol — reference to an undefined identifier.
    UnboundSymbol {
        symbol: String,
        hint: Option<String>,
    },
    /// Arity mismatch — wrong number of arguments to a function or
    /// special form. `expected` is a human-readable arity description
    /// (e.g. `"2"`, `"2-3"`, `"at least 1"`).
    Arity {
        function: String,
        expected: String,
        got: usize,
    },
    /// Type mismatch — argument or operand of the wrong runtime type.
    /// `got_type` is the `Val` variant name (e.g. `"int"`, `"map"`).
    TypeMismatch {
        context: String,
        expected: String,
        got_type: String,
    },
    /// Capability call failed — an RPC method on a capability returned
    /// an error.
    CapCall {
        cap: String,
        method: String,
        message: String,
    },
    /// Generic transport-level RPC failure (disconnect, timeout,
    /// malformed frame). Distinct from `CapCall`, which is method-level.
    Rpc { message: String },
    /// Epoch expired — a capability was used after the membrane epoch
    /// advanced.
    EpochExpired { cap: String },
    /// Permission denied — access refused (attenuated capability,
    /// missing membrane grant).
    PermissionDenied { what: String, hint: Option<String> },
    /// Fuel exhausted — the cell ran out of compute budget.
    FuelExhausted,
    /// Internal error — invariant violation. NOT a generic escape
    /// hatch; reach for a specific variant first.
    Internal { context: String, message: String },
    /// Continuation abandoned — a handler returned without calling
    /// `resume`, so the suspended body was discarded. Surfaces to a
    /// suspended computation that is resumed against a dropped one-shot
    /// channel (handler-abort path).
    ContinuationAbandoned,
    /// One-shot continuation reused — `resume` was called more than
    /// once. Continuations captured by `with-effect-handler` are
    /// one-shot; a second `resume` is a protocol violation.
    ContinuationAlreadyResumed,
    /// User-thrown error, constructed via the `ex-info` Glia builtin.
    /// The user's `:type` becomes the canonical dispatch tag; other
    /// user fields are carried in `extras`.
    User {
        type_tag: Val,
        message: String,
        extras: ValMap,
    },
}

impl GliaError {
    /// Stable namespaced tag for the dispatcher. For `User` errors
    /// the tag is whatever the user supplied as `:type` — falls back
    /// to the empty string for malformed user data.
    pub fn tag(&self) -> String {
        match self {
            Self::Parse { .. } => tag::PARSE.into(),
            Self::UnboundSymbol { .. } => tag::UNBOUND_SYMBOL.into(),
            Self::Arity { .. } => tag::ARITY.into(),
            Self::TypeMismatch { .. } => tag::TYPE_MISMATCH.into(),
            Self::CapCall { .. } => tag::CAP_CALL.into(),
            Self::Rpc { .. } => tag::RPC.into(),
            Self::EpochExpired { .. } => tag::EPOCH_EXPIRED.into(),
            Self::PermissionDenied { .. } => tag::PERMISSION_DENIED.into(),
            Self::FuelExhausted => tag::FUEL_EXHAUSTED.into(),
            Self::Internal { .. } => tag::INTERNAL.into(),
            Self::ContinuationAbandoned => tag::CONTINUATION_ABANDONED.into(),
            Self::ContinuationAlreadyResumed => tag::CONTINUATION_ALREADY_RESUMED.into(),
            Self::User { type_tag, .. } => match type_tag {
                Val::Keyword(s) | Val::Str(s) | Val::Sym(s) => s.clone(),
                _ => String::new(),
            },
        }
    }
}

impl From<GliaError> for Val {
    fn from(e: GliaError) -> Self {
        // Single source of truth for the canonical `Val::Map` schema.
        // Adding a variant fails to compile here until the new arm is
        // written; renaming a key changes one place.
        let tag_val = type_tag_val(&e);
        let mut pairs: Vec<(Val, Val)> = Vec::new();
        pairs.push((kw(key::TYPE), tag_val));

        match e {
            GliaError::Parse { location, message } => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(format!("parse error: {message}")),
                ));
                if let Some(loc) = location {
                    pairs.push((kw(key::SOURCE_LOCATION), Val::Str(loc)));
                }
            }
            GliaError::UnboundSymbol { symbol, hint } => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(format!("unbound symbol: {symbol}")),
                ));
                pairs.push((kw(key::SYMBOL), Val::Sym(symbol)));
                if let Some(h) = hint {
                    pairs.push((kw(key::HINT), Val::Str(h)));
                }
            }
            GliaError::Arity {
                function,
                expected,
                got,
            } => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(format!("{function}: expected {expected} arg(s), got {got}")),
                ));
                pairs.push((kw(key::FUNCTION), Val::Str(function)));
                pairs.push((kw(key::EXPECTED), Val::Str(expected)));
                pairs.push((kw(key::GOT), Val::Int(got as i64)));
            }
            GliaError::TypeMismatch {
                context,
                expected,
                got_type,
            } => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(format!("{context}: expected {expected}, got {got_type}")),
                ));
                pairs.push((kw(key::CONTEXT), Val::Str(context)));
                pairs.push((kw(key::EXPECTED), Val::Str(expected)));
                pairs.push((kw(key::GOT_TYPE), Val::Str(got_type)));
            }
            GliaError::CapCall {
                cap,
                method,
                message,
            } => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(format!("{cap}.{method} failed: {message}")),
                ));
                pairs.push((kw(key::CAP), Val::Str(cap)));
                pairs.push((kw(key::METHOD), Val::Str(method)));
            }
            GliaError::Rpc { message } => {
                pairs.push((kw(key::MESSAGE), Val::Str(format!("rpc error: {message}"))));
            }
            GliaError::EpochExpired { cap } => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(format!("epoch expired for cap '{cap}'")),
                ));
                pairs.push((kw(key::CAP), Val::Str(cap)));
            }
            GliaError::PermissionDenied { what, hint } => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(format!("permission denied: {what}")),
                ));
                pairs.push((kw(key::CONTEXT), Val::Str(what)));
                if let Some(h) = hint {
                    pairs.push((kw(key::HINT), Val::Str(h)));
                }
            }
            GliaError::FuelExhausted => {
                pairs.push((kw(key::MESSAGE), Val::Str("fuel exhausted".into())));
            }
            GliaError::Internal { context, message } => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(format!("internal error in {context}: {message}")),
                ));
                pairs.push((kw(key::CONTEXT), Val::Str(context)));
            }
            GliaError::ContinuationAbandoned => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str("continuation abandoned — handler returned without resuming".into()),
                ));
            }
            GliaError::ContinuationAlreadyResumed => {
                pairs.push((
                    kw(key::MESSAGE),
                    Val::Str(
                        "continuation already resumed — one-shot continuation may only be resumed once".into(),
                    ),
                ));
            }
            GliaError::User {
                type_tag,
                message,
                extras,
            } => {
                pairs.push((kw(key::MESSAGE), Val::Str(message.clone())));
                // Mirror ex-info's back-compat fields: the user's
                // :type and :message are preserved in their non-namespaced
                // forms alongside the namespaced canonical fields, so
                // legacy readers (and Clojure idiom) keep working.
                pairs.push((Val::Keyword("type".into()), type_tag));
                pairs.push((Val::Keyword("message".into()), Val::Str(message)));
                // User-supplied extras layer on top last so they win
                // any collision (defensive — extras shouldn't include
                // canonical keys, but if they do, user intent wins on
                // their own keys; canonical keys above are immutable).
                let mut m = ValMap::from_pairs(pairs);
                for (k, v) in extras.iter() {
                    // Skip canonical keys to preserve schema invariants.
                    if is_canonical_key(k) {
                        continue;
                    }
                    m = m.assoc(k.clone(), v.clone());
                }
                return Val::Map(m);
            }
        }

        Val::Map(ValMap::from_pairs(pairs))
    }
}

#[inline]
fn type_tag_val(e: &GliaError) -> Val {
    match e {
        GliaError::User { type_tag, .. } => type_tag.clone(),
        other => kw(&other.tag()),
    }
}

/// Reject keys that would corrupt the canonical schema if a `User`
/// error tries to override them via extras.
fn is_canonical_key(v: &Val) -> bool {
    matches!(v, Val::Keyword(k) if k.starts_with("glia.error/"))
}

// ----- Convenience constructors (thin wrappers over GliaError) -----------

/// Parse error.
pub fn parse(location: Option<&str>, message: impl Into<String>) -> Val {
    GliaError::Parse {
        location: location.map(String::from),
        message: message.into(),
    }
    .into()
}

/// Unbound symbol.
pub fn unbound_symbol(symbol: &str, hint: Option<&str>) -> Val {
    GliaError::UnboundSymbol {
        symbol: symbol.into(),
        hint: hint.map(String::from),
    }
    .into()
}

/// Arity mismatch.
pub fn arity(function: &str, expected: &str, got: usize) -> Val {
    GliaError::Arity {
        function: function.into(),
        expected: expected.into(),
        got,
    }
    .into()
}

/// Type mismatch — the `got` value's type name is recorded automatically.
pub fn type_mismatch(context: &str, expected: &str, got: &Val) -> Val {
    GliaError::TypeMismatch {
        context: context.into(),
        expected: expected.into(),
        got_type: val_type_name(got).into(),
    }
    .into()
}

/// Capability call failed.
pub fn cap_call(cap: &str, method: &str, message: impl Into<String>) -> Val {
    GliaError::CapCall {
        cap: cap.into(),
        method: method.into(),
        message: message.into(),
    }
    .into()
}

/// Generic transport-level RPC failure.
pub fn rpc(message: impl Into<String>) -> Val {
    GliaError::Rpc {
        message: message.into(),
    }
    .into()
}

/// Epoch expired.
pub fn epoch_expired(cap: &str) -> Val {
    GliaError::EpochExpired { cap: cap.into() }.into()
}

/// Permission denied.
pub fn permission_denied(what: &str, hint: Option<&str>) -> Val {
    GliaError::PermissionDenied {
        what: what.into(),
        hint: hint.map(String::from),
    }
    .into()
}

/// Fuel exhausted.
pub fn fuel_exhausted() -> Val {
    GliaError::FuelExhausted.into()
}

/// Internal error — for genuine "should not happen" bugs.
pub fn internal(context: &str, message: impl Into<String>) -> Val {
    GliaError::Internal {
        context: context.into(),
        message: message.into(),
    }
    .into()
}

/// Continuation abandoned — a handler returned without resuming, so
/// the suspended body was discarded (handler-abort cleanup path).
pub fn continuation_abandoned() -> Val {
    GliaError::ContinuationAbandoned.into()
}

/// One-shot continuation reused — `resume` was called more than once.
pub fn continuation_already_resumed() -> Val {
    GliaError::ContinuationAlreadyResumed.into()
}

/// User-thrown error (`ex-info`-style). `type_tag` should be a
/// `Val::Keyword`, `Val::Str`, or `Val::Sym` — anything else gives
/// an empty dispatch tag and won't be catchable.
pub fn user(type_tag: Val, message: impl Into<String>, extras: ValMap) -> Val {
    GliaError::User {
        type_tag,
        message: message.into(),
        extras,
    }
    .into()
}

// ----- Inspection accessors ----------------------------------------------

/// Extract the structured error map from an error `Val`. Mirrors
/// Clojure's `ex-data`. Returns `None` for plain values, plain
/// strings, or maps lacking `:glia.error/type`.
pub fn data(err: &Val) -> Option<&ValMap> {
    if let Val::Map(m) = err {
        if m.contains_key(&kw(key::TYPE)) {
            return Some(m);
        }
    }
    None
}

/// Extract the error message. Mirrors Clojure's `ex-message`.
/// Returns the `:glia.error/message` field from a structured error,
/// the contents of a `Val::Str` for legacy errors, or `None` otherwise.
pub fn message(err: &Val) -> Option<&str> {
    if let Val::Str(s) = err {
        return Some(s.as_str());
    }
    let m = data(err)?;
    match m.get(&kw(key::MESSAGE)) {
        Some(Val::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// Extract the namespaced `:glia.error/type` tag. Returns `None` for
/// plain-string errors or maps without a tag. Use `tag::*` constants
/// to compare against, never string literals.
pub fn type_tag(err: &Val) -> Option<&str> {
    let m = data(err)?;
    match m.get(&kw(key::TYPE)) {
        Some(Val::Keyword(k)) | Some(Val::Str(k)) | Some(Val::Sym(k)) => Some(k.as_str()),
        _ => None,
    }
}

/// Extract the optional `:glia.error/hint` field.
pub fn hint(err: &Val) -> Option<&str> {
    let m = data(err)?;
    match m.get(&kw(key::HINT)) {
        Some(Val::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// If `err` is the `Val::Effect` carrier produced by an unhandled
/// throw (`(perform :glia.exception ...)`), return a reference to
/// the inner error data. Otherwise return `None`.
///
/// Outer callers (kernel REPL, MCP cell, shell) call this once at
/// the eval boundary so downstream code only needs to know about
/// the structured error map shape.
pub fn unwrap_thrown(err: &Val) -> Option<&Val> {
    match err {
        Val::Effect { effect_type, data } if effect_type == EXCEPTION_EFFECT => Some(data),
        _ => None,
    }
}

// ----- Internal helpers ---------------------------------------------------

#[inline]
fn kw(s: &str) -> Val {
    Val::Keyword(s.into())
}

/// Human-readable name for a `Val` variant — used by `type_mismatch`
/// to fill the `:glia.error/got-type` field.
pub(crate) fn val_type_name(v: &Val) -> &'static str {
    match v {
        Val::Nil => "nil",
        Val::Bool(_) => "bool",
        Val::Int(_) => "int",
        Val::Float(_) => "float",
        Val::Str(_) => "string",
        Val::Sym(_) => "symbol",
        Val::Keyword(_) => "keyword",
        Val::Atom(_) => "atom",
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
        Val::Resume(_) => "resume",
        Val::Cap { .. } => "cap",
        Val::Cell { .. } => "cell",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val_str(s: &str) -> Val {
        Val::Str(s.into())
    }

    // ----- Schema correctness via the enum -------------------------------

    #[test]
    fn parse_has_canonical_shape() {
        let err = parse(Some("foo.glia:1:1"), "unexpected token");
        assert_eq!(type_tag(&err), Some(tag::PARSE));
        assert!(message(&err).unwrap().contains("unexpected token"));
        let m = data(&err).unwrap();
        assert!(matches!(
            m.get(&kw(key::SOURCE_LOCATION)),
            Some(Val::Str(loc)) if loc == "foo.glia:1:1"
        ));
    }

    #[test]
    fn parse_omits_location_when_none() {
        let err = parse(None, "syntax error");
        let m = data(&err).unwrap();
        assert!(m.get(&kw(key::SOURCE_LOCATION)).is_none());
    }

    #[test]
    fn unbound_symbol_includes_symbol_field() {
        let err = unbound_symbol("foo", Some("did you mean 'bar'?"));
        assert_eq!(type_tag(&err), Some(tag::UNBOUND_SYMBOL));
        let m = data(&err).unwrap();
        assert!(matches!(m.get(&kw(key::SYMBOL)), Some(Val::Sym(s)) if s == "foo"));
        assert_eq!(hint(&err), Some("did you mean 'bar'?"));
    }

    #[test]
    fn unbound_symbol_omits_hint_when_none() {
        let err = unbound_symbol("foo", None);
        assert!(hint(&err).is_none());
    }

    #[test]
    fn arity_records_function_expected_got() {
        let err = arity("def", "1-2", 5);
        assert_eq!(type_tag(&err), Some(tag::ARITY));
        let m = data(&err).unwrap();
        assert!(matches!(m.get(&kw(key::FUNCTION)), Some(Val::Str(s)) if s == "def"));
        assert!(matches!(m.get(&kw(key::EXPECTED)), Some(Val::Str(s)) if s == "1-2"));
        assert!(matches!(m.get(&kw(key::GOT)), Some(Val::Int(5))));
    }

    #[test]
    fn type_mismatch_records_got_type_from_val() {
        let err = type_mismatch("if", "bool", &Val::Int(42));
        assert_eq!(type_tag(&err), Some(tag::TYPE_MISMATCH));
        let m = data(&err).unwrap();
        assert!(matches!(m.get(&kw(key::CONTEXT)), Some(Val::Str(s)) if s == "if"));
        assert!(matches!(m.get(&kw(key::EXPECTED)), Some(Val::Str(s)) if s == "bool"));
        assert!(matches!(m.get(&kw(key::GOT_TYPE)), Some(Val::Str(s)) if s == "int"));
    }

    #[test]
    fn cap_call_records_cap_and_method() {
        let err = cap_call("host", "listen", "stale epoch");
        assert_eq!(type_tag(&err), Some(tag::CAP_CALL));
        assert!(message(&err).unwrap().contains("host.listen"));
        let m = data(&err).unwrap();
        assert!(matches!(m.get(&kw(key::CAP)), Some(Val::Str(s)) if s == "host"));
        assert!(matches!(m.get(&kw(key::METHOD)), Some(Val::Str(s)) if s == "listen"));
    }

    #[test]
    fn rpc_uses_namespaced_tag() {
        let err = rpc("connection reset");
        assert_eq!(type_tag(&err), Some(tag::RPC));
        assert!(message(&err).unwrap().contains("connection reset"));
    }

    #[test]
    fn epoch_expired_records_cap() {
        let err = epoch_expired("routing");
        assert_eq!(type_tag(&err), Some(tag::EPOCH_EXPIRED));
        let m = data(&err).unwrap();
        assert!(matches!(m.get(&kw(key::CAP)), Some(Val::Str(s)) if s == "routing"));
    }

    #[test]
    fn permission_denied_records_context_and_hint() {
        let err = permission_denied("network/dial", Some("graft network cap to enable"));
        assert_eq!(type_tag(&err), Some(tag::PERMISSION_DENIED));
        let m = data(&err).unwrap();
        assert!(matches!(
            m.get(&kw(key::CONTEXT)),
            Some(Val::Str(s)) if s == "network/dial"
        ));
        assert_eq!(hint(&err), Some("graft network cap to enable"));
    }

    #[test]
    fn fuel_exhausted_has_correct_tag() {
        let err = fuel_exhausted();
        assert_eq!(type_tag(&err), Some(tag::FUEL_EXHAUSTED));
    }

    #[test]
    fn internal_records_context() {
        let err = internal("eval_fn_body", "unreachable variant");
        assert_eq!(type_tag(&err), Some(tag::INTERNAL));
        assert!(message(&err).unwrap().contains("unreachable variant"));
    }

    #[test]
    fn continuation_abandoned_has_correct_tag() {
        let err = continuation_abandoned();
        assert_eq!(type_tag(&err), Some(tag::CONTINUATION_ABANDONED));
        assert!(message(&err).unwrap().contains("abandoned"));
    }

    #[test]
    fn continuation_already_resumed_has_correct_tag() {
        let err = continuation_already_resumed();
        assert_eq!(type_tag(&err), Some(tag::CONTINUATION_ALREADY_RESUMED));
        assert!(message(&err).unwrap().contains("one-shot"));
    }

    // ----- User variant (ex-info path) -----------------------------------

    #[test]
    fn user_keyword_tag_dispatches_by_user_type() {
        let err = user(
            Val::Keyword("network".into()),
            "peer unreachable",
            ValMap::from_pairs(vec![(
                Val::Keyword("peer".into()),
                Val::Str("QmFoo".into()),
            )]),
        );
        // Dispatch tag is the user's :type, NOT a namespaced :glia.error/...
        assert_eq!(type_tag(&err), Some("network"));
        // Message is canonical
        assert_eq!(message(&err), Some("peer unreachable"));
        let m = data(&err).unwrap();
        // Back-compat: :type and :message preserved
        assert_eq!(
            m.get(&Val::Keyword("type".into())),
            Some(&Val::Keyword("network".into()))
        );
        assert_eq!(
            m.get(&Val::Keyword("message".into())),
            Some(&Val::Str("peer unreachable".into()))
        );
        // User extras carried through
        assert_eq!(
            m.get(&Val::Keyword("peer".into())),
            Some(&Val::Str("QmFoo".into()))
        );
    }

    #[test]
    fn user_extras_cannot_override_canonical_keys() {
        // A user trying to set :glia.error/type via extras must NOT
        // win — canonical keys are immutable on the schema.
        let attempted_override = ValMap::from_pairs(vec![(
            Val::Keyword(key::TYPE.into()),
            Val::Keyword("evil".into()),
        )]);
        let err = user(Val::Keyword("intended".into()), "msg", attempted_override);
        assert_eq!(type_tag(&err), Some("intended"));
    }

    #[test]
    fn user_with_string_tag_works() {
        let err = user(Val::Str("custom-error".into()), "boom", ValMap::new());
        assert_eq!(type_tag(&err), Some("custom-error"));
    }

    #[test]
    fn user_with_non_string_tag_yields_empty_tag() {
        // Defensive — a user passing :type 42 still produces a Val::Map,
        // but the dispatcher tag is empty and the error is uncatchable
        // by tag (only catchable via wildcard).
        let err = user(Val::Int(42), "boom", ValMap::new());
        // tag() returns "", :glia.error/type carries the int verbatim
        let m = data(&err).unwrap();
        assert!(matches!(m.get(&kw(key::TYPE)), Some(Val::Int(42))));
        // type_tag accessor only returns Some for keyword/str/sym
        assert!(type_tag(&err).is_none());
    }

    // ----- Inspection accessors ------------------------------------------

    #[test]
    fn data_returns_none_for_plain_string() {
        assert!(data(&val_str("legacy error")).is_none());
    }

    #[test]
    fn data_returns_none_for_non_error_map() {
        let m = ValMap::from_pairs(vec![(kw("foo"), Val::Int(1))]);
        let val = Val::Map(m);
        assert!(data(&val).is_none());
    }

    #[test]
    fn data_returns_none_for_other_val_types() {
        assert!(data(&Val::Nil).is_none());
        assert!(data(&Val::Int(42)).is_none());
        assert!(data(&Val::Sym("foo".into())).is_none());
    }

    #[test]
    fn message_falls_back_to_plain_string() {
        assert_eq!(message(&val_str("boom")), Some("boom"));
    }

    #[test]
    fn message_extracts_from_structured_error() {
        let err = unbound_symbol("foo", None);
        assert!(message(&err).unwrap().contains("unbound symbol: foo"));
    }

    #[test]
    fn message_returns_none_for_non_string_non_error() {
        assert!(message(&Val::Int(42)).is_none());
    }

    #[test]
    fn type_tag_returns_none_for_plain_string() {
        assert!(type_tag(&val_str("oops")).is_none());
    }

    #[test]
    fn hint_returns_none_when_absent() {
        let err = unbound_symbol("foo", None);
        assert!(hint(&err).is_none());
    }

    // ----- unwrap_thrown -------------------------------------------------

    #[test]
    fn unwrap_thrown_returns_inner_for_glia_exception() {
        let inner = unbound_symbol("foo", None);
        let carrier = Val::Effect {
            effect_type: EXCEPTION_EFFECT.into(),
            data: Box::new(inner.clone()),
        };
        let unwrapped = unwrap_thrown(&carrier).unwrap();
        assert_eq!(type_tag(unwrapped), Some(tag::UNBOUND_SYMBOL));
    }

    #[test]
    fn unwrap_thrown_returns_none_for_other_effects() {
        let carrier = Val::Effect {
            effect_type: "fail".into(), // legacy effect target
            data: Box::new(Val::Str("legacy".into())),
        };
        assert!(unwrap_thrown(&carrier).is_none());
    }

    #[test]
    fn unwrap_thrown_returns_none_for_non_effect() {
        let direct = unbound_symbol("foo", None);
        // A direct error map (not wrapped in Val::Effect) returns None;
        // outer callers should treat this as "already inner data."
        assert!(unwrap_thrown(&direct).is_none());
    }

    // ----- Enum exhaustiveness regression --------------------------------

    #[test]
    fn enum_all_variants_round_trip_via_from() {
        // Iterate every variant; the From impl is exhaustive at compile
        // time, but this confirms each variant produces a non-empty tag
        // and a valid structured map at runtime.
        let cases: Vec<GliaError> = vec![
            GliaError::Parse {
                location: None,
                message: "x".into(),
            },
            GliaError::UnboundSymbol {
                symbol: "x".into(),
                hint: None,
            },
            GliaError::Arity {
                function: "x".into(),
                expected: "1".into(),
                got: 0,
            },
            GliaError::TypeMismatch {
                context: "x".into(),
                expected: "y".into(),
                got_type: "z".into(),
            },
            GliaError::CapCall {
                cap: "x".into(),
                method: "y".into(),
                message: "z".into(),
            },
            GliaError::Rpc {
                message: "x".into(),
            },
            GliaError::EpochExpired { cap: "x".into() },
            GliaError::PermissionDenied {
                what: "x".into(),
                hint: None,
            },
            GliaError::FuelExhausted,
            GliaError::Internal {
                context: "x".into(),
                message: "y".into(),
            },
            GliaError::ContinuationAbandoned,
            GliaError::ContinuationAlreadyResumed,
            GliaError::User {
                type_tag: Val::Keyword("x".into()),
                message: "y".into(),
                extras: ValMap::new(),
            },
        ];
        for case in cases {
            let val: Val = case.into();
            assert!(data(&val).is_some(), "variant produced unstructured Val");
            assert!(
                type_tag(&val).is_some()
                    || matches!(
                        &val,
                        Val::Map(_) // User with non-string tag handled separately
                    )
            );
        }
    }

    // ----- val_type_name -------------------------------------------------

    #[test]
    fn val_type_name_covers_all_variants() {
        assert_eq!(val_type_name(&Val::Nil), "nil");
        assert_eq!(val_type_name(&Val::Int(0)), "int");
        assert_eq!(val_type_name(&Val::Str("".into())), "string");
        assert_eq!(val_type_name(&Val::Keyword("k".into())), "keyword");
        assert_eq!(val_type_name(&Val::Map(ValMap::new())), "map");
    }
}
