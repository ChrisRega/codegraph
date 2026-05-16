//! Structured worklog stored in the graph itself. Schema:
//!
//! ```text
//!   (:WorklogItem {id, title, area, created_at,
//!                  current_status, current_status_at})
//!     -[:HAS_STATUS]->(:Status {text, created_at})
//!                       -[:HAS_COMMENT]->(:Comment {body, author, created_at})
//!   (:WorklogItem)-[:RELATES_TO]->(anything: Function|File|Concept|…)
//! ```
//!
//! `:Status` is append-only — every status change creates a new node, so
//! the timeline survives. `current_status` is denormalised onto the item
//! for cheap "what's open right now" queries; the linked :Status nodes
//! remain the source of truth for history. Each :Status can carry many
//! :Comment nodes (1:n).
//!
//! Tools: `worklog_create`, `worklog_set_status`, `worklog_comment`,
//! `worklog_list`, `worklog_md`. All write tools mutate user-derived
//! labels that survive the indexer's full wipe (see `run_indexer_inner`).

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::util::{chrono_now_iso, err_text, ok_text, safe_name_with_dashes};

const ALLOWED_STATUS: &[&str] = &["pending", "in_progress", "done", "blocked", "abandoned"];
const ALLOWED_KIND: &[&str] = &["bug", "feature", "task", "refactor", "perf", "docs"];

fn validate_status(s: &str) -> Result<(), String> {
    if ALLOWED_STATUS.contains(&s) {
        Ok(())
    } else {
        Err(format!(
            "invalid status `{s}`; expected one of: {}",
            ALLOWED_STATUS.join(", ")
        ))
    }
}

fn validate_kind(k: &str) -> Result<(), String> {
    if ALLOWED_KIND.contains(&k) {
        Ok(())
    } else {
        Err(format!(
            "invalid kind `{k}`; expected one of: {}",
            ALLOWED_KIND.join(", ")
        ))
    }
}

fn slug_from_title(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            last_dash = false;
        } else if !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 60 {
        out.truncate(60);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

fn make_id(title: &str, now: &str) -> String {
    let slug = slug_from_title(title);
    let stamp = now
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(14)
        .collect::<String>();
    if slug.is_empty() {
        format!("wl-{stamp}")
    } else {
        format!("wl-{slug}-{stamp}")
    }
}

pub fn handle_worklog_create(db: &Db, params: &Value) -> Value {
    let title = match params.get("title").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return err_text("missing required argument: title".to_string()),
    };
    let area = params
        .get("area")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let status = params
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("pending")
        .to_string();
    if let Err(e) = validate_status(&status) {
        return err_text(e);
    }
    let kind = params
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("task")
        .to_string();
    if let Err(e) = validate_kind(&kind) {
        return err_text(e);
    }
    let comment = params
        .get("comment")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let author = params
        .get("author")
        .and_then(|v| v.as_str())
        .unwrap_or("claude")
        .to_string();
    let id_in = params
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let match_clause = params
        .get("match")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let now = chrono_now_iso();
    let id = match id_in {
        Some(s) if !s.is_empty() => {
            if !safe_name_with_dashes(&s) {
                return err_text(format!("invalid id `{s}` (use letters/digits/_/-)"));
            }
            s
        }
        _ => make_id(&title, &now),
    };

    // Idempotency guard — refuse to clobber an existing item.
    if let Ok(t) = db.query(&format!(
        "MATCH (w:WorklogItem {{id: {}}}) RETURN w.id AS id",
        escape_str(&id)
    )) {
        if !t.rows.is_empty() {
            return err_text(format!(
                "worklog item `{id}` already exists; use worklog_set_status to update it"
            ));
        }
    }

    // CREATE item.
    let q = format!(
        "CREATE (w:WorklogItem {{id: {id}, title: {title}, area: {area}, kind: {kind}, \
         created_at: {now}, current_status: {status}, current_status_at: {now}}})",
        id = escape_str(&id),
        title = escape_str(&title),
        area = escape_str(&area),
        kind = escape_str(&kind),
        now = escape_str(&now),
        status = escape_str(&status),
    );
    if let Err(e) = db.run(&q) {
        return err_text(format!("worklog create failed: {e}"));
    }

    // Initial :Status node.
    let status_id = format!("{id}__s-{}", now.replace([':', '.'], "-"));
    let sq = format!(
        "MATCH (w:WorklogItem {{id: {id}}}) \
         CREATE (s:Status {{id: {sid}, text: {st}, created_at: {now}}}) \
         CREATE (w)-[:HAS_STATUS]->(s)",
        id = escape_str(&id),
        sid = escape_str(&status_id),
        st = escape_str(&status),
        now = escape_str(&now),
    );
    if let Err(e) = db.run(&sq) {
        return err_text(format!("worklog status create failed: {e}"));
    }

    // Optional comment on the initial status.
    if !comment.is_empty() {
        if let Err(e) = attach_comment(db, &status_id, &comment, &author, &now) {
            return err_text(e);
        }
    }

    // Optional :RELATES_TO edges.
    let mut related = 0usize;
    if !match_clause.is_empty() {
        let lower = match_clause.to_lowercase();
        if !lower.contains("match") || !match_clause.contains('t') {
            return err_text("`match` must be a Cypher MATCH that binds variable `t`".into());
        }
        let rq = format!(
            "{match_clause} \
             MATCH (w:WorklogItem {{id: {id}}}) \
             CREATE (w)-[:RELATES_TO]->(t)",
            id = escape_str(&id),
        );
        if let Err(e) = db.run(&rq) {
            return err_text(format!("RELATES_TO link failed: {e}"));
        }
        related = db
            .query(&format!(
                "MATCH (w:WorklogItem {{id: {}}})-[:RELATES_TO]->(x) RETURN count(x) AS c",
                escape_str(&id)
            ))
            .ok()
            .and_then(|t| t.rows.into_iter().next())
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_i64())
            .unwrap_or(0) as usize;
    }

    ok_text(format!(
        "created worklog `{id}` (kind `{kind}`, status `{status}`{rel})",
        rel = if related > 0 {
            format!(
                ", {related} related node{}",
                if related == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        }
    ))
}

pub fn handle_worklog_set_status(db: &Db, params: &Value) -> Value {
    let id = match params.get("id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err_text("missing required argument: id".to_string()),
    };
    let status = match params.get("status").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        _ => return err_text("missing required argument: status".to_string()),
    };
    if let Err(e) = validate_status(&status) {
        return err_text(e);
    }
    let comment = params
        .get("comment")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let author = params
        .get("author")
        .and_then(|v| v.as_str())
        .unwrap_or("claude")
        .to_string();

    // Existence guard.
    let exists = db
        .query(&format!(
            "MATCH (w:WorklogItem {{id: {}}}) RETURN w.id AS id",
            escape_str(&id)
        ))
        .map(|t| !t.rows.is_empty())
        .unwrap_or(false);
    if !exists {
        return err_text(format!("worklog item `{id}` not found"));
    }

    let now = chrono_now_iso();
    let status_id = format!("{id}__s-{}", now.replace([':', '.'], "-"));

    // Append new :Status, then update denormalised current_status on the item.
    let q = format!(
        "MATCH (w:WorklogItem {{id: {id}}}) \
         CREATE (s:Status {{id: {sid}, text: {st}, created_at: {now}}}) \
         CREATE (w)-[:HAS_STATUS]->(s) \
         SET w.current_status = {st}, w.current_status_at = {now}",
        id = escape_str(&id),
        sid = escape_str(&status_id),
        st = escape_str(&status),
        now = escape_str(&now),
    );
    if let Err(e) = db.run(&q) {
        return err_text(format!("set_status failed: {e}"));
    }

    if !comment.is_empty() {
        if let Err(e) = attach_comment(db, &status_id, &comment, &author, &now) {
            return err_text(e);
        }
    }

    ok_text(format!(
        "worklog `{id}` → status `{status}`{c}",
        c = if comment.is_empty() {
            String::new()
        } else {
            " (with comment)".to_string()
        }
    ))
}

pub fn handle_worklog_comment(db: &Db, params: &Value) -> Value {
    let id = match params.get("id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err_text("missing required argument: id".to_string()),
    };
    let body = match params.get("body").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err_text("missing required argument: body".to_string()),
    };
    let author = params
        .get("author")
        .and_then(|v| v.as_str())
        .unwrap_or("claude")
        .to_string();

    // Find the latest :Status node for this item (by created_at desc).
    let q = format!(
        "MATCH (w:WorklogItem {{id: {}}})-[:HAS_STATUS]->(s:Status) \
         RETURN s.id AS sid, s.created_at AS at \
         ORDER BY s.created_at DESC LIMIT 1",
        escape_str(&id)
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("worklog_comment lookup failed: {e}")),
    };
    let sid = match t.rows.into_iter().next() {
        Some(r) => r
            .into_iter()
            .next()
            .and_then(|c| c.as_str().map(str::to_string))
            .unwrap_or_default(),
        None => return err_text(format!("worklog item `{id}` has no statuses")),
    };
    if sid.is_empty() {
        return err_text(format!("worklog item `{id}` has no statuses"));
    }

    let now = chrono_now_iso();
    if let Err(e) = attach_comment(db, &sid, &body, &author, &now) {
        return err_text(e);
    }
    ok_text(format!("comment attached to `{sid}`"))
}

fn attach_comment(
    db: &Db,
    status_id: &str,
    body: &str,
    author: &str,
    now: &str,
) -> Result<(), String> {
    let cid = format!("{status_id}__c-{}", now.replace([':', '.'], "-"));
    let q = format!(
        "MATCH (s:Status {{id: {sid}}}) \
         CREATE (c:Comment {{id: {cid}, body: {body}, author: {author}, created_at: {now}}}) \
         CREATE (s)-[:HAS_COMMENT]->(c)",
        sid = escape_str(status_id),
        cid = escape_str(&cid),
        body = escape_str(body),
        author = escape_str(author),
        now = escape_str(now),
    );
    db.run(&q)
        .map_err(|e| format!("comment create failed: {e}"))?;
    Ok(())
}

pub fn handle_worklog_list(db: &Db, params: &Value) -> Value {
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(100)
        .max(1);
    let area_filter = params
        .get("area")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let status_filter = params
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind_filter = params
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !status_filter.is_empty() {
        if let Err(e) = validate_status(&status_filter) {
            return err_text(e);
        }
    }
    if !kind_filter.is_empty() {
        if let Err(e) = validate_kind(&kind_filter) {
            return err_text(e);
        }
    }

    let mut wheres: Vec<String> = Vec::new();
    if !area_filter.is_empty() {
        wheres.push(format!("w.area = {}", escape_str(&area_filter)));
    }
    if !status_filter.is_empty() {
        wheres.push(format!("w.current_status = {}", escape_str(&status_filter)));
    }
    if !kind_filter.is_empty() {
        wheres.push(format!("w.kind = {}", escape_str(&kind_filter)));
    }
    let where_sql = if wheres.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wheres.join(" AND "))
    };

    let q = format!(
        "MATCH (w:WorklogItem){where_sql} \
         RETURN w.id AS id, w.title AS title, w.area AS area, w.kind AS kind, \
                w.current_status AS status, w.current_status_at AS status_at, \
                w.created_at AS created_at \
         ORDER BY w.current_status_at DESC LIMIT {limit}"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("worklog_list failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text("_(no worklog items)_".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("# Worklog ({} items)\n\n", t.rows.len()));
    out.push_str("| status | kind | id | title | area | updated |\n");
    out.push_str("|--------|------|----|-------|------|---------|\n");
    for row in &t.rows {
        let id = row.first().and_then(|c| c.as_str()).unwrap_or("");
        let title = row.get(1).and_then(|c| c.as_str()).unwrap_or("");
        let area = row.get(2).and_then(|c| c.as_str()).unwrap_or("");
        let kind = row.get(3).and_then(|c| c.as_str()).unwrap_or("");
        let status = row.get(4).and_then(|c| c.as_str()).unwrap_or("");
        let at = row.get(5).and_then(|c| c.as_str()).unwrap_or("");
        out.push_str(&format!(
            "| `{status}` | `{kind}` | `{id}` | {title} | {area} | {at} |\n"
        ));
    }
    ok_text(out.trim_end().to_string())
}

pub fn handle_worklog_md(db: &Db, params: &Value) -> Value {
    let id = match params.get("id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err_text("missing required argument: id".to_string()),
    };

    let item_q = format!(
        "MATCH (w:WorklogItem {{id: {}}}) \
         RETURN w.title AS title, w.area AS area, w.kind AS kind, \
                w.created_at AS created_at, \
                w.current_status AS status, w.current_status_at AS status_at",
        escape_str(&id)
    );
    let it = match db.query(&item_q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("worklog_md item lookup failed: {e}")),
    };
    let row = match it.rows.into_iter().next() {
        Some(r) => r,
        None => return err_text(format!("worklog item `{id}` not found")),
    };
    let title = row
        .first()
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let area = row
        .get(1)
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let kind = row
        .get(2)
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let created_at = row
        .get(3)
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let status = row
        .get(4)
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let status_at = row
        .get(5)
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    let mut out = String::new();
    out.push_str(&format!("# {title}\n\n"));
    out.push_str(&format!("- **id**: `{id}`\n"));
    if !kind.is_empty() {
        out.push_str(&format!("- **kind**: `{kind}`\n"));
    }
    if !area.is_empty() {
        out.push_str(&format!("- **area**: {area}\n"));
    }
    out.push_str(&format!(
        "- **current status**: `{status}` (since {status_at})\n"
    ));
    out.push_str(&format!("- **created**: {created_at}\n\n"));

    // Related nodes — label is unknown, so just return their identity-ish props.
    let rel_q = format!(
        "MATCH (w:WorklogItem {{id: {}}})-[:RELATES_TO]->(t) \
         RETURN coalesce(t.qualified_name, t.path, t.name, t.id) AS ref, labels(t) AS labels \
         LIMIT 50",
        escape_str(&id)
    );
    if let Ok(rt) = db.query(&rel_q) {
        if !rt.rows.is_empty() {
            out.push_str("## Related\n\n");
            for r in &rt.rows {
                let refv = r.first().and_then(|c| c.as_str()).unwrap_or("?");
                let labels = r.get(1).and_then(|c| c.as_str()).unwrap_or("");
                out.push_str(&format!("- `{refv}` ({labels})\n"));
            }
            out.push('\n');
        }
    }

    // Status timeline + comments.
    let st_q = format!(
        "MATCH (w:WorklogItem {{id: {}}})-[:HAS_STATUS]->(s:Status) \
         RETURN s.id AS sid, s.text AS text, s.created_at AS at \
         ORDER BY s.created_at ASC",
        escape_str(&id)
    );
    let st = match db.query(&st_q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("worklog_md status lookup failed: {e}")),
    };
    out.push_str("## Timeline\n\n");
    for r in &st.rows {
        let sid = r.first().and_then(|c| c.as_str()).unwrap_or("");
        let text = r.get(1).and_then(|c| c.as_str()).unwrap_or("");
        let at = r.get(2).and_then(|c| c.as_str()).unwrap_or("");
        out.push_str(&format!("### `{text}` — {at}\n\n"));

        let cq = format!(
            "MATCH (s:Status {{id: {}}})-[:HAS_COMMENT]->(c:Comment) \
             RETURN c.body AS body, c.author AS author, c.created_at AS at \
             ORDER BY c.created_at ASC",
            escape_str(sid)
        );
        if let Ok(ct) = db.query(&cq) {
            for cr in &ct.rows {
                let body = cr.first().and_then(|c| c.as_str()).unwrap_or("");
                let author = cr.get(1).and_then(|c| c.as_str()).unwrap_or("");
                let cat = cr.get(2).and_then(|c| c.as_str()).unwrap_or("");
                out.push_str(&format!("> **{author}** ({cat}):\n>\n"));
                for line in body.lines() {
                    out.push_str(&format!("> {line}\n"));
                }
                out.push('\n');
            }
        }
    }
    ok_text(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn text(v: &Value) -> String {
        v.get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|x| x.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn is_err(v: &Value) -> bool {
        v.get("isError").and_then(|b| b.as_bool()).unwrap_or(false)
    }

    #[test]
    fn create_set_status_comment_flow() {
        let db = db();
        let r = handle_worklog_create(
            &db,
            &json!({"title": "Build worklog feature", "area": "mcp",
                    "comment": "kickoff — schema in graph"}),
        );
        assert!(!is_err(&r), "create errored: {}", text(&r));
        let created = text(&r);
        let id = created
            .split('`')
            .nth(1)
            .expect("id in backticks")
            .to_string();

        let r2 = handle_worklog_set_status(
            &db,
            &json!({"id": id, "status": "in_progress",
                    "comment": "wiring tools"}),
        );
        assert!(!is_err(&r2), "set_status errored: {}", text(&r2));

        let r3 = handle_worklog_comment(
            &db,
            &json!({"id": id, "body": "extra thought after the fact"}),
        );
        assert!(!is_err(&r3), "comment errored: {}", text(&r3));

        // Verify schema shape: 2 statuses, 3 comments total.
        let s = db
            .query(&format!(
                "MATCH (w:WorklogItem {{id: {}}})-[:HAS_STATUS]->(s:Status) RETURN count(s) AS n",
                escape_str(&id)
            ))
            .unwrap();
        assert_eq!(s.rows[0][0].as_i64().unwrap(), 2);

        let c = db
            .query(&format!(
                "MATCH (w:WorklogItem {{id: {}}})-[:HAS_STATUS]->(:Status)-[:HAS_COMMENT]->(c:Comment) \
                 RETURN count(c) AS n",
                escape_str(&id)
            ))
            .unwrap();
        assert_eq!(c.rows[0][0].as_i64().unwrap(), 3);

        // The latest-status comment goes on the in_progress status.
        let q = format!(
            "MATCH (w:WorklogItem {{id: {}}})-[:HAS_STATUS]->(s:Status)-[:HAS_COMMENT]->(c:Comment) \
             WHERE c.body = 'extra thought after the fact' RETURN s.text AS text",
            escape_str(&id)
        );
        let r = db.query(&q).unwrap();
        assert_eq!(r.rows[0][0].as_str().unwrap(), "in_progress");

        // current_status is denormalised on the item.
        let cs = db
            .query(&format!(
                "MATCH (w:WorklogItem {{id: {}}}) RETURN w.current_status AS s",
                escape_str(&id)
            ))
            .unwrap();
        assert_eq!(cs.rows[0][0].as_str().unwrap(), "in_progress");
    }

    #[test]
    fn list_filters_by_status_and_area() {
        let db = db();
        handle_worklog_create(&db, &json!({"title": "A", "area": "mcp"}));
        handle_worklog_create(
            &db,
            &json!({"title": "B", "area": "mcp",
                                            "status": "done"}),
        );
        handle_worklog_create(&db, &json!({"title": "C", "area": "indexer"}));

        let all = text(&handle_worklog_list(&db, &json!({})));
        assert!(all.contains("3 items"), "all: {all}");

        let mcp = text(&handle_worklog_list(&db, &json!({"area": "mcp"})));
        assert!(mcp.contains("2 items"), "mcp: {mcp}");

        let done = text(&handle_worklog_list(&db, &json!({"status": "done"})));
        assert!(done.contains("1 items"), "done: {done}");
    }

    #[test]
    fn invalid_status_rejected() {
        let db = db();
        let r = handle_worklog_create(&db, &json!({"title": "X", "status": "weird"}));
        assert!(is_err(&r));
        assert!(text(&r).contains("invalid status"));
    }

    #[test]
    fn kind_defaults_to_task_and_can_be_overridden_and_filtered() {
        let db = db();
        let r = handle_worklog_create(&db, &json!({"title": "default"}));
        assert!(text(&r).contains("kind `task`"), "{}", text(&r));

        handle_worklog_create(&db, &json!({"title": "a bug", "kind": "bug"}));
        handle_worklog_create(&db, &json!({"title": "a feat", "kind": "feature"}));
        handle_worklog_create(&db, &json!({"title": "a perf", "kind": "perf"}));

        let bad = handle_worklog_create(&db, &json!({"title": "no", "kind": "weird"}));
        assert!(is_err(&bad));
        assert!(text(&bad).contains("invalid kind"));

        let bugs = text(&handle_worklog_list(&db, &json!({"kind": "bug"})));
        assert!(bugs.contains("1 items"), "{bugs}");
        assert!(bugs.contains("a bug"), "{bugs}");
        assert!(!bugs.contains("a feat"), "{bugs}");
    }

    #[test]
    fn worklog_md_renders_timeline() {
        let db = db();
        let r = handle_worklog_create(&db, &json!({"title": "Doc thing", "comment": "starting"}));
        let id = text(&r).split('`').nth(1).unwrap().to_string();
        handle_worklog_set_status(
            &db,
            &json!({"id": id, "status": "done", "comment": "shipped"}),
        );
        let md = text(&handle_worklog_md(&db, &json!({"id": id})));
        assert!(md.contains("# Doc thing"), "{md}");
        assert!(md.contains("## Timeline"), "{md}");
        assert!(md.contains("`pending`"), "{md}");
        assert!(md.contains("`done`"), "{md}");
        assert!(md.contains("starting"), "{md}");
        assert!(md.contains("shipped"), "{md}");
    }
}
