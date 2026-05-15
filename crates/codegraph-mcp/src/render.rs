//! Markdown / TSV rendering primitives shared across handlers.
//!
//! Each function takes already-fetched data; nothing in here talks to the
//! `Db` directly except [`neighbour_degrees`], which is the one piece of
//! enrichment that's logically a render-side concern (degree-aware
//! sort + tag).

use codegraph_core::{escape_str, Cell, Db, Table};

// ── TSV ──────────────────────────────────────────────────────────────────────

pub fn format_cell(c: &Cell) -> String {
    match c {
        Cell::Null => "null".to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Integer(i) => i.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Text(s) => format!("{:?}", s),
        Cell::Json(s) => s.clone(),
    }
}

pub fn format_table(t: &Table) -> String {
    if t.columns.is_empty() && t.rows.is_empty() {
        return "(no results)".to_string();
    }
    if t.rows.is_empty() {
        return format!("(no rows; columns: {})", t.columns.join(", "));
    }
    let mut out = String::new();
    out.push_str(&t.columns.join("\t"));
    out.push('\n');
    for row in &t.rows {
        let cells: Vec<String> = row.iter().map(format_cell).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out.trim_end().to_string()
}

// ── Markdown ─────────────────────────────────────────────────────────────────

/// Escape a single cell for inclusion in a GFM table cell.
/// Pipes break columns; newlines break rows. Both must go.
pub fn md_cell(c: &Cell) -> String {
    let raw = match c {
        Cell::Null => "—".to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Integer(i) => i.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Json(s) => s.clone(),
    };
    raw.replace('|', "\\|").replace(['\n', '\r', '\t'], " ")
}

pub fn format_table_md(t: &Table) -> String {
    if t.columns.is_empty() && t.rows.is_empty() {
        return "_(no results)_".to_string();
    }
    if t.rows.is_empty() {
        return format!(
            "_(no rows; columns: {})_",
            t.columns
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut out = String::new();
    out.push_str("| ");
    out.push_str(&t.columns.join(" | "));
    out.push_str(" |\n");
    out.push('|');
    for _ in &t.columns {
        out.push_str(" --- |");
    }
    out.push('\n');
    for row in &t.rows {
        out.push_str("| ");
        let cells: Vec<String> = row.iter().map(md_cell).collect();
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
    }
    format!(
        "{out}\n_{} row{}_",
        t.rows.len(),
        if t.rows.len() == 1 { "" } else { "s" }
    )
}

// ── Note / neighbour rendering ───────────────────────────────────────────────

pub fn render_notes_rows(t: &Table) -> String {
    let mut out = String::new();
    let id_i = t.col("id");
    let title_i = t.col("title");
    let author_i = t.col("author");
    let created_i = t.col("created_at");
    let tags_i = t.col("tags");
    let md_i = t.col("markdown");
    for row in &t.rows {
        let id = id_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("?");
        let title = title_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let author = author_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let created = created_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let tags = tags_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let md = md_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let heading = if title.is_empty() { id } else { title };
        out.push_str(&format!("## {heading}\n\n"));
        out.push_str(&format!(
            "_id: `{id}` · author: `{author}` · created: `{created}`{}_\n\n",
            if tags.is_empty() {
                String::new()
            } else {
                format!(" · tags: `{tags}`")
            }
        ));
        out.push_str(md);
        if !md.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Best-effort total degree (in + out) for the given `qualified_name`s.
/// Returns an empty map if the aggregating query fails — the caller treats
/// missing entries as degree 0 and sorts them last, so a velr regression
/// just degrades to alphabetical ordering instead of erroring out.
pub fn neighbour_degrees(db: &Db, qns: &[String]) -> std::collections::HashMap<String, i64> {
    use std::collections::HashMap;
    let mut map: HashMap<String, i64> = HashMap::new();
    if qns.is_empty() {
        return map;
    }
    let in_list = qns
        .iter()
        .map(|s| escape_str(s))
        .collect::<Vec<_>>()
        .join(",");
    let q = format!(
        "MATCH (m) WHERE m.qualified_name IN [{in_list}] \
         OPTIONAL MATCH (m)-[r]-() \
         RETURN m.qualified_name AS qn, count(r) AS deg"
    );
    if let Ok(t) = db.query(&q) {
        for row in &t.rows {
            let qn = row
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let deg = row.get(1).and_then(|c| c.as_i64()).unwrap_or(0);
            if !qn.is_empty() {
                map.insert(qn, deg);
            }
        }
    }
    map
}

pub fn render_neighbours(db: &Db, query: &str, limit_per_rel: i64) -> String {
    let t = match db.query(query) {
        Ok(t) => t,
        Err(e) => return format!("_(query error: {e})_\n"),
    };
    if t.rows.is_empty() {
        return "_(none)_\n".to_string();
    }
    use std::collections::BTreeMap;
    // (lbls, identity, qn_for_degree_lookup)
    let mut groups: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    let rel_i = t.col("rel");
    let lbl_i = t.col("lbls");
    let qn_i = t.col("qn");
    let nm_i = t.col("nm");
    let pa_i = t.col("path");
    let mut degree_lookup_qns: Vec<String> = Vec::new();
    for row in &t.rows {
        let rel = rel_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("?")
            .to_string();
        let lbls = lbl_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("[]")
            .to_string();
        let qn = qn_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let identity = if !qn.is_empty() {
            qn.clone()
        } else {
            nm_i.and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .or_else(|| pa_i.and_then(|i| row.get(i)).and_then(|c| c.as_str()))
                .unwrap_or("?")
                .to_string()
        };
        if !qn.is_empty() {
            degree_lookup_qns.push(qn.clone());
        }
        groups.entry(rel).or_default().push((lbls, identity, qn));
    }

    let degrees = neighbour_degrees(db, &degree_lookup_qns);

    let mut out = String::new();
    for (rel, mut items) in groups {
        // Sort by degree desc, then by identity asc for stable output.
        items.sort_by(|a, b| {
            let da = degrees.get(&a.2).copied().unwrap_or(0);
            let db_ = degrees.get(&b.2).copied().unwrap_or(0);
            db_.cmp(&da).then_with(|| a.1.cmp(&b.1))
        });
        let total = items.len();
        let truncated = total > limit_per_rel as usize;
        items.truncate(limit_per_rel as usize);
        out.push_str(&format!(
            "- **`-[:{rel}]->`** ({total}{})\n",
            if truncated {
                format!(", showing top {limit_per_rel}")
            } else {
                String::new()
            }
        ));
        for (lbls, ident, qn) in items {
            let deg_tag = degrees
                .get(&qn)
                .filter(|d| **d > 0)
                .map(|d| format!(" _(deg {d})_"))
                .unwrap_or_default();
            out.push_str(&format!("  - `{lbls}` `{ident}`{deg_tag}\n"));
        }
    }
    out
}
