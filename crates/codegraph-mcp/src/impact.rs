//! `impact` MCP tool — transitive blast-radius report for a node.
//! Walks `[:CALLS]` outwards (callees) and inwards (callers) via
//! app-side BFS, plus one-hop for `[:MENTIONS]` (`:DocSection`s) and
//! `[:IMPLEMENTED_BY]` (BDD `:Step`s).

use std::collections::BTreeSet;

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::util::{err_text, ok_text, parse_node_address_with_defaults};

/// One BFS hop along `rel`, expanding from a frontier of nodes identified by
/// `(label, key, value ∈ frontier)`. Returns the next frontier as
/// `(qualified_name, path)` pairs not already in `visited`.
fn bfs_hop(
    db: &Db,
    label: &str,
    key: &str,
    rel: &str,
    outgoing: bool,
    frontier: &[String],
    visited: &mut BTreeSet<String>,
) -> Vec<(String, String)> {
    if frontier.is_empty() {
        return Vec::new();
    }
    let in_list = frontier
        .iter()
        .map(|s| escape_str(s))
        .collect::<Vec<_>>()
        .join(",");
    let pattern = if outgoing {
        format!("(n:{label})-[:{rel}]->(m:{label})")
    } else {
        format!("(n:{label})<-[:{rel}]-(m:{label})")
    };
    let q = format!(
        "MATCH {pattern} \
         WHERE n.{key} IN [{in_list}] \
         RETURN DISTINCT m.{key} AS qn, m.path AS path"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for row in &t.rows {
        let qn = row
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        if qn.is_empty() {
            continue;
        }
        if visited.insert(qn.clone()) {
            let path = row
                .get(1)
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            out.push((qn, path));
        }
    }
    out
}

/// BFS along `rel` from the seed up to `max_depth`, returning
/// `(qualified_name, path, depth)` for every newly discovered node.
fn bfs_collect(
    db: &Db,
    label: &str,
    key: &str,
    seed: &str,
    rel: &str,
    outgoing: bool,
    max_depth: i64,
) -> Vec<(String, String, i64)> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(seed.to_string());
    let mut frontier = vec![seed.to_string()];
    let mut found = Vec::new();
    for d in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let next = bfs_hop(db, label, key, rel, outgoing, &frontier, &mut visited);
        let next_keys: Vec<String> = next.iter().map(|(qn, _)| qn.clone()).collect();
        for (qn, path) in next {
            found.push((qn, path, d));
        }
        frontier = next_keys;
    }
    found
}

fn render_impact_section(title: &str, items: &[(String, String, i64)], top: i64) -> String {
    let mut out = String::new();
    out.push_str(&format!("## {} ({})\n\n", title, items.len()));
    if items.is_empty() {
        out.push_str("_(none)_\n\n");
        return out;
    }
    let total = items.len();
    let shown = (top as usize).min(total);
    for (qn, path, depth) in items.iter().take(shown) {
        let path_part = if path.is_empty() {
            String::new()
        } else {
            format!(" — `{path}`")
        };
        out.push_str(&format!("- depth {depth}: `{qn}`{path_part}\n"));
    }
    if shown < total {
        out.push_str(&format!("- _… {} more_\n", total - shown));
    }
    out.push('\n');
    out
}

pub fn handle_impact(db: &Db, params: &Value) -> Value {
    let (label, key, value) =
        match parse_node_address_with_defaults(params, Some("Function"), Some("qualified_name")) {
            Ok(t) => t,
            Err(e) => return err_text(e),
        };
    let depth = params
        .get("depth")
        .and_then(|v| v.as_i64())
        .unwrap_or(3)
        .clamp(1, 6);
    let top = params
        .get("top")
        .and_then(|v| v.as_i64())
        .unwrap_or(15)
        .max(1);
    let val_lit = escape_str(&value);

    // Verify the seed exists (and grab its file).
    let seed_q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}}) \
         OPTIONAL MATCH (n)-[:DEFINED_IN]->(f:File) \
         RETURN f.path AS path LIMIT 1"
    );
    let (exists, def_file) = match db.query(&seed_q) {
        Ok(t) if !t.rows.is_empty() => (
            true,
            t.rows[0]
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        Ok(_) => (false, String::new()),
        Err(e) => return err_text(format!("seed lookup failed: {e}")),
    };
    if !exists {
        return ok_text(format!(
            "# Not found\n\nNo `:{label}` with `{key} = {value:?}`.\n"
        ));
    }

    let callees = bfs_collect(db, &label, &key, &value, "CALLS", true, depth);
    let callers = bfs_collect(db, &label, &key, &value, "CALLS", false, depth);

    // One-hop: doc sections that mention this node.
    let mentions_q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}})<-[:MENTIONS]-(s:DocSection) \
         RETURN s.qualified_name AS qn, s.path AS path"
    );
    let mut mentions: Vec<(String, String, i64)> = Vec::new();
    if let Ok(t) = db.query(&mentions_q) {
        for row in &t.rows {
            let qn = row
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let path = row
                .get(1)
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if !qn.is_empty() {
                mentions.push((qn, path, 1));
            }
        }
    }

    // One-hop: BDD steps implemented by this function.
    let steps_q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}})<-[:IMPLEMENTED_BY]-(st:Step) \
         RETURN st.qualified_name AS qn, st.text AS text"
    );
    let mut steps: Vec<(String, String, i64)> = Vec::new();
    if let Ok(t) = db.query(&steps_q) {
        for row in &t.rows {
            let qn = row
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let text = row
                .get(1)
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if !qn.is_empty() {
                steps.push((qn, text, 1));
            }
        }
    }

    let total_radius = callees.len() + callers.len() + mentions.len() + steps.len();
    let mut out = String::new();
    out.push_str(&format!("# Impact: `:{label} {{{key}: {value:?}}}`\n\n"));
    if !def_file.is_empty() {
        out.push_str(&format!("Defined in `{def_file}`. "));
    }
    out.push_str(&format!(
        "**Blast radius: {total_radius}** \
         (callers {}, callees {}, doc mentions {}, scenario steps {}).\n\n",
        callers.len(),
        callees.len(),
        mentions.len(),
        steps.len()
    ));
    out.push_str(&render_impact_section(
        &format!("Callers (transitive, depth ≤ {depth})"),
        &callers,
        top,
    ));
    out.push_str(&render_impact_section(
        &format!("Callees (transitive, depth ≤ {depth})"),
        &callees,
        top,
    ));
    out.push_str(&render_impact_section("Doc mentions", &mentions, top));
    out.push_str(&render_impact_section(
        "Scenario steps implemented",
        &steps,
        top,
    ));

    ok_text(out.trim_end().to_string())
}
