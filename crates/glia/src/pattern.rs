//! Pattern matching engine — shared by `match`, `let`, `fn`, and `loop`.
//!
//! ```text
//!                      ┌─────────────┐
//!                      │ pattern.rs  │
//!                      │             │
//!                      │ Pattern enum│
//!                      │ match_pat() │
//!                      │ analyze_pat │
//!                      └──────┬──────┘
//!                             │
//!              ┌──────────────┼──────────────┐
//!              │              │              │
//!        ┌─────┴─────┐ ┌─────┴─────┐ ┌─────┴─────┐
//!        │  match     │ │  let      │ │  fn/loop  │
//!        │ (expr.rs)  │ │ (eval.rs) │ │ (eval.rs) │
//!        └────────────┘ └───────────┘ └───────────┘
//! ```

use crate::Val;
use std::collections::BTreeSet;

#[cfg(test)]
use crate::ValMap;

// =========================================================================
// Types
// =========================================================================

/// A compiled pattern — the IR shared by match, let, fn, and loop.
///
/// Patterns are finite trees. Linear matching (first match wins) is used
/// everywhere. No decision tree optimization in v1 — see TODOS.md for the
/// Maranget column-selection path if profiling demands it.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// `_` — matches anything, no binding.
    Wildcard,
    /// Symbol name — matches anything, binds the value.
    Bind(String),
    /// Literal — matches exactly equal values (nil, bool, int, float, str, keyword).
    Literal(Val),
    /// `[a b c]` — matches a vector/list with exact element count.
    /// `[a b & rest]` — matches 2+ elements, rest collects remainder.
    ///
    /// Exact-length: `[a b c]` matches only 3-element collections.
    /// With rest: `[a b & rest]` matches 2+ elements; rest may be empty list.
    Vector {
        elements: Vec<Pattern>,
        rest: Option<Box<Pattern>>,
    },
    /// `{:key pattern}` — matches a map by key presence, binds values.
    ///
    /// Partial/open matching: `{:name name}` matches `{:name "Alice" :age 30}`.
    /// Extra keys are ignored (asymmetric with Vector's exact-length matching
    /// because maps are unordered key-value stores, not positional).
    Map(Vec<(Val, Pattern)>),
}

impl Pattern {
    /// Return all names this pattern binds.
    pub fn bound_names(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        self.collect_bound_names(&mut out);
        out
    }

    fn collect_bound_names(&self, out: &mut BTreeSet<String>) {
        match self {
            Pattern::Wildcard | Pattern::Literal(_) => {}
            Pattern::Bind(name) => {
                out.insert(name.clone());
            }
            Pattern::Vector { elements, rest } => {
                for element in elements {
                    element.collect_bound_names(out);
                }
                if let Some(rest_pattern) = rest {
                    rest_pattern.collect_bound_names(out);
                }
            }
            Pattern::Map(pairs) => {
                for (_key, pattern) in pairs {
                    pattern.collect_bound_names(out);
                }
            }
        }
    }
}

/// A parameter in fn/loop — either a simple name or a destructuring pattern.
#[derive(Debug, Clone)]
pub enum FnParam {
    Simple(String),
    Destructure(Pattern),
}

/// A binding in let/loop — either a simple name or a destructuring pattern.
#[derive(Debug, Clone)]
pub enum LetBinding {
    Simple(String),
    Destructure(Pattern),
}

/// Result of a successful match: name → value bindings.
pub type Bindings = Vec<(String, Val)>;

// =========================================================================
// Pattern analysis (Val source → Pattern IR)
// =========================================================================

/// Analyze a Val (from source) into a Pattern.
///
/// Disambiguation rules:
/// - `Val::Nil`, `Val::Bool`, `Val::Int`, `Val::Float`, `Val::Str`, `Val::Keyword`
///   → `Pattern::Literal`
/// - `Val::Sym("_")` → `Pattern::Wildcard`
/// - `Val::Sym(name)` → `Pattern::Bind(name)`
/// - `Val::Vector(...)` → `Pattern::Vector` (recurse into elements)
/// - `Val::Map(...)` → `Pattern::Map` (recurse into values)
///
/// Note: `nil`, `true`, `false` arrive as `Val::Nil`, `Val::Bool` — NOT as
/// `Val::Sym("nil")`. The reader handles this distinction.
pub fn analyze_pattern(val: &Val) -> Result<Pattern, String> {
    match val {
        // Literals — exact match
        Val::Nil | Val::Bool(_) | Val::Int(_) | Val::Float(_) | Val::Str(_) | Val::Keyword(_) => {
            Ok(Pattern::Literal(val.clone()))
        }

        // Symbols — wildcard or bind
        Val::Sym(s) if s == "_" => Ok(Pattern::Wildcard),
        Val::Sym(s) => Ok(Pattern::Bind(s.clone())),

        // Vector pattern — positional, with optional & rest
        Val::Vector(items) => {
            let mut elements = Vec::new();
            let mut rest = None;
            let mut i = 0;
            while i < items.len() {
                if let Val::Sym(s) = &items[i] {
                    if s == "&" {
                        // Next element is the rest pattern
                        if i + 1 >= items.len() {
                            return Err("pattern: & must be followed by a binding".into());
                        }
                        if i + 2 < items.len() {
                            return Err("pattern: nothing allowed after & rest".into());
                        }
                        rest = Some(Box::new(analyze_pattern(&items[i + 1])?));
                        break;
                    }
                }
                elements.push(analyze_pattern(&items[i])?);
                i += 1;
            }
            Ok(Pattern::Vector { elements, rest })
        }

        // Map pattern — partial match by key
        Val::Map(m) => {
            let mut pat_pairs = Vec::new();
            for (k, v) in m.iter() {
                // Keys must be literals (keywords, strings, ints)
                let pat = analyze_pattern(v)?;
                pat_pairs.push((k.clone(), pat));
            }
            Ok(Pattern::Map(pat_pairs))
        }

        // List patterns — not supported (lists are call forms in Glia)
        Val::List(_) => Err("pattern: lists are not valid patterns (use vectors instead)".into()),

        // Everything else
        _ => Err(format!("pattern: unsupported pattern form: {val}")),
    }
}

/// Analyze a let/loop binding name — returns Simple for symbols, Destructure for patterns.
pub fn analyze_binding(val: &Val) -> Result<LetBinding, String> {
    match val {
        Val::Sym(s) => Ok(LetBinding::Simple(s.clone())),
        Val::Vector(_) | Val::Map(_) => Ok(LetBinding::Destructure(analyze_pattern(val)?)),
        _ => Err(format!(
            "binding name must be a symbol or destructuring pattern, got {val}"
        )),
    }
}

/// Analyze a fn/loop param — returns Simple for symbols, Destructure for patterns.
pub fn analyze_param(val: &Val) -> Result<FnParam, String> {
    match val {
        Val::Sym(s) => Ok(FnParam::Simple(s.clone())),
        Val::Vector(_) | Val::Map(_) => Ok(FnParam::Destructure(analyze_pattern(val)?)),
        _ => Err(format!(
            "param must be a symbol or destructuring pattern, got {val}"
        )),
    }
}

// =========================================================================
// Pattern matching (Pattern + Val → Option<Bindings>)
// =========================================================================

/// Attempt to match a pattern against a value.
/// Returns `Some(bindings)` on match, `None` on mismatch.
pub fn match_pattern(pattern: &Pattern, value: &Val) -> Option<Bindings> {
    match pattern {
        Pattern::Wildcard => Some(vec![]),

        Pattern::Bind(name) => Some(vec![(name.clone(), value.clone())]),

        Pattern::Literal(lit) => {
            if lit == value {
                Some(vec![])
            } else {
                None
            }
        }

        Pattern::Vector { elements, rest } => {
            let items = match value {
                Val::Vector(v) => v,
                Val::List(v) => v,
                _ => return None,
            };

            if let Some(rest_pat) = rest {
                // With rest: need at least elements.len() items
                if items.len() < elements.len() {
                    return None;
                }
                let mut bindings = Vec::new();
                for (pat, val) in elements.iter().zip(items.iter()) {
                    bindings.extend(match_pattern(pat, val)?);
                }
                // Rest collects remaining items as a list
                let rest_val = Val::List(items[elements.len()..].to_vec());
                bindings.extend(match_pattern(rest_pat, &rest_val)?);
                Some(bindings)
            } else {
                // Exact length
                if items.len() != elements.len() {
                    return None;
                }
                let mut bindings = Vec::new();
                for (pat, val) in elements.iter().zip(items.iter()) {
                    bindings.extend(match_pattern(pat, val)?);
                }
                Some(bindings)
            }
        }

        Pattern::Map(pat_pairs) => {
            let m = match value {
                Val::Map(m) => m,
                _ => return None,
            };

            let mut bindings = Vec::new();
            for (key, pat) in pat_pairs {
                match m.get(key) {
                    Some(val) => {
                        bindings.extend(match_pattern(pat, val)?);
                    }
                    None => return None, // Required key missing
                }
            }
            Some(bindings)
        }
    }
}

/// Apply pattern bindings to an Env frame. Used by let/fn/loop destructuring.
/// Returns Err if the pattern doesn't match the value.
pub fn bind_pattern(
    pattern: &Pattern,
    value: &Val,
    context: &str,
    env_set: &mut dyn FnMut(&str, Val),
) -> Result<(), Val> {
    match match_pattern(pattern, value) {
        Some(bindings) => {
            for (name, val) in bindings {
                env_set(&name, val);
            }
            Ok(())
        }
        None => Err(Val::from(format!(
            "{context}: destructuring failed — pattern did not match value {value}"
        ))),
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- analyze_pattern tests ---

    #[test]
    fn analyze_nil_literal() {
        assert_eq!(
            analyze_pattern(&Val::Nil).unwrap(),
            Pattern::Literal(Val::Nil)
        );
    }

    #[test]
    fn analyze_bool_literal() {
        assert_eq!(
            analyze_pattern(&Val::Bool(true)).unwrap(),
            Pattern::Literal(Val::Bool(true))
        );
    }

    #[test]
    fn analyze_int_literal() {
        assert_eq!(
            analyze_pattern(&Val::Int(42)).unwrap(),
            Pattern::Literal(Val::Int(42))
        );
    }

    #[test]
    fn analyze_str_literal() {
        assert_eq!(
            analyze_pattern(&Val::Str("hello".into())).unwrap(),
            Pattern::Literal(Val::Str("hello".into()))
        );
    }

    #[test]
    fn analyze_keyword_literal() {
        assert_eq!(
            analyze_pattern(&Val::Keyword("ok".into())).unwrap(),
            Pattern::Literal(Val::Keyword("ok".into()))
        );
    }

    #[test]
    fn analyze_wildcard() {
        assert_eq!(
            analyze_pattern(&Val::Sym("_".into())).unwrap(),
            Pattern::Wildcard
        );
    }

    #[test]
    fn analyze_bind() {
        assert_eq!(
            analyze_pattern(&Val::Sym("x".into())).unwrap(),
            Pattern::Bind("x".into())
        );
    }

    #[test]
    fn analyze_vector_pattern() {
        let pat = analyze_pattern(&Val::Vector(vec![
            Val::Sym("a".into()),
            Val::Sym("b".into()),
        ]))
        .unwrap();
        assert!(matches!(pat, Pattern::Vector { ref elements, rest: None } if elements.len() == 2));
    }

    #[test]
    fn analyze_vector_with_rest() {
        let pat = analyze_pattern(&Val::Vector(vec![
            Val::Sym("a".into()),
            Val::Sym("&".into()),
            Val::Sym("rest".into()),
        ]))
        .unwrap();
        match pat {
            Pattern::Vector { elements, rest } => {
                assert_eq!(elements.len(), 1);
                assert!(rest.is_some());
            }
            _ => panic!("expected Vector pattern"),
        }
    }

    #[test]
    fn analyze_map_pattern() {
        let pat = analyze_pattern(&Val::Map(ValMap::from_pairs(vec![(
            Val::Keyword("name".into()),
            Val::Sym("n".into()),
        )])))
        .unwrap();
        assert!(matches!(pat, Pattern::Map(pairs) if pairs.len() == 1));
    }

    #[test]
    fn analyze_list_errors() {
        assert!(analyze_pattern(&Val::List(vec![Val::Int(1)])).is_err());
    }

    #[test]
    fn analyze_nested_vector_of_maps() {
        let pat = analyze_pattern(&Val::Vector(vec![Val::Map(ValMap::from_pairs(vec![(
            Val::Keyword("x".into()),
            Val::Sym("a".into()),
        )]))]));
        assert!(pat.is_ok());
    }

    #[test]
    fn analyze_ampersand_must_have_binding() {
        let result = analyze_pattern(&Val::Vector(vec![
            Val::Sym("a".into()),
            Val::Sym("&".into()),
        ]));
        assert!(result.is_err());
    }

    #[test]
    fn analyze_nothing_after_rest() {
        let result = analyze_pattern(&Val::Vector(vec![
            Val::Sym("a".into()),
            Val::Sym("&".into()),
            Val::Sym("rest".into()),
            Val::Sym("extra".into()),
        ]));
        assert!(result.is_err());
    }

    // --- bound_names tests ---

    #[test]
    fn bound_names_simple_bind() {
        let pattern = Pattern::Bind("x".into());
        assert_eq!(pattern.bound_names(), BTreeSet::from(["x".to_string()]));
    }

    #[test]
    fn bound_names_nested_vector_map() {
        let pattern = Pattern::Vector {
            elements: vec![
                Pattern::Bind("a".into()),
                Pattern::Map(vec![
                    (Val::Keyword("k".into()), Pattern::Bind("b".into())),
                    (Val::Keyword("skip".into()), Pattern::Wildcard),
                ]),
            ],
            rest: Some(Box::new(Pattern::Bind("rest".into()))),
        };

        assert_eq!(
            pattern.bound_names(),
            BTreeSet::from(["a".to_string(), "b".to_string(), "rest".to_string()])
        );
    }

    // --- match_pattern tests ---

    #[test]
    fn match_wildcard() {
        assert_eq!(
            match_pattern(&Pattern::Wildcard, &Val::Int(42)),
            Some(vec![])
        );
    }

    #[test]
    fn match_bind() {
        let result = match_pattern(&Pattern::Bind("x".into()), &Val::Int(42));
        assert_eq!(result, Some(vec![("x".into(), Val::Int(42))]));
    }

    #[test]
    fn match_literal_equal() {
        assert_eq!(
            match_pattern(&Pattern::Literal(Val::Int(42)), &Val::Int(42)),
            Some(vec![])
        );
    }

    #[test]
    fn match_literal_mismatch() {
        assert_eq!(
            match_pattern(&Pattern::Literal(Val::Int(42)), &Val::Int(99)),
            None
        );
    }

    #[test]
    fn match_literal_nil() {
        assert_eq!(
            match_pattern(&Pattern::Literal(Val::Nil), &Val::Nil),
            Some(vec![])
        );
        assert_eq!(
            match_pattern(&Pattern::Literal(Val::Nil), &Val::Int(0)),
            None
        );
    }

    #[test]
    fn match_literal_keyword() {
        assert_eq!(
            match_pattern(
                &Pattern::Literal(Val::Keyword("ok".into())),
                &Val::Keyword("ok".into())
            ),
            Some(vec![])
        );
        assert_eq!(
            match_pattern(
                &Pattern::Literal(Val::Keyword("ok".into())),
                &Val::Keyword("err".into())
            ),
            None
        );
    }

    #[test]
    fn match_vector_exact_length() {
        let pat = Pattern::Vector {
            elements: vec![Pattern::Bind("a".into()), Pattern::Bind("b".into())],
            rest: None,
        };
        // Exact match
        let result = match_pattern(&pat, &Val::Vector(vec![Val::Int(1), Val::Int(2)]));
        assert_eq!(
            result,
            Some(vec![("a".into(), Val::Int(1)), ("b".into(), Val::Int(2))])
        );
        // Wrong length — too few
        assert_eq!(match_pattern(&pat, &Val::Vector(vec![Val::Int(1)])), None);
        // Wrong length — too many
        assert_eq!(
            match_pattern(
                &pat,
                &Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)])
            ),
            None
        );
    }

    #[test]
    fn match_vector_with_rest() {
        let pat = Pattern::Vector {
            elements: vec![Pattern::Bind("a".into())],
            rest: Some(Box::new(Pattern::Bind("rest".into()))),
        };
        // 3 elements — rest gets [2, 3]
        let result = match_pattern(
            &pat,
            &Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]),
        );
        assert_eq!(
            result,
            Some(vec![
                ("a".into(), Val::Int(1)),
                ("rest".into(), Val::List(vec![Val::Int(2), Val::Int(3)])),
            ])
        );
    }

    #[test]
    fn match_vector_rest_empty() {
        let pat = Pattern::Vector {
            elements: vec![Pattern::Bind("a".into())],
            rest: Some(Box::new(Pattern::Bind("rest".into()))),
        };
        // Exactly 1 element — rest gets []
        let result = match_pattern(&pat, &Val::Vector(vec![Val::Int(1)]));
        assert_eq!(
            result,
            Some(vec![
                ("a".into(), Val::Int(1)),
                ("rest".into(), Val::List(vec![])),
            ])
        );
    }

    #[test]
    fn match_vector_rest_too_few() {
        let pat = Pattern::Vector {
            elements: vec![Pattern::Bind("a".into()), Pattern::Bind("b".into())],
            rest: Some(Box::new(Pattern::Bind("rest".into()))),
        };
        // Only 1 element — need at least 2
        assert_eq!(match_pattern(&pat, &Val::Vector(vec![Val::Int(1)])), None);
    }

    #[test]
    fn match_vector_nested() {
        let pat = Pattern::Vector {
            elements: vec![
                Pattern::Vector {
                    elements: vec![Pattern::Bind("a".into())],
                    rest: None,
                },
                Pattern::Bind("b".into()),
            ],
            rest: None,
        };
        let val = Val::Vector(vec![Val::Vector(vec![Val::Int(1)]), Val::Int(2)]);
        let result = match_pattern(&pat, &val);
        assert_eq!(
            result,
            Some(vec![("a".into(), Val::Int(1)), ("b".into(), Val::Int(2))])
        );
    }

    #[test]
    fn match_vector_against_list() {
        // Vector pattern also matches Val::List (both are sequential)
        let pat = Pattern::Vector {
            elements: vec![Pattern::Bind("a".into())],
            rest: None,
        };
        assert_eq!(
            match_pattern(&pat, &Val::List(vec![Val::Int(1)])),
            Some(vec![("a".into(), Val::Int(1))])
        );
    }

    #[test]
    fn match_vector_against_non_seq() {
        let pat = Pattern::Vector {
            elements: vec![Pattern::Bind("a".into())],
            rest: None,
        };
        assert_eq!(match_pattern(&pat, &Val::Int(42)), None);
    }

    #[test]
    fn match_map_key_present() {
        let pat = Pattern::Map(vec![(
            Val::Keyword("name".into()),
            Pattern::Bind("n".into()),
        )]);
        let val = Val::Map(ValMap::from_pairs(vec![(
            Val::Keyword("name".into()),
            Val::Str("Alice".into()),
        )]));
        assert_eq!(
            match_pattern(&pat, &val),
            Some(vec![("n".into(), Val::Str("Alice".into()))])
        );
    }

    #[test]
    fn match_map_key_missing() {
        let pat = Pattern::Map(vec![(
            Val::Keyword("name".into()),
            Pattern::Bind("n".into()),
        )]);
        let val = Val::Map(ValMap::from_pairs(vec![(
            Val::Keyword("age".into()),
            Val::Int(30),
        )]));
        assert_eq!(match_pattern(&pat, &val), None);
    }

    #[test]
    fn match_map_extra_keys_ignored() {
        let pat = Pattern::Map(vec![(
            Val::Keyword("name".into()),
            Pattern::Bind("n".into()),
        )]);
        let val = Val::Map(ValMap::from_pairs(vec![
            (Val::Keyword("name".into()), Val::Str("Alice".into())),
            (Val::Keyword("age".into()), Val::Int(30)),
        ]));
        assert_eq!(
            match_pattern(&pat, &val),
            Some(vec![("n".into(), Val::Str("Alice".into()))])
        );
    }

    #[test]
    fn match_map_multiple_keys() {
        let pat = Pattern::Map(vec![
            (Val::Keyword("name".into()), Pattern::Bind("n".into())),
            (Val::Keyword("age".into()), Pattern::Bind("a".into())),
        ]);
        let val = Val::Map(ValMap::from_pairs(vec![
            (Val::Keyword("name".into()), Val::Str("Alice".into())),
            (Val::Keyword("age".into()), Val::Int(30)),
        ]));
        let result = match_pattern(&pat, &val);
        assert_eq!(
            result,
            Some(vec![
                ("n".into(), Val::Str("Alice".into())),
                ("a".into(), Val::Int(30)),
            ])
        );
    }

    #[test]
    fn match_map_nested_vector() {
        let pat = Pattern::Map(vec![(
            Val::Keyword("coords".into()),
            Pattern::Vector {
                elements: vec![Pattern::Bind("lat".into()), Pattern::Bind("lng".into())],
                rest: None,
            },
        )]);
        let val = Val::Map(ValMap::from_pairs(vec![(
            Val::Keyword("coords".into()),
            Val::Vector(vec![Val::Float(1.0), Val::Float(2.0)]),
        )]));
        let result = match_pattern(&pat, &val);
        assert_eq!(
            result,
            Some(vec![
                ("lat".into(), Val::Float(1.0)),
                ("lng".into(), Val::Float(2.0)),
            ])
        );
    }

    #[test]
    fn match_map_against_non_map() {
        let pat = Pattern::Map(vec![(Val::Keyword("x".into()), Pattern::Bind("a".into()))]);
        assert_eq!(match_pattern(&pat, &Val::Int(42)), None);
    }

    #[test]
    fn match_deeply_nested() {
        // [{:name name} {:name other}]
        let pat = Pattern::Vector {
            elements: vec![
                Pattern::Map(vec![(
                    Val::Keyword("name".into()),
                    Pattern::Bind("a".into()),
                )]),
                Pattern::Map(vec![(
                    Val::Keyword("name".into()),
                    Pattern::Bind("b".into()),
                )]),
            ],
            rest: None,
        };
        let val = Val::Vector(vec![
            Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("name".into()),
                Val::Str("Alice".into()),
            )])),
            Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("name".into()),
                Val::Str("Bob".into()),
            )])),
        ]);
        let result = match_pattern(&pat, &val);
        assert_eq!(
            result,
            Some(vec![
                ("a".into(), Val::Str("Alice".into())),
                ("b".into(), Val::Str("Bob".into())),
            ])
        );
    }

    #[test]
    fn match_literal_with_wildcard_nested() {
        // [42 _] — first element must be 42, second ignored
        let pat = Pattern::Vector {
            elements: vec![Pattern::Literal(Val::Int(42)), Pattern::Wildcard],
            rest: None,
        };
        assert!(match_pattern(&pat, &Val::Vector(vec![Val::Int(42), Val::Int(99)])).is_some());
        assert!(match_pattern(&pat, &Val::Vector(vec![Val::Int(0), Val::Int(99)])).is_none());
    }

    // --- bind_pattern tests ---

    #[test]
    fn bind_pattern_success() {
        let pat = Pattern::Vector {
            elements: vec![Pattern::Bind("a".into()), Pattern::Bind("b".into())],
            rest: None,
        };
        let val = Val::Vector(vec![Val::Int(1), Val::Int(2)]);
        let mut bindings = Vec::new();
        bind_pattern(&pat, &val, "test", &mut |name, val| {
            bindings.push((name.to_string(), val));
        })
        .unwrap();
        assert_eq!(bindings.len(), 2);
    }

    #[test]
    fn bind_pattern_failure() {
        let pat = Pattern::Vector {
            elements: vec![Pattern::Bind("a".into())],
            rest: None,
        };
        let val = Val::Int(42); // not a vector
        let result = bind_pattern(&pat, &val, "let", &mut |_, _| {});
        assert!(result.is_err());
    }

    // --- analyze_binding / analyze_param tests ---

    #[test]
    fn analyze_binding_simple() {
        let b = analyze_binding(&Val::Sym("x".into())).unwrap();
        assert!(matches!(b, LetBinding::Simple(s) if s == "x"));
    }

    #[test]
    fn analyze_binding_destructure() {
        let b = analyze_binding(&Val::Vector(vec![
            Val::Sym("a".into()),
            Val::Sym("b".into()),
        ]))
        .unwrap();
        assert!(matches!(b, LetBinding::Destructure(_)));
    }

    #[test]
    fn analyze_param_simple() {
        let p = analyze_param(&Val::Sym("x".into())).unwrap();
        assert!(matches!(p, FnParam::Simple(s) if s == "x"));
    }

    #[test]
    fn analyze_param_destructure() {
        let p = analyze_param(&Val::Vector(vec![
            Val::Sym("a".into()),
            Val::Sym("b".into()),
        ]))
        .unwrap();
        assert!(matches!(p, FnParam::Destructure(_)));
    }
}
