//! `:Note` MCP tools — `write_note`, `list_notes`. Notes survive
//! `--full` reindex and surface automatically inside `node_md`.

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::render::render_notes_rows;
use crate::util::{chrono_now_iso, err_text, ok_text};

pub fn handle_write_note(db: &Db, params: &Value) -> Value {
    let match_clause = match params.get("match").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return err_text("missing required argument: match".to_string()),
    };
    let markdown = match params.get("markdown").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return err_text("missing required argument: markdown".to_string()),
    };
    let title = params
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let author = params
        .get("author")
        .and_then(|v| v.as_str())
        .unwrap_or("claude")
        .to_string();
    let tags = params
        .get("tags")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Lightweight guard: the user-supplied MATCH must bind variable `t`.
    let lower = match_clause.to_lowercase();
    if !lower.contains("match") || !match_clause.contains('t') {
        return err_text("`match` must be a Cypher MATCH clause that binds variable `t`".into());
    }

    let now = chrono_now_iso();
    let note_id = format!("note-{}", now.replace([':', '.'], "-"));

    // Create the note node + attach via :NOTES edge to every target.
    let q = format!(
        "{match_clause} \
         MERGE (n:Note {{id: {id}}}) \
         SET n.title = {title}, n.author = {author}, n.tags = {tags}, \
             n.created_at = {created}, n.markdown = {md} \
         CREATE (n)-[:NOTES]->(t)",
        id = escape_str(&note_id),
        title = escape_str(&title),
        author = escape_str(&author),
        tags = escape_str(&tags),
        created = escape_str(&now),
        md = escape_str(&markdown),
    );
    if let Err(e) = db.run(&q) {
        return err_text(format!("note write failed: {e}"));
    }

    // Count how many targets got the note.
    let count_q = format!(
        "MATCH (n:Note {{id: {}}})-[:NOTES]->(x) RETURN count(x) AS c",
        escape_str(&note_id)
    );
    let attached = db
        .query(&count_q)
        .ok()
        .and_then(|t| t.rows.into_iter().next())
        .and_then(|r| r.into_iter().next())
        .and_then(|c| c.as_i64())
        .unwrap_or(0);

    if attached == 0 {
        // No target matched — clean up the orphan note so we don't accumulate junk.
        let _ = db.run(&format!(
            "MATCH (n:Note {{id: {}}}) DETACH DELETE n",
            escape_str(&note_id)
        ));
        return err_text(
            "MATCH bound no targets — note discarded. Verify your MATCH clause first with `cypher`.".into(),
        );
    }
    ok_text(format!(
        "wrote note `{note_id}` attached to {attached} target{}",
        if attached == 1 { "" } else { "s" }
    ))
}

pub fn handle_list_notes(db: &Db, params: &Value) -> Value {
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .max(1);
    let q = match params.get("match").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => format!(
            "{s} \
             MATCH (note:Note)-[:NOTES]->(t) \
             RETURN DISTINCT note.id AS id, note.title AS title, note.author AS author, \
                    note.created_at AS created_at, note.tags AS tags, note.markdown AS markdown \
             ORDER BY note.created_at DESC LIMIT {limit}"
        ),
        _ => format!(
            "MATCH (note:Note) \
             RETURN note.id AS id, note.title AS title, note.author AS author, \
                    note.created_at AS created_at, note.tags AS tags, note.markdown AS markdown \
             ORDER BY note.created_at DESC LIMIT {limit}"
        ),
    };
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("list_notes query failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text("_(no notes)_".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("# Notes ({})\n\n", t.rows.len()));
    out.push_str(&render_notes_rows(&t));
    ok_text(out.trim_end().to_string())
}
