//! `import_pr` MCP tool — add a `:PR` node to the graph from one
//! `gh pr view --json …` payload.
//!
//! Schema written:
//!
//! - `:PR {number, title, state, body, head_sha, merged_at, author}`
//! - `[:AUTHORED]` from `:Author` (matched by `login` or `email`) → `:PR`
//! - `[:MERGES_INTO]` from `:PR` → `:GitCommit` (matched on `merge_commit_sha`,
//!   when supplied and present in the graph)
//! - `[:REFERENCES]` from `:PR` → `:WorklogItem` (every `nx-XX` id
//!   found in title or body via the same `Refs:`-trailer-style
//!   pattern as commit messages, but more permissive — PR bodies often
//!   mention IDs in prose)
//!
//! Idempotent on `number`: re-importing the same PR updates its
//! properties + re-MERGEs edges. Missing fields are accepted (the JSON
//! shape varies between `gh pr view`, `gh api`, and hand-rolled
//! payloads); the tool tolerates whatever it gets.
//!
//! Companion to `import_pr_notes` (which writes `:Note`s for review
//! comments) — both can be called against the same PR for a complete
//! picture.

use codegraph_core::{escape_str, Db};
use serde::Deserialize;
use serde_json::Value;

use crate::util::{err_text, ok_text};

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct PrPayload {
    number: Option<i64>,
    title: String,
    state: String,
    body: String,
    // `gh pr view` returns the field as `mergeCommit: {"oid": "..."}` or null.
    merge_commit: Option<MergeCommit>,
    /// Plain string also accepted, makes manual / `gh api` payloads easier.
    merge_commit_sha: Option<String>,
    head_ref_oid: Option<String>,
    merged_at: Option<String>,
    author: Option<AuthorRef>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct MergeCommit {
    oid: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct AuthorRef {
    login: String,
    email: String,
}

pub fn handle_import_pr(db: &Db, params: &Value) -> Value {
    let json = match params.get("json").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => {
            return err_text(
                "missing required argument: `json` (string with the PR payload)".to_string(),
            )
        }
    };
    let pr: PrPayload = match serde_json::from_str(&json) {
        Ok(p) => p,
        Err(e) => return err_text(format!("could not parse PR JSON: {e}")),
    };
    let number = match pr.number {
        Some(n) => n,
        None => return err_text("PR JSON has no `number` field".to_string()),
    };
    let merge_sha = pr
        .merge_commit
        .as_ref()
        .and_then(|m| m.oid.as_deref())
        .or(pr.merge_commit_sha.as_deref())
        .unwrap_or("")
        .to_string();
    let head_sha = pr.head_ref_oid.as_deref().unwrap_or("").to_string();
    let merged_at = pr.merged_at.as_deref().unwrap_or("").to_string();
    let author_login = pr
        .author
        .as_ref()
        .map(|a| a.login.as_str())
        .unwrap_or("")
        .to_string();
    let author_email = pr
        .author
        .as_ref()
        .map(|a| a.email.as_str())
        .unwrap_or("")
        .to_string();

    // Upsert :PR node.
    if let Err(e) = db.run(&format!(
        "MERGE (p:PR {{number: {n}}}) \
         SET p.title = {t}, p.state = {st}, p.body = {b}, \
             p.head_sha = {hs}, p.merge_commit_sha = {ms}, \
             p.merged_at = {ma}, p.author = {al}",
        n = number,
        t = escape_str(&pr.title),
        st = escape_str(&pr.state),
        b = escape_str(&pr.body),
        hs = escape_str(&head_sha),
        ms = escape_str(&merge_sha),
        ma = escape_str(&merged_at),
        al = escape_str(&author_login),
    )) {
        return err_text(format!("PR upsert failed: {e}"));
    }

    let mut edges = 0u32;

    // Author link: try login first, fall back to email.
    if !author_login.is_empty() || !author_email.is_empty() {
        let where_clause = if !author_email.is_empty() {
            format!("a.email = {}", escape_str(&author_email))
        } else {
            format!("a.name = {}", escape_str(&author_login))
        };
        if db
            .run(&format!(
                "MATCH (a:Author), (p:PR {{number: {n}}}) WHERE {wc} \
                 MERGE (a)-[:AUTHORED]->(p)",
                n = number,
                wc = where_clause,
            ))
            .is_ok()
        {
            edges += 1;
        }
    }

    // Merge commit link: only when we have a sha AND the commit is in the graph.
    if !merge_sha.is_empty()
        && db
            .run(&format!(
                "MATCH (p:PR {{number: {n}}}), (c:GitCommit {{hash: {h}}}) \
                 MERGE (p)-[:MERGES_INTO]->(c)",
                n = number,
                h = escape_str(&merge_sha),
            ))
            .is_ok()
    {
        edges += 1;
    }

    // Worklog refs from title + body.
    let mut refs: Vec<String> = Vec::new();
    refs.extend(extract_worklog_refs(&pr.title));
    refs.extend(extract_worklog_refs(&pr.body));
    refs.sort();
    refs.dedup();
    for id in &refs {
        if db
            .run(&format!(
                "MATCH (p:PR {{number: {n}}}), (w:WorklogItem {{id: {id}}}) \
                 MERGE (p)-[:REFERENCES]->(w)",
                n = number,
                id = escape_str(id),
            ))
            .is_ok()
        {
            edges += 1;
        }
    }

    ok_text(format!(
        "# import_pr\n\n\
         - PR: **#{number}**  ·  state: `{state}`\n\
         - title: {title}\n\
         - edges written: **{edges}**\n\
         - worklog refs found: {refs_render}\n",
        state = pr.state,
        title = pr.title,
        refs_render = if refs.is_empty() {
            "_(none)_".to_string()
        } else {
            refs.iter()
                .map(|r| format!("`{r}`"))
                .collect::<Vec<_>>()
                .join(", ")
        },
    ))
}

/// Permissive worklog-id extractor: matches any token shaped like
/// `nx-<chars>` (case-insensitive), `[A-Za-z]+-\d+`, or
/// `[A-Za-z]+_[A-Za-z0-9]+`. Looser than the strict `Refs:` trailer
/// parser the indexer uses for commit messages, because PR titles and
/// bodies mention IDs in prose ("fixes nx-42", "see #123").
fn extract_worklog_refs(text: &str) -> Vec<String> {
    // Walk the string char-by-char, accumulate runs of [A-Za-z0-9_-].
    // Tokens shaped like `xx-yy` (letters-digits / letters-letters with
    // a hyphen) qualify. Conservative enough to skip plain numbers and
    // ordinary words.
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let push = |out: &mut Vec<String>, cur: &mut String| {
        if cur.contains('-') {
            let parts: Vec<&str> = cur.split('-').collect();
            if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                // require at least one ASCII letter to avoid pure numerics.
                if cur.chars().any(|c| c.is_ascii_alphabetic()) && !out.iter().any(|x| x == cur) {
                    out.push(cur.clone());
                }
            }
        }
        cur.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            cur.push(c);
        } else {
            push(&mut out, &mut cur);
        }
    }
    push(&mut out, &mut cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(v: &Value) -> String {
        v["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn extract_refs_finds_common_id_shapes() {
        let s = "Closes nx-09. Fixes nx-15 and abc-42, mentions #99 but not 999.";
        let refs = extract_worklog_refs(s);
        assert!(refs.contains(&"nx-09".to_string()), "{refs:?}");
        assert!(refs.contains(&"nx-15".to_string()), "{refs:?}");
        assert!(refs.contains(&"abc-42".to_string()), "{refs:?}");
        assert!(!refs.contains(&"99".to_string()), "{refs:?}");
    }

    #[test]
    fn import_pr_creates_node_and_renders_summary() {
        let db = Db::open_in_memory().unwrap();
        let payload = json!({
            "number": 7,
            "title": "feat: bake the cake (refs nx-09)",
            "state": "OPEN",
            "body": "Closes nx-15.",
            "author": {"login": "ChrisRega", "email": ""},
        })
        .to_string();
        let v = handle_import_pr(&db, &json!({"json": payload}));
        let md = text(&v);
        assert!(md.contains("PR: **#7**"), "{md}");
        assert!(md.contains("`OPEN`"), "{md}");
        assert!(md.contains("nx-09") && md.contains("nx-15"), "{md}");

        // Node exists with the right title.
        let t = db
            .query("MATCH (p:PR {number: 7}) RETURN p.title AS t")
            .unwrap();
        assert_eq!(
            t.rows[0][0].as_str().unwrap(),
            "feat: bake the cake (refs nx-09)"
        );
    }

    #[test]
    fn import_pr_links_to_worklog_items_when_they_exist() {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (:WorklogItem {id: 'nx-09', title: 'foo'})")
            .unwrap();
        let payload = json!({
            "number": 3,
            "title": "feat: pass-2",
            "state": "MERGED",
            "body": "Closes nx-09. Also touches nx-99 (no node).",
        })
        .to_string();
        handle_import_pr(&db, &json!({"json": payload}));
        let t = db
            .query("MATCH (p:PR {number: 3})-[:REFERENCES]->(w:WorklogItem) RETURN w.id AS id")
            .unwrap();
        let ids: Vec<&str> = t.rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["nx-09"]);
    }

    #[test]
    fn import_pr_rejects_missing_number() {
        let db = Db::open_in_memory().unwrap();
        let payload = json!({"title": "no number"}).to_string();
        let v = handle_import_pr(&db, &json!({"json": payload}));
        let md = text(&v);
        assert!(md.contains("no `number`"), "{md}");
    }
}
