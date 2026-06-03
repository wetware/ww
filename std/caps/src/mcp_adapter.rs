//! Shared helpers for MCP-facing Glia adapters.
//!
//! Callers keep their exposed tool surface explicit by passing a `ToolSpec`
//! table. This module owns the security-sensitive string escaping, action
//! validation, expression rendering, and Glia error/JSON serialization.

use glia::Val;

/// One known MCP tool and its allowed action templates.
pub struct ToolSpec {
    pub name: &'static str,
    pub action_policy: ActionPolicy,
    pub actions: &'static [ToolAction],
}

/// How an MCP tool uses the conventional `action` argument.
pub enum ActionPolicy {
    /// The tool requires a non-empty safe `action` string.
    RequiredSafe,
    /// The tool does not dispatch on `action`; any supplied value is ignored.
    Ignore,
}

/// A single allowed action for a tool.
pub struct ToolAction {
    pub action: Option<&'static str>,
    pub template: &'static [ExprPart],
}

/// A segment of a rendered Glia expression.
pub enum ExprPart {
    Literal(&'static str),
    QuotedStringField {
        field: &'static str,
        default: &'static str,
    },
    U64Field {
        field: &'static str,
        default: u64,
    },
}

/// Escape a string for safe embedding inside a Glia double-quoted string literal.
pub fn glia_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Validate that a Glia identifier/keyword segment cannot break expression syntax.
pub fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Translate an MCP tool call into a Glia expression using the supplied tool table.
pub fn tool_call_to_glia(
    specs: &[ToolSpec],
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<String> {
    let spec = specs.iter().find(|spec| spec.name == tool_name)?;

    let selected = match spec.action_policy {
        ActionPolicy::RequiredSafe => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
            if !is_safe_identifier(action) {
                return None;
            }
            spec.actions
                .iter()
                .find(|candidate| candidate.action == Some(action))?
        }
        ActionPolicy::Ignore => spec
            .actions
            .iter()
            .find(|candidate| candidate.action.is_none())?,
    };

    render_expr(selected.template, args)
}

/// Returns true when `tool_name` is present in the supplied tool table.
pub fn has_tool(specs: &[ToolSpec], tool_name: &str) -> bool {
    specs.iter().any(|spec| spec.name == tool_name)
}

fn render_expr(parts: &[ExprPart], args: &serde_json::Value) -> Option<String> {
    let mut out = String::new();
    for part in parts {
        match part {
            ExprPart::Literal(text) => out.push_str(text),
            ExprPart::QuotedStringField { field, default } => {
                let value = args.get(*field).and_then(|v| v.as_str()).unwrap_or(default);
                out.push('"');
                out.push_str(&glia_escape(value));
                out.push('"');
            }
            ExprPart::U64Field { field, default } => {
                let value = args
                    .get(*field)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(*default);
                out.push_str(&value.to_string());
            }
        }
    }
    Some(out)
}

/// Render a `Val` into a JSON representation suitable for MCP structured data.
pub fn val_to_json(v: &Val) -> serde_json::Value {
    match v {
        Val::Nil => serde_json::Value::Null,
        Val::Bool(b) => serde_json::Value::Bool(*b),
        Val::Int(i) => serde_json::Value::from(*i),
        Val::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Val::Str(s) | Val::Sym(s) | Val::Keyword(s) => serde_json::Value::String(s.clone()),
        Val::List(items) | Val::Vector(items) | Val::Set(items) => {
            serde_json::Value::Array(items.iter().map(val_to_json).collect())
        }
        Val::Map(map) => {
            let mut object = serde_json::Map::new();
            for (k, v) in map {
                object.insert(val_to_json_key(k), val_to_json(v));
            }
            serde_json::Value::Object(object)
        }
        Val::Bytes(bytes) => serde_json::Value::String(format!("<{} bytes>", bytes.len())),
        other => serde_json::Value::String(format!("{other}")),
    }
}

/// Convert a Glia error value into MCP human-readable error text.
pub fn val_to_mcp_error_text(err: &Val) -> String {
    let inner = glia::error::unwrap_thrown(err).unwrap_or(err);

    let msg = glia::error::message(inner)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{inner}"));

    let Some(tag) = glia::error::type_tag(inner) else {
        return msg;
    };

    let mut text = format!("[{tag}] {msg}");
    if let Some(hint) = glia::error::hint(inner) {
        text.push_str("\n\nhint: ");
        text.push_str(hint);
    }
    text
}

/// Convert a Glia error value into MCP structured error data.
pub fn val_to_mcp_error_data(err: &Val) -> serde_json::Value {
    let inner = glia::error::unwrap_thrown(err).unwrap_or(err);
    let Some(data) = glia::error::data(inner) else {
        return serde_json::Value::Null;
    };

    let mut object = serde_json::Map::new();
    for (k, v) in data {
        object.insert(val_to_json_key(k), val_to_json(v));
    }
    serde_json::Value::Object(object)
}

fn val_to_json_key(v: &Val) -> String {
    match v {
        Val::Keyword(s) | Val::Str(s) | Val::Sym(s) => s.clone(),
        other => format!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_ID_EXPR: &[ExprPart] = &[ExprPart::Literal("(perform host :id)")];
    const HOST_ACTIONS: &[ToolAction] = &[ToolAction {
        action: Some("id"),
        template: HOST_ID_EXPR,
    }];
    const IMPORT_EXPR: &[ExprPart] = &[
        ExprPart::Literal("(perform import "),
        ExprPart::QuotedStringField {
            field: "path",
            default: "",
        },
        ExprPart::Literal(")"),
    ];
    const IMPORT_ACTIONS: &[ToolAction] = &[ToolAction {
        action: None,
        template: IMPORT_EXPR,
    }];
    const IDENTITY_SIGN_EXPR: &[ExprPart] = &[
        ExprPart::Literal("(perform identity :sign "),
        ExprPart::QuotedStringField {
            field: "domain",
            default: "default",
        },
        ExprPart::Literal(" "),
        ExprPart::U64Field {
            field: "nonce",
            default: 0,
        },
        ExprPart::Literal(")"),
    ];
    const IDENTITY_ACTIONS: &[ToolAction] = &[ToolAction {
        action: Some("sign"),
        template: IDENTITY_SIGN_EXPR,
    }];
    const TOOL_SPECS: &[ToolSpec] = &[
        ToolSpec {
            name: "host",
            action_policy: ActionPolicy::RequiredSafe,
            actions: HOST_ACTIONS,
        },
        ToolSpec {
            name: "import",
            action_policy: ActionPolicy::Ignore,
            actions: IMPORT_ACTIONS,
        },
        ToolSpec {
            name: "identity",
            action_policy: ActionPolicy::RequiredSafe,
            actions: IDENTITY_ACTIONS,
        },
    ];

    #[test]
    fn safe_identifier_accepts_expected_names() {
        assert!(is_safe_identifier("peers"));
        assert!(is_safe_identifier("findProviders123"));
        assert!(is_safe_identifier("find-providers"));
        assert!(is_safe_identifier("my_tool"));
        assert!(is_safe_identifier("a-b_c"));
    }

    #[test]
    fn safe_identifier_rejects_empty_and_injection_chars() {
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("foo bar"));
        assert!(!is_safe_identifier("foo\"bar"));
        assert!(!is_safe_identifier("foo)bar"));
        assert!(!is_safe_identifier("foo(bar"));
        assert!(!is_safe_identifier(":keyword"));
        assert!(!is_safe_identifier("foo;bar"));
    }

    #[test]
    fn glia_escape_escapes_quotes_and_backslashes() {
        assert_eq!(glia_escape("hello world"), "hello world");
        assert_eq!(glia_escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(glia_escape(r"path\to\file"), r"path\\to\\file");
        assert_eq!(glia_escape(r#"a\"b"#), r#"a\\\"b"#);
        assert_eq!(glia_escape(""), "");
    }

    #[test]
    fn tool_call_renders_known_action() {
        let expr = tool_call_to_glia(TOOL_SPECS, "host", &serde_json::json!({ "action": "id" }));
        assert_eq!(expr, Some("(perform host :id)".into()));
    }

    #[test]
    fn tool_call_rejects_unsafe_action_identifier() {
        let expr = tool_call_to_glia(
            TOOL_SPECS,
            "host",
            &serde_json::json!({ "action": "id) (evil" }),
        );
        assert_eq!(expr, None);
    }

    #[test]
    fn tool_call_escapes_user_string_fields() {
        let expr = tool_call_to_glia(
            TOOL_SPECS,
            "import",
            &serde_json::json!({ "path": r#"core") (evil"# }),
        );
        assert_eq!(expr, Some(r#"(perform import "core\") (evil")"#.into()));
    }

    #[test]
    fn tool_call_renders_u64_fields_with_defaults() {
        let expr = tool_call_to_glia(
            TOOL_SPECS,
            "identity",
            &serde_json::json!({ "action": "sign", "domain": "test" }),
        );
        assert_eq!(expr, Some(r#"(perform identity :sign "test" 0)"#.into()));
    }

    #[test]
    fn has_tool_checks_supplied_table() {
        assert!(has_tool(TOOL_SPECS, "host"));
        assert!(!has_tool(TOOL_SPECS, "routing"));
    }

    fn unbound_err() -> Val {
        glia::error::unbound_symbol("foo", Some("did you mean 'bar'?"))
    }

    #[test]
    fn envelope_text_includes_tag_and_message_for_structured_error() {
        let text = val_to_mcp_error_text(&unbound_err());
        assert!(
            text.starts_with("[glia.error/unbound-symbol]"),
            "got: {text}"
        );
        assert!(text.contains("unbound symbol: foo"), "got: {text}");
        assert!(text.contains("hint: did you mean 'bar'?"), "got: {text}");
    }

    #[test]
    fn envelope_text_falls_back_to_plain_string_for_legacy_errors() {
        assert_eq!(
            val_to_mcp_error_text(&Val::Str("legacy plain error".into())),
            "legacy plain error"
        );
    }

    #[test]
    fn envelope_data_carries_full_structured_map() {
        let data = val_to_mcp_error_data(&unbound_err());
        let object = data.as_object().expect("structured data should be object");
        assert_eq!(
            object.get("glia.error/type").and_then(|v| v.as_str()),
            Some("glia.error/unbound-symbol")
        );
        assert!(object
            .get("glia.error/message")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("unbound symbol: foo"))
            .unwrap_or(false));
        assert_eq!(
            object.get("glia.error/hint").and_then(|v| v.as_str()),
            Some("did you mean 'bar'?")
        );
        assert_eq!(
            object.get("glia.error/symbol").and_then(|v| v.as_str()),
            Some("foo")
        );
    }

    #[test]
    fn envelope_data_returns_null_for_legacy_string_errors() {
        assert!(val_to_mcp_error_data(&Val::Str("legacy".into())).is_null());
    }

    #[test]
    fn envelope_peels_glia_exception_carrier() {
        let carrier = Val::Effect {
            effect_type: "glia.exception".into(),
            data: Box::new(unbound_err()),
        };
        let text = val_to_mcp_error_text(&carrier);
        assert!(
            text.starts_with("[glia.error/unbound-symbol]"),
            "got: {text}"
        );
        let data = val_to_mcp_error_data(&carrier);
        assert_eq!(
            data.get("glia.error/type").and_then(|v| v.as_str()),
            Some("glia.error/unbound-symbol")
        );
    }

    #[test]
    fn envelope_text_honors_user_thrown_ex_info() {
        let err = glia::error::user(
            Val::Keyword("network".into()),
            "peer unreachable",
            glia::ValMap::from_pairs(vec![(
                Val::Keyword("peer".into()),
                Val::Str("QmFoo".into()),
            )]),
        );
        let text = val_to_mcp_error_text(&err);
        assert!(text.starts_with("[network]"), "got: {text}");
        let data = val_to_mcp_error_data(&err);
        let object = data.as_object().unwrap();
        assert_eq!(object.get("peer").and_then(|v| v.as_str()), Some("QmFoo"));
    }

    #[test]
    fn val_to_json_recurses_through_composites() {
        let value = Val::Map(glia::ValMap::from_pairs(vec![(
            Val::Keyword("items".into()),
            Val::Vector(vec![
                Val::Int(1),
                Val::Bool(true),
                Val::Bytes(vec![1, 2, 3]),
            ]),
        )]));
        assert_eq!(
            val_to_json(&value),
            serde_json::json!({ "items": [1, true, "<3 bytes>"] })
        );
    }
}
