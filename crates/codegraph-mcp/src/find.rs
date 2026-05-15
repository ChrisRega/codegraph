//! `find_symbol` MCP tool — fuzzy substring search over `:Function`
//! and `:Symbol` qualified_name / name with relevance ranking
//! (exact > startsWith on name > startsWith on qn > contains).

use codegraph_core::{escape_str, Cell, Db};
use serde_json::Value;

use crate::render::md_cell;
use crate::util::{err_text, ok_text};

#[derive(Clone)]
struct SymbolHit {
    label: String,
    qn: String,
    name: String,
    kind: String,
    path: String,
    line: i64,
    body: String,
}

/// Relevance score (lower is better): 0 exact, 1 startsWith on name,
/// 2 startsWith on qn, 3 contains on name, 4 contains on qn.
fn relevance(needle_lower: &str, hit: &SymbolHit) -> u8 {
    let name = hit.name.to_lowercase();
    let qn = hit.qn.to_lowercase();
    if name == needle_lower || qn == needle_lower {
        0
    } else if name.starts_with(needle_lower) {
        1
    } else if qn.starts_with(needle_lower) {
        2
    } else if name.contains(needle_lower) {
        3
    } else {
        4
    }
}

fn collect_symbols(db: &Db, label: &str, kind_filter_clause: &str) -> Vec<SymbolHit> {
    // Pull a generous candidate set; final filtering / scoring happens
    // client-side so we don't depend on velr's substring-match semantics.
    let q = format!(
        "MATCH (s:{label}) {kind_filter_clause} \
         OPTIONAL MATCH (s)-[:DEFINED_IN]->(f:File) \
         RETURN s.qualified_name AS qn, s.name AS name, s.kind AS kind, \
                f.path AS path, s.line_start AS line, s.body AS body \
         LIMIT 5000"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let qn_i = t.col("qn");
    let nm_i = t.col("name");
    let kd_i = t.col("kind");
    let pa_i = t.col("path");
    let ln_i = t.col("line");
    let bd_i = t.col("body");
    t.rows
        .iter()
        .map(|row| SymbolHit {
            label: label.to_string(),
            qn: qn_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            name: nm_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            kind: kd_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            path: pa_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            line: ln_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_i64())
                .unwrap_or(0),
            body: bd_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect()
}

pub fn handle_find_symbol(db: &Db, params: &Value) -> Value {
    let needle = match params.get("query").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return err_text("missing required argument: query".to_string()),
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(25)
        .max(1) as usize;
    let kind = params
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let kind_clause = match &kind {
        Some(k) if !k.is_empty() => format!("WHERE s.kind = {}", escape_str(k)),
        _ => String::new(),
    };

    let needle_lower = needle.to_lowercase();
    let mut hits: Vec<SymbolHit> = collect_symbols(db, "Function", &kind_clause);
    hits.extend(collect_symbols(db, "Symbol", &kind_clause));

    hits.retain(|h| {
        h.qn.to_lowercase().contains(&needle_lower) || h.name.to_lowercase().contains(&needle_lower)
    });

    hits.sort_by(|a, b| {
        let ra = relevance(&needle_lower, a);
        let rb = relevance(&needle_lower, b);
        ra.cmp(&rb)
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.qn.cmp(&b.qn))
    });

    let total = hits.len();
    if total == 0 {
        return ok_text(format!(
            "_(no `:Function` or `:Symbol` matched `{needle}`)_"
        ));
    }
    hits.truncate(limit);

    let mut out = String::new();
    out.push_str(&format!(
        "# `find_symbol({needle:?})` — {} of {total} match{}\n\n",
        hits.len(),
        if total == 1 { "" } else { "es" }
    ));
    out.push_str("| kind | qualified_name | location | signature |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for h in &hits {
        let loc = if h.path.is_empty() {
            "—".to_string()
        } else if h.line > 0 {
            format!("`{}:{}`", h.path, h.line)
        } else {
            format!("`{}`", h.path)
        };
        let sig = h
            .body
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .replace('|', "\\|");
        let sig = if sig.is_empty() {
            "—".to_string()
        } else {
            format!("`{sig}`")
        };
        let label_tag = format!("{}:{}", h.label, h.kind);
        out.push_str(&format!(
            "| `{}` | `{}` | {loc} | {sig} |\n",
            md_cell(&Cell::Text(label_tag)),
            md_cell(&Cell::Text(h.qn.clone())),
        ));
    }
    if total > hits.len() {
        out.push_str(&format!(
            "\n_… {} more (raise `limit`)_",
            total - hits.len()
        ));
    }
    ok_text(out.trim_end().to_string())
}
