//! `diff_since` MCP tool — what landed between a baseline `:GitCommit`
//! and HEAD.
//!
//! Lists commits in the open-closed interval `(baseline, HEAD]` and the
//! `:File` / `:Function` nodes whose `first_seen_commit` lands inside.
//! Removals aren't tracked because the indexer doesn't keep tombstones —
//! the output footer makes this explicit so the agent doesn't infer
//! "no removals" from an absent section.

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::render::md_cell;
use crate::util::{err_text, ok_text};

pub fn handle_diff_since(db: &Db, params: &Value) -> Value {
    let given = match params.get("commit").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return err_text("missing required argument: commit".to_string()),
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .max(1);

    let g_lit = escape_str(&given);
    // velr's planner turns `WHERE x = a OR y = a` into a UNION which clashes
    // with LIMIT placement, so try the two keys separately.
    let try_lookup = |key: &str| -> Option<(String, String, String)> {
        let q = format!(
            "MATCH (c:GitCommit) WHERE c.{key} = {g_lit} \
             RETURN c.hash AS hash, c.short_hash AS short, c.timestamp AS ts LIMIT 1"
        );
        let t = db.query(&q).ok()?;
        let r = t.rows.into_iter().next()?;
        let mut it = r.into_iter();
        let h = it.next().and_then(|c| c.as_str().map(str::to_string))?;
        let s = it
            .next()
            .and_then(|c| c.as_str().map(str::to_string))
            .unwrap_or_default();
        let ts = it
            .next()
            .and_then(|c| c.as_str().map(str::to_string))
            .unwrap_or_default();
        Some((h, s, ts))
    };
    let (given_hash, given_short, given_ts) =
        match try_lookup("hash").or_else(|| try_lookup("short_hash")) {
            Some(t) => t,
            None => return ok_text(format!("_(no `:GitCommit` matches `{given}`)_")),
        };

    let head_q = "MATCH (h:GitCommit)-[:SNAPSHOT_OF]->(:Workspace) \
                  RETURN h.hash AS hash, h.short_hash AS short, h.timestamp AS ts LIMIT 1";
    let (head_hash, head_short, head_ts) = match db.query(head_q) {
        Ok(t) if !t.rows.is_empty() => {
            let r = &t.rows[0];
            let h = r.first().and_then(|c| c.as_str()).unwrap_or("").to_string();
            let s = r.get(1).and_then(|c| c.as_str()).unwrap_or("").to_string();
            let ts = r.get(2).and_then(|c| c.as_str()).unwrap_or("").to_string();
            (h, s, ts)
        }
        _ => {
            return err_text(
                "no HEAD `:GitCommit` (no `[:SNAPSHOT_OF]->(:Workspace)` edge) — was the indexer ever run?".to_string(),
            )
        }
    };

    let gt_lit = escape_str(&given_ts);
    let ht_lit = escape_str(&head_ts);

    // Commits strictly newer than the baseline up to and including HEAD.
    let range_q = format!(
        "MATCH (c:GitCommit) WHERE c.timestamp > {gt_lit} AND c.timestamp <= {ht_lit} \
         OPTIONAL MATCH (a:Author)-[:AUTHORED]->(c) \
         RETURN c.hash AS hash, c.short_hash AS short, c.timestamp AS ts, \
                a.name AS author, c.message AS msg \
         ORDER BY c.timestamp"
    );
    let range = match db.query(&range_q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("range query failed: {e}")),
    };
    let range_hashes: Vec<String> = range
        .rows
        .iter()
        .filter_map(|r| r.first().and_then(|c| c.as_str()).map(|s| s.to_string()))
        .collect();

    let mut out = String::new();
    out.push_str(&format!(
        "# Diff since `{given_short}` → HEAD `{head_short}`\n\n"
    ));
    out.push_str(&format!(
        "_Baseline `{given_hash}` ({given_ts})_  \n_HEAD `{head_hash}` ({head_ts})_\n\n"
    ));

    if range_hashes.is_empty() {
        out.push_str("No commits between baseline and HEAD.\n");
        return ok_text(out.trim_end().to_string());
    }

    out.push_str(&format!("## Commits in range ({})\n\n", range_hashes.len()));
    out.push_str("| short | timestamp | author | message |\n| --- | --- | --- | --- |\n");
    let s_i = range.col("short");
    let ts_i = range.col("ts");
    let a_i = range.col("author");
    let m_i = range.col("msg");
    for row in range.rows.iter().take(limit as usize) {
        let s = s_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let ts = ts_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let a = a_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let m = m_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        out.push_str(&format!("| `{s}` | {ts} | {a} | {m} |\n"));
    }
    if range_hashes.len() > limit as usize {
        out.push_str(&format!(
            "_… {} more (raise `limit`)_\n",
            range_hashes.len() - limit as usize
        ));
    }
    out.push('\n');

    let in_list = range_hashes
        .iter()
        .map(|h| escape_str(h))
        .collect::<Vec<_>>()
        .join(",");

    let added_section = |label: &str, key: &str, alias: &str| -> String {
        let q = format!(
            "MATCH (n:{label}) WHERE n.first_seen_commit IN [{in_list}] \
             RETURN n.{key} AS {alias}, n.first_seen_commit AS first \
             ORDER BY n.{key} LIMIT {limit_x}",
            limit_x = limit + 1
        );
        let mut s = String::new();
        match db.query(&q) {
            Ok(t) => {
                if t.rows.is_empty() {
                    s.push_str(&format!("## Added `:{label}` (0)\n\n_(none)_\n\n"));
                } else {
                    let total = t.rows.len();
                    let truncated = total > limit as usize;
                    let shown = (limit as usize).min(total);
                    s.push_str(&format!(
                        "## Added `:{label}` ({}{})\n\n",
                        if truncated { ">=" } else { "" },
                        total
                    ));
                    s.push_str("| identifier | first_seen_commit |\n| --- | --- |\n");
                    for row in t.rows.iter().take(shown) {
                        let id = row
                            .first()
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        let f = row
                            .get(1)
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        let f_short = if f.len() > 8 { &f[..8] } else { &f };
                        s.push_str(&format!("| `{id}` | `{f_short}` |\n"));
                    }
                    if truncated {
                        s.push_str("_… more (raise `limit`)_\n");
                    }
                    s.push('\n');
                }
            }
            Err(e) => {
                s.push_str(&format!("## Added `:{label}`\n\n_(query failed: {e})_\n\n"));
            }
        }
        s
    };

    out.push_str(&added_section("File", "path", "id"));
    out.push_str(&added_section("Function", "qualified_name", "id"));

    out.push_str(
        "> Removals are **not** listed: the indexer detaches deleted nodes \
         on each pass and does not keep tombstones. To detect a removal, \
         compare two snapshots externally (e.g. `git log -S<symbol>`).\n",
    );

    ok_text(out.trim_end().to_string())
}
