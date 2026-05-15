//! Saved views — `save_view`, `view`, `list_views`. Persist reusable
//! Cypher queries as `:View` nodes; `view(name, params)` substitutes
//! `$key` tokens via `escape_str` at run time and renders the result.

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::render::{format_table_md, md_cell};
use crate::util::{chrono_now_iso, err_text, ok_text, safe_name_with_dashes};

/// Substitute `$key` tokens in `cypher` with `escape_str(value)` for each
/// `(key, value)` in `params`. Tokens are matched as `$` followed by an
/// identifier-shaped run (`[A-Za-z_][A-Za-z0-9_]*`); unknown tokens stay.
pub(crate) fn substitute_view_params(
    cypher: &str,
    params: &serde_json::Map<String, Value>,
) -> String {
    let bytes = cypher.as_bytes();
    let mut out = String::with_capacity(cypher.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'$' && i + 1 < bytes.len() {
            let start = i + 1;
            let mut end = start;
            if end < bytes.len() && (bytes[end].is_ascii_alphabetic() || bytes[end] == b'_') {
                end += 1;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                let key = &cypher[start..end];
                if let Some(v) = params.get(key) {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        Value::Bool(b) => b.to_string(),
                        Value::Null => "null".to_string(),
                        other => other.to_string(),
                    };
                    out.push_str(&escape_str(&s));
                    i = end;
                    continue;
                }
            }
        }
        out.push(c as char);
        i += 1;
    }
    out
}

pub fn handle_save_view(db: &Db, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) if safe_name_with_dashes(s) => s.to_string(),
        Some(s) => return err_text(format!("invalid view name: {s:?}")),
        None => return err_text("missing required argument: name".to_string()),
    };
    let cypher = match params.get("cypher").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return err_text("missing required argument: cypher".to_string()),
    };
    let description = params
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let now = chrono_now_iso();
    let q = format!(
        "MERGE (v:View {{name: {name}}}) \
         SET v.cypher = {cypher}, v.description = {desc}, v.updated_at = {now}, \
             v.created_at = coalesce(v.created_at, {now})",
        name = escape_str(&name),
        cypher = escape_str(&cypher),
        desc = escape_str(&description),
        now = escape_str(&now),
    );
    if let Err(e) = db.run(&q) {
        return err_text(format!("save_view failed: {e}"));
    }
    ok_text(format!("saved view `{name}`"))
}

pub fn handle_view(db: &Db, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) if safe_name_with_dashes(s) => s.to_string(),
        Some(s) => return err_text(format!("invalid view name: {s:?}")),
        None => return err_text("missing required argument: name".to_string()),
    };
    let lookup = format!(
        "MATCH (v:View {{name: {n}}}) RETURN v.cypher AS cypher LIMIT 1",
        n = escape_str(&name),
    );
    let cypher_template = match db.query(&lookup) {
        Ok(t) if !t.rows.is_empty() => t.rows[0]
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
        Ok(_) => return ok_text(format!("_(no view named `{name}`)_")),
        Err(e) => return err_text(format!("view lookup failed: {e}")),
    };
    let empty = serde_json::Map::new();
    let map = params
        .get("params")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let cypher = substitute_view_params(&cypher_template, map);

    let now = chrono_now_iso();
    let _ = db.run(&format!(
        "MATCH (v:View {{name: {n}}}) SET v.last_run_at = {now}",
        n = escape_str(&name),
        now = escape_str(&now),
    ));

    let mut out = String::new();
    out.push_str(&format!("# View `{name}`\n\n"));
    out.push_str("```cypher\n");
    out.push_str(&cypher);
    if !cypher.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n\n");
    match db.query(&cypher) {
        Ok(t) => out.push_str(&format_table_md(&t)),
        Err(e) => out.push_str(&format!("_(query failed: {e})_")),
    }
    ok_text(out)
}

pub fn handle_list_views(db: &Db) -> Value {
    let q = "MATCH (v:View) RETURN v.name AS name, v.description AS description, \
             v.created_at AS created_at, v.last_run_at AS last_run_at \
             ORDER BY v.name";
    let t = match db.query(q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("list_views failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text("_(no saved views)_".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("# Saved views ({})\n\n", t.rows.len()));
    out.push_str("| name | description | created_at | last_run_at |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    let n_i = t.col("name");
    let d_i = t.col("description");
    let c_i = t.col("created_at");
    let l_i = t.col("last_run_at");
    for row in &t.rows {
        let n = n_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let d = d_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let c = c_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let l = l_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        out.push_str(&format!("| `{n}` | {d} | {c} | {l} |\n"));
    }
    ok_text(out.trim_end().to_string())
}
