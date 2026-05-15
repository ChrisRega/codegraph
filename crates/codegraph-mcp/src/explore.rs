//! `explore` MCP tool — token-budgeted graph exploration.
//!
//! BFS from a seed up to `max_depth`, score each candidate
//! (`degree + 4·has_notes + 2·has_mentions − 5·depth`), greedily fill
//! a Markdown report from highest-scoring downward until `char_budget`
//! is exhausted. Footer reports drops so the agent knows whether to
//! raise the budget or pivot.
//!
//! Replaces the multi-`node_md`-call pattern with one bounded call.

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::util::{err_text, ok_text, parse_node_address};

#[derive(Clone)]
struct ExploreCandidate {
    qn: String,
    label: String,
    depth: u32,
    /// Most recent edge type used to reach this candidate (for the rendered hint).
    via_rel: String,
    /// Whether the BFS entered this node via outgoing or incoming edges.
    via_outgoing: bool,
    /// Cached importance metrics.
    deg: i64,
    has_notes: bool,
    has_mentions: bool,
}

impl ExploreCandidate {
    fn score(&self) -> f64 {
        let depth_penalty = (self.depth as f64) * 5.0;
        let base = self.deg as f64;
        let notes = if self.has_notes { 4.0 } else { 0.0 };
        let mentions = if self.has_mentions { 2.0 } else { 0.0 };
        base + notes + mentions - depth_penalty
    }

    fn render_line(&self) -> String {
        let arrow = if self.via_outgoing {
            format!("-[:{}]->", self.via_rel)
        } else {
            format!("<-[:{}]-", self.via_rel)
        };
        let mut tags = Vec::new();
        if self.deg > 0 {
            tags.push(format!("deg {}", self.deg));
        }
        if self.has_notes {
            tags.push("has notes".to_string());
        }
        if self.has_mentions {
            tags.push("doc'd".to_string());
        }
        let tag = if tags.is_empty() {
            String::new()
        } else {
            format!(" _({})_", tags.join(", "))
        };
        format!(
            "- depth {} `{}` `{}` `{}`{tag}",
            self.depth, arrow, self.label, self.qn
        )
    }
}

/// Single-hop neighbours of `seed_qns`. The label constraint on the
/// matched far-side node is dropped so any neighbour type can surface.
fn explore_hop(
    db: &Db,
    seed_label: &str,
    seed_key: &str,
    seed_qns: &[String],
    outgoing: bool,
) -> Vec<(String, String, String)> {
    if seed_qns.is_empty() {
        return Vec::new();
    }
    let in_list = seed_qns
        .iter()
        .map(|s| escape_str(s))
        .collect::<Vec<_>>()
        .join(",");
    let pattern = if outgoing {
        format!("(n:{seed_label})-[r]->(m)")
    } else {
        format!("(n:{seed_label})<-[r]-(m)")
    };
    let q = format!(
        "MATCH {pattern} \
         WHERE n.{seed_key} IN [{in_list}] \
         RETURN DISTINCT type(r) AS rel, labels(m) AS lbls, m.qualified_name AS qn"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    t.rows
        .iter()
        .filter_map(|row| {
            let rel = row
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let lbls = row
                .get(1)
                .and_then(|c| c.as_str())
                .unwrap_or("[]")
                .to_string();
            let qn = row.get(2).and_then(|c| c.as_str())?.to_string();
            if qn.is_empty() {
                return None;
            }
            Some((rel, lbls, qn))
        })
        .collect()
}

pub fn handle_explore(db: &Db, params: &Value) -> Value {
    let (label, key, value) = match parse_node_address(params) {
        Ok(t) => t,
        Err(e) => return err_text(e),
    };
    let char_budget = params
        .get("char_budget")
        .and_then(|v| v.as_i64())
        .unwrap_or(8000)
        .max(500) as usize;
    let max_depth = params
        .get("max_depth")
        .and_then(|v| v.as_i64())
        .unwrap_or(2)
        .clamp(1, 4) as u32;

    let val_lit = escape_str(&value);

    let seed_q =
        format!("MATCH (n:{label} {{{key}: {val_lit}}}) RETURN properties(n) AS props LIMIT 1");
    let seed_props = match db.query(&seed_q) {
        Ok(t) if !t.rows.is_empty() => t.rows[0]
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
        Ok(_) => {
            return ok_text(format!(
                "# Not found\n\nNo `:{label}` with `{key} = {value:?}`.\n"
            ))
        }
        Err(e) => return err_text(format!("seed lookup failed: {e}")),
    };

    use std::collections::{BTreeMap, BTreeSet};
    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(value.clone());
    let mut frontier: Vec<String> = vec![value.clone()];
    let mut discovered: Vec<ExploreCandidate> = Vec::new();
    for d in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let mut next_qns: Vec<String> = Vec::new();
        for outgoing in [true, false] {
            let hop = explore_hop(db, &label, &key, &frontier, outgoing);
            for (rel, lbls, qn) in hop {
                if !visited.insert(qn.clone()) {
                    continue;
                }
                discovered.push(ExploreCandidate {
                    qn: qn.clone(),
                    label: lbls,
                    depth: d,
                    via_rel: rel,
                    via_outgoing: outgoing,
                    deg: 0,
                    has_notes: false,
                    has_mentions: false,
                });
                next_qns.push(qn);
            }
        }
        frontier = next_qns;
    }

    if !discovered.is_empty() {
        let qns: Vec<String> = discovered.iter().map(|c| c.qn.clone()).collect();
        let in_list = qns
            .iter()
            .map(|s| escape_str(s))
            .collect::<Vec<_>>()
            .join(",");

        let deg_q = format!(
            "MATCH (m) WHERE m.qualified_name IN [{in_list}] \
             OPTIONAL MATCH (m)-[r]-() \
             RETURN m.qualified_name AS qn, count(r) AS deg"
        );
        let mut deg_map: BTreeMap<String, i64> = BTreeMap::new();
        if let Ok(t) = db.query(&deg_q) {
            for row in &t.rows {
                if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                    let d = row.get(1).and_then(|c| c.as_i64()).unwrap_or(0);
                    deg_map.insert(qn.to_string(), d);
                }
            }
        }

        let notes_q = format!(
            "MATCH (m)<-[:NOTES]-(:Note) WHERE m.qualified_name IN [{in_list}] \
             RETURN DISTINCT m.qualified_name AS qn"
        );
        let mut notes_set: BTreeSet<String> = BTreeSet::new();
        if let Ok(t) = db.query(&notes_q) {
            for row in &t.rows {
                if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                    notes_set.insert(qn.to_string());
                }
            }
        }

        let men_q = format!(
            "MATCH (m)<-[:MENTIONS]-(:DocSection) WHERE m.qualified_name IN [{in_list}] \
             RETURN DISTINCT m.qualified_name AS qn"
        );
        let mut men_set: BTreeSet<String> = BTreeSet::new();
        if let Ok(t) = db.query(&men_q) {
            for row in &t.rows {
                if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                    men_set.insert(qn.to_string());
                }
            }
        }

        for c in &mut discovered {
            c.deg = deg_map.get(&c.qn).copied().unwrap_or(0);
            c.has_notes = notes_set.contains(&c.qn);
            c.has_mentions = men_set.contains(&c.qn);
        }
    }

    discovered.sort_by(|a, b| {
        b.score()
            .partial_cmp(&a.score())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = String::new();
    out.push_str(&format!(
        "# Explore: `:{label} {{{key}: {value:?}}}` (budget {char_budget} chars, depth ≤ {max_depth})\n\n"
    ));
    out.push_str("## Seed\n\n```json\n");
    out.push_str(&seed_props);
    if !seed_props.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n\n");
    out.push_str(&format!(
        "## Neighbourhood (ranked, BFS up to depth {max_depth})\n\n"
    ));

    let total = discovered.len();
    let mut included = 0usize;
    for c in &discovered {
        let line = c.render_line();
        let footer_reserve = 160;
        if out.len() + line.len() + footer_reserve >= char_budget {
            break;
        }
        out.push_str(&line);
        out.push('\n');
        included += 1;
    }
    if total == 0 {
        out.push_str("_(no neighbours within depth)_\n");
    }
    out.push('\n');
    let dropped = total.saturating_sub(included);
    let used = out.len();
    out.push_str(&format!(
        "_Showed {included}/{total} candidates · used ~{used}/{char_budget} chars · {dropped} dropped (raise `char_budget` or `max_depth` to see more)._"
    ));

    ok_text(out)
}
