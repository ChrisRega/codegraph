//! Small shared helpers used by every MCP tool handler. Lives in its own
//! module so adding a new helper doesn't bloat `main.rs`.

use serde_json::{json, Value};

/// Sourced from `codegraph-core::time::now_iso` so the indexer and the
/// MCP server emit identical timestamps. See refactoring 1b.
pub use codegraph_core::time::now_iso as chrono_now_iso;

/// Wrap `text` in the MCP `tools/call` success envelope.
pub fn ok_text(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

/// Wrap `msg` in the MCP `tools/call` error envelope (sets `isError: true`
/// so the agent can distinguish "wrong input" from "tool produced text").
pub fn err_text(msg: String) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

/// True if `s` is a bare identifier — letter/underscore start, then
/// alphanumeric/underscore. Used as the inline-Cypher safety check for
/// any user-supplied label or property name we splice into a query.
pub fn safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Looser identifier check — allows dashes too. Used for
/// user-curated names (saved views, concepts) that go into a `name`
/// property, never directly into Cypher syntax. Capped at 80 chars
/// because longer names defeat the dossier point.
pub fn safe_name_with_dashes(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Pull `label` / `key` / `value` out of an MCP `tools/call` `arguments`
/// object, validating `label` and `key` against [`safe_ident`].
pub fn parse_node_address(params: &Value) -> Result<(String, String, String), String> {
    parse_node_address_with_defaults(params, None, None)
}

/// Same shape as [`parse_node_address`] but `label` / `key` may have
/// defaults, used by handlers like `impact` and `find_symbol` where
/// "Function/qualified_name" is the overwhelming common case.
pub fn parse_node_address_with_defaults(
    params: &Value,
    default_label: Option<&str>,
    default_key: Option<&str>,
) -> Result<(String, String, String), String> {
    let label = params
        .get("label")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| default_label.map(str::to_string))
        .ok_or("missing required argument: label")?;
    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| default_key.map(str::to_string))
        .ok_or("missing required argument: key")?;
    let value = params
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or("missing required argument: value")?
        .to_string();
    if !safe_ident(&label) {
        return Err(format!("invalid label: {label}"));
    }
    if !safe_ident(&key) {
        return Err(format!("invalid key: {key}"));
    }
    Ok((label, key, value))
}
