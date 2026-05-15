//! `import_pr_notes` MCP tool — bulk-imports PR / code-review comments
//! as `:Note` nodes attached to any `:Function` they reference.

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::util::{chrono_now_iso, err_text, ok_text};

/// Extract backtick-delimited tokens from `body`. Tokens longer than 120
/// chars (almost certainly fenced code blocks) and tokens that don't look
/// like identifiers are dropped. Handles ```…``` blocks by skipping their
/// contents entirely.
///
/// `pub(crate)` so the existing test in `main.rs::tests` can drive it.
pub(crate) fn extract_backticked_symbols(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        // Triple backtick ⇒ skip to the matching closer.
        if bytes.get(i + 1) == Some(&b'`') && bytes.get(i + 2) == Some(&b'`') {
            if let Some(rel_end) = body[i + 3..].find("```") {
                i = i + 3 + rel_end + 3;
            } else {
                break;
            }
            continue;
        }
        // Single backtick ⇒ find next single backtick.
        if let Some(rel_end) = body[i + 1..].find('`') {
            let raw = &body[i + 1..i + 1 + rel_end];
            // Strip a trailing `()` so `foo()` becomes `foo` before validation.
            let token = raw.trim_end_matches("()");
            if !token.is_empty()
                && token.len() <= 120
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '.')
            {
                out.push(token.to_string());
            }
            i = i + 1 + rel_end + 1;
        } else {
            break;
        }
    }
    out
}

fn lookup_function_targets(db: &Db, symbol: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut hits: BTreeSet<String> = BTreeSet::new();
    let s_lit = escape_str(symbol);
    for key in ["name", "qualified_name"] {
        let q = format!(
            "MATCH (f:Function) WHERE f.{key} = {s_lit} \
             RETURN f.qualified_name AS qn LIMIT 10"
        );
        if let Ok(t) = db.query(&q) {
            for row in &t.rows {
                if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                    hits.insert(qn.to_string());
                }
            }
        }
    }
    hits.into_iter().collect()
}

pub fn handle_import_pr_notes(db: &Db, params: &Value) -> Value {
    let pr = params
        .get("pr")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let comments = match params.get("comments").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return err_text("missing required argument: comments (array)".into()),
    };

    let mut comments_processed = 0usize;
    let mut notes_created = 0usize;
    let mut total_attached = 0usize;
    let mut symbols_seen = 0usize;
    let now_base = chrono_now_iso();

    for (idx, c) in comments.iter().enumerate() {
        let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let author = c.get("author").and_then(|v| v.as_str()).unwrap_or("github");
        let url = c.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if body.trim().is_empty() {
            continue;
        }
        comments_processed += 1;
        let symbols = extract_backticked_symbols(body);
        if symbols.is_empty() {
            continue;
        }
        symbols_seen += symbols.len();
        use std::collections::BTreeSet;
        let mut targets: BTreeSet<String> = BTreeSet::new();
        for s in &symbols {
            for qn in lookup_function_targets(db, s) {
                targets.insert(qn);
            }
        }
        if targets.is_empty() {
            continue;
        }

        let note_id = format!(
            "pr-{}-{}-{}",
            pr.replace(['/', '#', ' '], "_"),
            idx,
            now_base.replace([':', '.'], "-")
        );
        let title = format!("PR {pr} — {author}");
        let md = if url.is_empty() {
            body.to_string()
        } else {
            format!("{body}\n\n[source]({url})")
        };
        let upsert = format!(
            "MERGE (n:Note {{id: {id}}}) \
             SET n.title = {title}, n.author = {author}, n.tags = 'pr-comment', \
                 n.created_at = {now}, n.markdown = {md}",
            id = escape_str(&note_id),
            title = escape_str(&title),
            author = escape_str(author),
            now = escape_str(&now_base),
            md = escape_str(&md),
        );
        if db.run(&upsert).is_err() {
            continue;
        }
        notes_created += 1;
        for qn in &targets {
            let q = format!(
                "MATCH (n:Note {{id: {id}}}), (t:Function {{qualified_name: {qn}}}) \
                 MERGE (n)-[:NOTES]->(t)",
                id = escape_str(&note_id),
                qn = escape_str(qn),
            );
            if db.run(&q).is_ok() {
                total_attached += 1;
            }
        }
    }

    ok_text(format!(
        "Processed {comments_processed} comments, scanned {symbols_seen} backticked tokens, \
         created {notes_created} notes attached to {total_attached} `:Function` targets."
    ))
}
