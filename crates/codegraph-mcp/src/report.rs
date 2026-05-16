//! `codegraph-mcp report --db <path> --out <dir>` — render the
//! graph-stored worklog into two Markdown files:
//!
//! * `<out>/ROADMAP.md` — current state, grouped by area + status,
//!   done items kept (not deleted) so progress is visible.
//! * `<out>/WORKLOG.md` — chronological log, every item with its full
//!   `:Status` timeline and `:Comment` threads.
//!
//! ARCHITECTURE.md stays handwritten — it does not change every commit
//! and is not graph-derived.

use std::collections::BTreeMap;
use std::path::Path;

use codegraph_core::{escape_str, Db};

pub fn run(db_path: &str, out_dir: &str) -> Result<(), String> {
    let db = Db::open(db_path).map_err(|e| format!("open {db_path}: {e}"))?;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("mkdir {out_dir}: {e}"))?;

    let roadmap = render_roadmap(&db);
    let worklog = render_worklog(&db);

    let rp = Path::new(out_dir).join("ROADMAP.md");
    let wp = Path::new(out_dir).join("WORKLOG.md");
    std::fs::write(&rp, roadmap).map_err(|e| format!("write {}: {e}", rp.display()))?;
    std::fs::write(&wp, worklog).map_err(|e| format!("write {}: {e}", wp.display()))?;

    println!("wrote {}", rp.display());
    println!("wrote {}", wp.display());
    Ok(())
}

pub fn render_roadmap(db: &Db) -> String {
    let mut out = String::new();
    out.push_str("# Roadmap\n\n");
    out.push_str("_Generated from the graph by `codegraph-mcp report`. Do not edit by hand._\n\n");

    let items = fetch_items(db);
    if items.is_empty() {
        out.push_str("_No worklog items yet._\n");
        return out;
    }

    // Group by area, then by status.
    let mut by_area: BTreeMap<String, Vec<&Item>> = BTreeMap::new();
    for it in &items {
        let key = if it.area.is_empty() {
            "_(no area)_".to_string()
        } else {
            it.area.clone()
        };
        by_area.entry(key).or_default().push(it);
    }

    for (area, group) in &by_area {
        out.push_str(&format!("## {area}\n\n"));
        let mut by_status: BTreeMap<&str, Vec<&Item>> = BTreeMap::new();
        for it in group {
            by_status.entry(it.status.as_str()).or_default().push(it);
        }
        // Stable status display order — open work first.
        for status in ["in_progress", "pending", "blocked", "done", "abandoned"] {
            if let Some(rows) = by_status.get(status) {
                out.push_str(&format!("### {status}\n\n"));
                for it in rows {
                    out.push_str(&format!(
                        "- [{}] {} **{}** `({})` — _since {}_  \n  `{}`\n",
                        checkbox(&it.status),
                        kind_tag(&it.kind),
                        it.title,
                        it.status,
                        it.status_at,
                        it.id,
                    ));
                }
                out.push('\n');
            }
        }
    }
    out
}

pub fn render_worklog(db: &Db) -> String {
    let mut out = String::new();
    out.push_str("# Worklog\n\n");
    out.push_str("_Generated from the graph by `codegraph-mcp report`. Append-only history._\n\n");

    let mut items = fetch_items(db);
    if items.is_empty() {
        out.push_str("_No worklog items yet._\n");
        return out;
    }
    // Newest first.
    items.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    for it in &items {
        out.push_str(&format!(
            "## {} {} {}\n",
            status_badge(&it.status),
            kind_tag(&it.kind),
            it.title
        ));
        out.push_str(&format!(
            "_id `{}` · area `{}` · created {}_\n\n",
            it.id,
            if it.area.is_empty() {
                "—"
            } else {
                it.area.as_str()
            },
            it.created_at
        ));

        let related = fetch_related(db, &it.id);
        if !related.is_empty() {
            out.push_str("**Related:** ");
            let parts: Vec<String> = related
                .iter()
                .map(|(r, l)| format!("`{r}` ({l})"))
                .collect();
            out.push_str(&parts.join(", "));
            out.push_str("\n\n");
        }

        let statuses = fetch_statuses(db, &it.id);
        for st in &statuses {
            out.push_str(&format!("### `{}` — {}\n\n", st.text, st.at));
            let comments = fetch_comments(db, &st.id);
            for c in &comments {
                out.push_str(&format!("> **{}** ({}):\n>\n", c.author, c.at));
                for line in c.body.lines() {
                    out.push_str(&format!("> {line}\n"));
                }
                out.push('\n');
            }
        }
        out.push_str("---\n\n");
    }
    out
}

fn checkbox(status: &str) -> &'static str {
    match status {
        "done" => "x",
        "abandoned" => "~",
        _ => " ",
    }
}

fn kind_tag(kind: &str) -> String {
    if kind.is_empty() {
        String::new()
    } else {
        format!("`[{}]`", kind)
    }
}

fn status_badge(status: &str) -> &'static str {
    match status {
        "done" => "[DONE]",
        "in_progress" => "[WIP]",
        "blocked" => "[BLOCKED]",
        "abandoned" => "[DROPPED]",
        _ => "[TODO]",
    }
}

#[derive(Debug)]
struct Item {
    id: String,
    title: String,
    area: String,
    kind: String,
    status: String,
    status_at: String,
    created_at: String,
}

fn fetch_items(db: &Db) -> Vec<Item> {
    let q = "MATCH (w:WorklogItem) \
             RETURN w.id AS id, w.title AS title, w.area AS area, w.kind AS kind, \
                    w.current_status AS status, w.current_status_at AS status_at, \
                    w.created_at AS created_at \
             ORDER BY w.current_status_at DESC";
    let t = match db.query(q) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    t.rows
        .into_iter()
        .map(|r| Item {
            id: r.first().and_then(|c| c.as_str()).unwrap_or("").to_string(),
            title: r.get(1).and_then(|c| c.as_str()).unwrap_or("").to_string(),
            area: r.get(2).and_then(|c| c.as_str()).unwrap_or("").to_string(),
            kind: r.get(3).and_then(|c| c.as_str()).unwrap_or("").to_string(),
            status: r.get(4).and_then(|c| c.as_str()).unwrap_or("").to_string(),
            status_at: r.get(5).and_then(|c| c.as_str()).unwrap_or("").to_string(),
            created_at: r.get(6).and_then(|c| c.as_str()).unwrap_or("").to_string(),
        })
        .collect()
}

fn fetch_related(db: &Db, id: &str) -> Vec<(String, String)> {
    let q = format!(
        "MATCH (w:WorklogItem {{id: {}}})-[:RELATES_TO]->(t) \
         RETURN coalesce(t.qualified_name, t.path, t.name, t.id) AS ref, labels(t) AS labels",
        escape_str(id)
    );
    db.query(&q)
        .ok()
        .map(|t| {
            t.rows
                .into_iter()
                .map(|r| {
                    (
                        r.first()
                            .and_then(|c| c.as_str())
                            .unwrap_or("?")
                            .to_string(),
                        r.get(1).and_then(|c| c.as_str()).unwrap_or("").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug)]
struct StatusRow {
    id: String,
    text: String,
    at: String,
}

fn fetch_statuses(db: &Db, id: &str) -> Vec<StatusRow> {
    let q = format!(
        "MATCH (w:WorklogItem {{id: {}}})-[:HAS_STATUS]->(s:Status) \
         RETURN s.id AS sid, s.text AS text, s.created_at AS at \
         ORDER BY s.created_at ASC",
        escape_str(id)
    );
    db.query(&q)
        .ok()
        .map(|t| {
            t.rows
                .into_iter()
                .map(|r| StatusRow {
                    id: r.first().and_then(|c| c.as_str()).unwrap_or("").to_string(),
                    text: r.get(1).and_then(|c| c.as_str()).unwrap_or("").to_string(),
                    at: r.get(2).and_then(|c| c.as_str()).unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug)]
struct CommentRow {
    body: String,
    author: String,
    at: String,
}

fn fetch_comments(db: &Db, status_id: &str) -> Vec<CommentRow> {
    let q = format!(
        "MATCH (s:Status {{id: {}}})-[:HAS_COMMENT]->(c:Comment) \
         RETURN c.body AS body, c.author AS author, c.created_at AS at \
         ORDER BY c.created_at ASC",
        escape_str(status_id)
    );
    db.query(&q)
        .ok()
        .map(|t| {
            t.rows
                .into_iter()
                .map(|r| CommentRow {
                    body: r.first().and_then(|c| c.as_str()).unwrap_or("").to_string(),
                    author: r.get(1).and_then(|c| c.as_str()).unwrap_or("").to_string(),
                    at: r.get(2).and_then(|c| c.as_str()).unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worklog::{handle_worklog_create, handle_worklog_set_status};
    use serde_json::{json, Value};

    fn id_of(v: &Value) -> String {
        v["content"][0]["text"]
            .as_str()
            .unwrap()
            .split('`')
            .nth(1)
            .unwrap()
            .to_string()
    }

    #[test]
    fn roadmap_groups_and_worklog_renders_timeline() {
        let db = Db::open_in_memory().unwrap();
        let a = id_of(&handle_worklog_create(
            &db,
            &json!({"title": "Add report subcommand", "area": "mcp",
                    "comment": "kickoff"}),
        ));
        handle_worklog_set_status(
            &db,
            &json!({"id": a, "status": "done", "comment": "shipped"}),
        );
        id_of(&handle_worklog_create(
            &db,
            &json!({"title": "Open thing", "area": "indexer"}),
        ));

        let rm = render_roadmap(&db);
        assert!(rm.contains("## mcp"), "{rm}");
        assert!(rm.contains("## indexer"), "{rm}");
        assert!(rm.contains("### done"), "{rm}");
        assert!(rm.contains("### pending"), "{rm}");
        assert!(rm.contains("Add report subcommand"), "{rm}");

        let wl = render_worklog(&db);
        assert!(wl.contains("Add report subcommand"), "{wl}");
        assert!(wl.contains("`pending`"), "{wl}");
        assert!(wl.contains("`done`"), "{wl}");
        assert!(wl.contains("kickoff"), "{wl}");
        assert!(wl.contains("shipped"), "{wl}");
    }
}
