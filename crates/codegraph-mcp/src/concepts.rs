//! `:Concept` MCP tools — `define_concept`, `concept`, `list_concepts`.
//!
//! User-curated subsystem labels: a `:Concept` collects an ad-hoc set of
//! nodes via `[:DESCRIBES]` edges, and the dossier rolls up direct
//! members + functions in scope + tests + notes into one Markdown
//! report. Survives `--full` reindex (the indexer's wipe set excludes
//! `:Concept`).

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::render::{md_cell, render_notes_rows};
use crate::util::{chrono_now_iso, err_text, ok_text, safe_name_with_dashes};

pub fn handle_define_concept(db: &Db, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) if safe_name_with_dashes(s) => s.to_string(),
        Some(s) => return err_text(format!("invalid concept name: {s:?}")),
        None => return err_text("missing required argument: name".to_string()),
    };
    let match_clause = match params.get("match").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return err_text("missing required argument: match".to_string()),
    };
    let description = params
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let lower = match_clause.to_lowercase();
    if !lower.contains("match") || !match_clause.contains('t') {
        return err_text("`match` must be a Cypher MATCH clause binding `t`".into());
    }
    let now = chrono_now_iso();
    let upsert = format!(
        "MERGE (c:Concept {{name: {n}}}) \
         SET c.description = {d}, c.updated_at = {now}, \
             c.created_at = coalesce(c.created_at, {now})",
        n = escape_str(&name),
        d = escape_str(&description),
        now = escape_str(&now),
    );
    if let Err(e) = db.run(&upsert) {
        return err_text(format!("concept upsert failed: {e}"));
    }
    let attach = format!(
        "{match_clause} \
         MATCH (c:Concept {{name: {n}}}) \
         MERGE (c)-[:DESCRIBES]->(t)",
        n = escape_str(&name),
    );
    if let Err(e) = db.run(&attach) {
        return err_text(format!("concept attach failed: {e}"));
    }
    // nx-12: also emit a direct `[:RELATES_TO]` edge so queries can navigate
    // concept → target in one hop without traversing `[:DESCRIBES]`. The
    // edge is generic on the target side (whatever label `t` carries), so a
    // typed query like `MATCH (c:Concept)-[:RELATES_TO]->(f:Function)` falls
    // out naturally on the consumer end. `[:DESCRIBES]` is kept for backward
    // compatibility with existing saved views / dossiers.
    let attach_relates = format!(
        "{match_clause} \
         MATCH (c:Concept {{name: {n}}}) \
         MERGE (c)-[:RELATES_TO]->(t)",
        n = escape_str(&name),
    );
    if let Err(e) = db.run(&attach_relates) {
        eprintln!("[concept] :RELATES_TO mirror failed (non-fatal): {e}");
    }
    let count_q = format!(
        "MATCH (:Concept {{name: {n}}})-[:DESCRIBES]->(t) RETURN count(t) AS c",
        n = escape_str(&name),
    );
    let attached = db
        .query(&count_q)
        .ok()
        .and_then(|t| t.rows.into_iter().next())
        .and_then(|r| r.into_iter().next())
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    ok_text(format!(
        "concept `{name}` now describes {attached} member{}",
        if attached == 1 { "" } else { "s" }
    ))
}

pub fn handle_concept(db: &Db, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) if safe_name_with_dashes(s) => s.to_string(),
        Some(s) => return err_text(format!("invalid concept name: {s:?}")),
        None => return err_text("missing required argument: name".to_string()),
    };
    let n_lit = escape_str(&name);

    let head_q = format!(
        "MATCH (c:Concept {{name: {n_lit}}}) \
         RETURN c.description AS d, c.created_at AS created LIMIT 1"
    );
    let (description, created) = match db.query(&head_q) {
        Ok(t) if !t.rows.is_empty() => {
            let r = &t.rows[0];
            let d = r.first().and_then(|c| c.as_str()).unwrap_or("").to_string();
            let cr = r.get(1).and_then(|c| c.as_str()).unwrap_or("").to_string();
            (d, cr)
        }
        Ok(_) => return ok_text(format!("# Not found\n\nNo `:Concept` named `{name}`.\n")),
        Err(e) => return err_text(format!("concept lookup failed: {e}")),
    };

    let mut out = String::new();
    out.push_str(&format!("# Concept `{name}`\n\n"));
    if !description.is_empty() {
        out.push_str(&format!("> {description}\n\n"));
    }
    if !created.is_empty() {
        out.push_str(&format!("_created: {created}_\n\n"));
    }

    // Direct members.
    let members_q = format!(
        "MATCH (:Concept {{name: {n_lit}}})-[:DESCRIBES]->(t) \
         RETURN labels(t) AS lbls, t.qualified_name AS qn, t.path AS path, t.name AS name \
         LIMIT 200"
    );
    out.push_str("## Members\n\n");
    match db.query(&members_q) {
        Ok(t) if !t.rows.is_empty() => {
            for row in &t.rows {
                let lbls = row
                    .first()
                    .and_then(|c| c.as_str())
                    .unwrap_or("[]")
                    .to_string();
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
                    .unwrap_or("?")
                    .to_string();
                out.push_str(&format!("- `{lbls}` `{id}`\n"));
            }
            out.push('\n');
        }
        _ => out.push_str("_(none)_\n\n"),
    }

    // Functions reachable from members: members are :Function directly,
    // OR members are :DocSection that MENTIONS a :Function.
    // velr's OR-becomes-UNION quirk again — split into two queries.
    use std::collections::BTreeSet;
    let mut function_qns: BTreeSet<String> = BTreeSet::new();
    let q_direct = format!(
        "MATCH (:Concept {{name: {n_lit}}})-[:DESCRIBES]->(f:Function) \
         RETURN f.qualified_name AS qn"
    );
    if let Ok(t) = db.query(&q_direct) {
        for row in &t.rows {
            if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                function_qns.insert(qn.to_string());
            }
        }
    }
    let q_via_doc = format!(
        "MATCH (:Concept {{name: {n_lit}}})-[:DESCRIBES]->(:DocSection)-[:MENTIONS]->(f:Function) \
         RETURN f.qualified_name AS qn"
    );
    if let Ok(t) = db.query(&q_via_doc) {
        for row in &t.rows {
            if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                function_qns.insert(qn.to_string());
            }
        }
    }

    out.push_str(&format!(
        "## Functions in scope ({})\n\n",
        function_qns.len()
    ));
    if function_qns.is_empty() {
        out.push_str("_(none)_\n\n");
    } else {
        for qn in function_qns.iter().take(50) {
            out.push_str(&format!("- `{qn}`\n"));
        }
        if function_qns.len() > 50 {
            out.push_str(&format!("- _… {} more_\n", function_qns.len() - 50));
        }
        out.push('\n');
    }

    // Tests covering those functions.
    if !function_qns.is_empty() {
        let in_list = function_qns
            .iter()
            .map(|s| escape_str(s))
            .collect::<Vec<_>>()
            .join(",");
        let tests_q = format!(
            "MATCH (t:Test)-[:TESTS]->(f:Function) WHERE f.qualified_name IN [{in_list}] \
             RETURN t.qualified_name AS test, f.qualified_name AS fn"
        );
        if let Ok(t) = db.query(&tests_q) {
            out.push_str(&format!("## Tests covering scope ({})\n\n", t.rows.len()));
            if t.rows.is_empty() {
                out.push_str("_(none)_\n\n");
            } else {
                for row in &t.rows {
                    let test = row
                        .first()
                        .and_then(|c| c.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let fn_ = row
                        .get(1)
                        .and_then(|c| c.as_str())
                        .unwrap_or("?")
                        .to_string();
                    out.push_str(&format!("- `{test}` → `{fn_}`\n"));
                }
                out.push('\n');
            }
        }

        // Notes on members or in-scope functions.
        let notes_q = format!(
            "MATCH (note:Note)-[:NOTES]->(f:Function) WHERE f.qualified_name IN [{in_list}] \
             RETURN note.title AS title, note.author AS author, note.created_at AS created_at, \
                    note.tags AS tags, note.markdown AS markdown \
             ORDER BY note.created_at DESC LIMIT 25"
        );
        if let Ok(t) = db.query(&notes_q) {
            if !t.rows.is_empty() {
                out.push_str(&format!(
                    "## Notes on functions in scope ({})\n\n",
                    t.rows.len()
                ));
                out.push_str(&render_notes_rows(&t));
            }
        }
    }

    ok_text(out.trim_end().to_string())
}

pub fn handle_list_concepts(db: &Db) -> Value {
    let q = "MATCH (c:Concept) \
             OPTIONAL MATCH (c)-[:DESCRIBES]->(t) \
             RETURN c.name AS name, c.description AS description, c.created_at AS created_at, \
                    count(t) AS members \
             ORDER BY c.name";
    let t = match db.query(q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("list_concepts failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text("_(no concepts defined)_".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("# Concepts ({})\n\n", t.rows.len()));
    out.push_str("| name | description | members | created_at |\n| --- | --- | --- | --- |\n");
    let n_i = t.col("name");
    let d_i = t.col("description");
    let c_i = t.col("created_at");
    let m_i = t.col("members");
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
        let m = m_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        out.push_str(&format!("| `{n}` | {d} | {m} | {c} |\n"));
    }
    ok_text(out.trim_end().to_string())
}
