//! `history` MCP tool — list the `:GitCommit` snapshots recorded in
//! the graph, newest first, joined to `:Author` via `[:AUTHORED]`.

use codegraph_core::Db;
use serde_json::Value;

use crate::render::md_cell;
use crate::util::{err_text, ok_text};

pub fn handle_history(db: &Db, params: &Value) -> Value {
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .max(1);
    let q = format!(
        "MATCH (c:GitCommit) \
         OPTIONAL MATCH (a:Author)-[:AUTHORED]->(c) \
         RETURN c.short_hash AS short, c.timestamp AS ts, a.name AS author, c.message AS message \
         ORDER BY c.timestamp DESC LIMIT {limit}"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("history query failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text(
            "_(no `:GitCommit` nodes recorded — run the indexer inside a git repo)_".to_string(),
        );
    }
    let mut out = String::new();
    out.push_str(&format!("# Indexed commits ({})\n\n", t.rows.len()));
    out.push_str("| short | timestamp | author | message |\n| --- | --- | --- | --- |\n");
    let s_i = t.col("short");
    let ts_i = t.col("ts");
    let a_i = t.col("author");
    let m_i = t.col("message");
    for row in &t.rows {
        let s = s_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let ts = ts_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let a = a_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let m = m_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        out.push_str(&format!("| `{s}` | {ts} | {a} | {m} |\n"));
    }
    ok_text(out.trim_end().to_string())
}
