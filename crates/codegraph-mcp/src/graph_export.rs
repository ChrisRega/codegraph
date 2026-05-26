//! `graph_export` MCP tool — render a node-centered subgraph as Mermaid
//! or Graphviz DOT. The output is a copy-pasteable diagram for chats,
//! PRs and notes; complements the per-node `node_md` dossier with a
//! visual neighbour map.
//!
//! Traversal is intentionally a thin layer over `explore_hop`-style
//! single-property BFS: at depth `d`, only nodes that share the seed's
//! label / key are followed further. That covers the common case
//! ("show my function's call neighbourhood" / "show what's attached to
//! this WorklogItem") cleanly without inventing a polymorphic identity
//! scheme. Heterogeneous deeper traversal is a follow-up.
//!
//! Safety caps: `depth` clamps to 1..=3, `max_nodes` to 5..=200. Both
//! protect the LLM context window from a hub explosion.

use std::collections::{BTreeMap, BTreeSet};

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::util::{err_text, ok_text, parse_node_address};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct NodeRef {
    /// Joined label string like `Function:Symbol`, taken verbatim from
    /// the Cypher `labels(n)` result so deduping survives label-set
    /// changes between runs.
    labels: String,
    /// Display id — the value of the seed key on the matched node, or
    /// the first non-empty of a small set of common identity props on
    /// neighbour nodes (qualified_name / id / path / name / hash).
    id: String,
}

#[derive(Debug, Clone)]
struct Edge {
    src: NodeRef,
    dst: NodeRef,
    rel: String,
    outgoing: bool,
}

/// Fetch one BFS hop. Returns the edges between the frontier (matched
/// on `seed_label` + `seed_key`) and any neighbour node, plus the
/// neighbour's display id.
fn hop(
    db: &Db,
    seed_label: &str,
    seed_key: &str,
    frontier: &[String],
    outgoing: bool,
) -> Vec<(String, String, NodeRef)> {
    if frontier.is_empty() {
        return Vec::new();
    }
    let in_list = frontier
        .iter()
        .map(|s| escape_str(s))
        .collect::<Vec<_>>()
        .join(",");
    let pattern = if outgoing {
        format!("(n:{seed_label})-[r]->(m)")
    } else {
        format!("(n:{seed_label})<-[r]-(m)")
    };
    // Pull a small bouquet of common identity properties so we can pick
    // a stable display id without knowing each node label's convention
    // up front. coalesce() at the renderer (Rust) side keeps the Cypher
    // simple and works around velr's thin function surface.
    let q = format!(
        "MATCH {pattern} \
         WHERE n.{seed_key} IN [{in_list}] \
         RETURN n.{seed_key} AS src_id, type(r) AS rel, labels(m) AS lbls, \
                m.qualified_name AS qn, m.id AS id, m.path AS path, \
                m.name AS name, m.hash AS hash"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let pick = |row: &[codegraph_core::Cell], cols: &[usize]| -> String {
        for &i in cols {
            if let Some(s) = row.get(i).and_then(|c| c.as_str()) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        String::new()
    };
    t.rows
        .iter()
        .filter_map(|row| {
            let src_id = row.first().and_then(|c| c.as_str())?.to_string();
            let rel = row
                .get(1)
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let lbls = row
                .get(2)
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let id = pick(row, &[3, 4, 5, 6, 7]);
            if id.is_empty() {
                return None;
            }
            Some((src_id, rel, NodeRef { labels: lbls, id }))
        })
        .collect()
}

/// Render the discovered nodes + edges as Mermaid `flowchart LR`.
fn render_mermaid(seed: &NodeRef, nodes: &BTreeMap<NodeRef, String>, edges: &[Edge]) -> String {
    let mut out = String::new();
    out.push_str("```mermaid\nflowchart LR\n");
    for (n, slug) in nodes {
        let label = format!("{}<br/>{}", first_label(&n.labels), short(&n.id));
        let class = if n == seed { ":::seed" } else { "" };
        out.push_str(&format!("    {slug}[\"{label}\"]{class}\n"));
    }
    out.push_str("    classDef seed fill:#fde68a,stroke:#b45309;\n");
    for e in edges {
        let s = nodes.get(&e.src).cloned().unwrap_or_default();
        let d = nodes.get(&e.dst).cloned().unwrap_or_default();
        if s.is_empty() || d.is_empty() {
            continue;
        }
        let (from, to) = if e.outgoing { (&s, &d) } else { (&d, &s) };
        out.push_str(&format!("    {from} -- \"{}\" --> {to}\n", e.rel));
    }
    out.push_str("```\n");
    out
}

/// Render the discovered nodes + edges as Graphviz DOT.
fn render_dot(seed: &NodeRef, nodes: &BTreeMap<NodeRef, String>, edges: &[Edge]) -> String {
    let mut out = String::new();
    out.push_str("```dot\ndigraph G {\n  rankdir=LR;\n  node [shape=box, style=rounded];\n");
    for (n, slug) in nodes {
        let label = format!("{}\\n{}", first_label(&n.labels), short(&n.id));
        let extra = if n == seed {
            ", style=\"rounded,filled\", fillcolor=\"#fde68a\""
        } else {
            ""
        };
        out.push_str(&format!("  {slug} [label=\"{label}\"{extra}];\n"));
    }
    for e in edges {
        let s = nodes.get(&e.src).cloned().unwrap_or_default();
        let d = nodes.get(&e.dst).cloned().unwrap_or_default();
        if s.is_empty() || d.is_empty() {
            continue;
        }
        let (from, to) = if e.outgoing { (&s, &d) } else { (&d, &s) };
        out.push_str(&format!("  {from} -> {to} [label=\"{}\"];\n", e.rel));
    }
    out.push_str("}\n```\n");
    out
}

/// Strip the `[Function, Symbol]` Cypher wrapping and return the first
/// label found, so a Mermaid box reads `Function` instead of `["Function"]`.
fn first_label(s: &str) -> &str {
    let trimmed = s.trim_matches(|c: char| c == '[' || c == ']' || c == ' ');
    trimmed
        .split([',', '"', '\''])
        .find(|p| !p.trim().is_empty())
        .map(|p| p.trim())
        .unwrap_or(trimmed)
}

/// Truncate long identifiers so Mermaid/DOT boxes stay readable.
fn short(id: &str) -> String {
    const MAX: usize = 40;
    if id.len() <= MAX {
        id.to_string()
    } else {
        format!("…{}", &id[id.len() - MAX + 1..])
    }
}

/// Render an integer slug for each node so the diagram syntax doesn't
/// have to grapple with special characters in qualified names.
fn build_slugs(nodes: &BTreeSet<NodeRef>) -> BTreeMap<NodeRef, String> {
    nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), format!("n{i}")))
        .collect()
}

pub fn handle_graph_export(db: &Db, params: &Value) -> Value {
    let (seed_label, seed_key, seed_value) = match parse_node_address(params) {
        Ok(t) => t,
        Err(e) => return err_text(e),
    };
    let depth = params
        .get("depth")
        .and_then(|v| v.as_i64())
        .unwrap_or(1)
        .clamp(1, 3) as u32;
    let max_nodes = params
        .get("max_nodes")
        .and_then(|v| v.as_i64())
        .unwrap_or(60)
        .clamp(5, 200) as usize;
    let format = params
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("mermaid")
        .to_lowercase();
    if format != "mermaid" && format != "dot" {
        return err_text(format!(
            "invalid format `{format}` — expected `mermaid` or `dot`"
        ));
    }

    // Confirm the seed exists. Empty seed is a clearer error than an
    // empty diagram.
    let val_lit = escape_str(&seed_value);
    let seed_q = format!(
        "MATCH (n:{seed_label} {{{seed_key}: {val_lit}}}) RETURN labels(n) AS lbls LIMIT 1"
    );
    let seed_lbls = match db.query(&seed_q) {
        Ok(t) if !t.rows.is_empty() => t.rows[0]
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
        Ok(_) => {
            return ok_text(format!(
                "_No `:{seed_label}` with `{seed_key} = {seed_value:?}` — nothing to draw._"
            ))
        }
        Err(e) => return err_text(format!("seed lookup failed: {e}")),
    };

    let seed_node = NodeRef {
        labels: seed_lbls.clone(),
        id: seed_value.clone(),
    };

    let mut all_nodes: BTreeSet<NodeRef> = BTreeSet::from([seed_node.clone()]);
    let mut all_edges: Vec<Edge> = Vec::new();
    // Frontier: ids that still match the seed label/key and can be
    // expanded one more hop. Always includes the seed value at depth 0.
    let mut frontier: Vec<String> = vec![seed_value.clone()];
    let mut visited_ids: BTreeSet<String> = BTreeSet::from([seed_value.clone()]);
    let mut truncated = false;

    'depth_loop: for _ in 0..depth {
        let mut next_frontier: Vec<String> = Vec::new();
        for outgoing in [true, false] {
            let edges = hop(db, &seed_label, &seed_key, &frontier, outgoing);
            for (src_id, rel, dst) in edges {
                let src = NodeRef {
                    labels: seed_lbls.clone(),
                    id: src_id,
                };
                if all_nodes.insert(src.clone()) && all_nodes.len() > max_nodes {
                    truncated = true;
                    break 'depth_loop;
                }
                if all_nodes.insert(dst.clone()) && all_nodes.len() > max_nodes {
                    truncated = true;
                    break 'depth_loop;
                }
                // If the neighbour also matches the seed label, it can
                // continue the BFS next round.
                if first_label(&dst.labels) == seed_label && visited_ids.insert(dst.id.clone()) {
                    next_frontier.push(dst.id.clone());
                }
                all_edges.push(Edge {
                    src,
                    dst,
                    rel,
                    outgoing,
                });
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    let slugs = build_slugs(&all_nodes);
    let mut out = String::new();
    out.push_str(&format!(
        "# graph_export `{seed_label}({seed_key}={seed_value})` depth={depth}\n\n"
    ));
    out.push_str(&format!(
        "- nodes: **{}**, edges: **{}**, format: `{format}`\n",
        all_nodes.len(),
        all_edges.len()
    ));
    if truncated {
        out.push_str(&format!(
            "- ⚠ truncated at `max_nodes = {max_nodes}` — pass a higher cap or smaller `depth`\n"
        ));
    }
    out.push('\n');
    out.push_str(&match format.as_str() {
        "dot" => render_dot(&seed_node, &slugs, &all_edges),
        _ => render_mermaid(&seed_node, &slugs, &all_edges),
    });
    ok_text(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(v: &Value) -> String {
        v["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn seed_db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (a:Function {qualified_name: 'crate::a', name: 'a'})")
            .unwrap();
        db.run("CREATE (b:Function {qualified_name: 'crate::b', name: 'b'})")
            .unwrap();
        db.run("CREATE (c:Function {qualified_name: 'crate::c', name: 'c'})")
            .unwrap();
        db.run("MATCH (a:Function {qualified_name: 'crate::a'}), (b:Function {qualified_name: 'crate::b'}) MERGE (a)-[:CALLS]->(b)").unwrap();
        db.run("MATCH (b:Function {qualified_name: 'crate::b'}), (c:Function {qualified_name: 'crate::c'}) MERGE (b)-[:CALLS]->(c)").unwrap();
        db
    }

    #[test]
    fn missing_seed_returns_friendly_message() {
        let db = seed_db();
        let v = handle_graph_export(
            &db,
            &json!({
                "label": "Function",
                "key": "qualified_name",
                "value": "crate::ghost",
            }),
        );
        let md = text(&v);
        assert!(md.contains("nothing to draw"), "{md}");
    }

    #[test]
    fn mermaid_depth_one_renders_seed_and_neighbours() {
        let db = seed_db();
        let v = handle_graph_export(
            &db,
            &json!({
                "label": "Function",
                "key": "qualified_name",
                "value": "crate::a",
                "depth": 1,
            }),
        );
        let md = text(&v);
        assert!(md.contains("```mermaid"), "no mermaid fence: {md}");
        assert!(md.contains("flowchart LR"), "{md}");
        assert!(md.contains("crate::a"), "{md}");
        assert!(md.contains("crate::b"), "{md}");
        assert!(md.contains("CALLS"), "{md}");
        // depth 1 must NOT reach crate::c
        assert!(!md.contains("crate::c"), "depth leaked: {md}");
        // seed gets the styling class
        assert!(md.contains("classDef seed"), "{md}");
    }

    #[test]
    fn mermaid_depth_two_walks_one_more_hop() {
        let db = seed_db();
        let v = handle_graph_export(
            &db,
            &json!({
                "label": "Function",
                "key": "qualified_name",
                "value": "crate::a",
                "depth": 2,
            }),
        );
        let md = text(&v);
        assert!(md.contains("crate::c"), "depth 2 should reach c: {md}");
    }

    #[test]
    fn dot_format_emits_digraph() {
        let db = seed_db();
        let v = handle_graph_export(
            &db,
            &json!({
                "label": "Function",
                "key": "qualified_name",
                "value": "crate::a",
                "format": "dot",
            }),
        );
        let md = text(&v);
        assert!(md.contains("```dot"), "{md}");
        assert!(md.contains("digraph G"), "{md}");
        assert!(md.contains("rankdir=LR"), "{md}");
        assert!(md.contains("CALLS"), "{md}");
    }

    #[test]
    fn invalid_format_rejected() {
        let db = seed_db();
        let v = handle_graph_export(
            &db,
            &json!({
                "label": "Function",
                "key": "qualified_name",
                "value": "crate::a",
                "format": "svg",
            }),
        );
        let md = text(&v);
        assert!(md.contains("invalid format"), "{md}");
    }
}
