//! Watch / unwatch / list_watches MCP tools. The `:Watch` label is a
//! cross-session marker the indexer's Phase 7 reads to fire watch-trigger
//! `:Note`s when the marked node's body changes.

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::util::{chrono_now_iso, err_text, ok_text, parse_node_address};

fn current_head_hash(db: &Db) -> String {
    db.query("MATCH (h:GitCommit)-[:SNAPSHOT_OF]->(:Workspace) RETURN h.hash AS hash LIMIT 1")
        .ok()
        .and_then(|t| t.rows.into_iter().next())
        .and_then(|r| r.into_iter().next())
        .and_then(|c| c.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn handle_watch(db: &Db, params: &Value) -> Value {
    let (label, key, value) = match parse_node_address(params) {
        Ok(t) => t,
        Err(e) => return err_text(e),
    };
    let val_lit = escape_str(&value);
    let head = current_head_hash(db);
    let head_lit = escape_str(&head);
    let now = chrono_now_iso();
    let now_lit = escape_str(&now);
    // velr quirk: the SET inside the same statement that filters on
    // a coalesced default could be tricky. Keep the contract simple:
    // baseline = current body (may be NULL → that's fine, the next
    // diff just won't fire until body becomes non-NULL).
    let q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}}) \
         SET n:Watch, n.watch_baseline_body = n.body, \
             n.watch_set_at_commit = {head_lit}, n.watch_set_at = {now_lit}"
    );
    if let Err(e) = db.run(&q) {
        return err_text(format!("watch failed: {e}"));
    }
    let count_q = format!("MATCH (n:{label}:Watch {{{key}: {val_lit}}}) RETURN count(n) AS c");
    let n = db
        .query(&count_q)
        .ok()
        .and_then(|t| t.rows.into_iter().next())
        .and_then(|r| r.into_iter().next())
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    if n == 0 {
        return err_text(format!(
            "no `:{label}` matched `{key} = {value:?}` — nothing watched"
        ));
    }
    ok_text(format!(
        "watching `:{label} {{{key}: {value:?}}}` at commit `{}`",
        if head.is_empty() {
            "(no HEAD)".to_string()
        } else {
            head[..head.len().min(8)].to_string()
        }
    ))
}

pub fn handle_unwatch(db: &Db, params: &Value) -> Value {
    let (label, key, value) = match parse_node_address(params) {
        Ok(t) => t,
        Err(e) => return err_text(e),
    };
    let val_lit = escape_str(&value);
    let q = format!(
        "MATCH (n:{label}:Watch {{{key}: {val_lit}}}) \
         REMOVE n:Watch \
         REMOVE n.watch_baseline_body, n.watch_set_at_commit, n.watch_set_at"
    );
    if let Err(e) = db.run(&q) {
        return err_text(format!("unwatch failed: {e}"));
    }
    ok_text(format!("unwatched `:{label} {{{key}: {value:?}}}`"))
}

pub fn handle_list_watches(db: &Db) -> Value {
    let q = "MATCH (w:Watch) \
             RETURN labels(w) AS lbls, w.qualified_name AS qn, w.path AS path, \
                    w.name AS name, w.watch_set_at_commit AS commit, \
                    w.watch_set_at AS at \
             ORDER BY at DESC LIMIT 200";
    let t = match db.query(q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("list_watches failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text("_(nothing is watched)_".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("# Watches ({})\n\n", t.rows.len()));
    out.push_str("| labels | identifier | watched at commit | watched at |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for row in &t.rows {
        let lbls = row.first().and_then(|c| c.as_str()).unwrap_or("[]");
        let id = row
            .get(1)
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                row.get(2)
                    .and_then(|c| c.as_str())
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| row.get(3).and_then(|c| c.as_str()))
            .unwrap_or("?");
        let commit = row.get(4).and_then(|c| c.as_str()).unwrap_or("");
        let commit_short = if commit.len() > 8 {
            &commit[..8]
        } else {
            commit
        };
        let at = row.get(5).and_then(|c| c.as_str()).unwrap_or("");
        out.push_str(&format!(
            "| `{lbls}` | `{id}` | `{commit_short}` | {at} |\n"
        ));
    }
    ok_text(out.trim_end().to_string())
}
