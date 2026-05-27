//! `dead_code` MCP tool — list `:Function` nodes with no incoming
//! `:CALLS` edges. The "graph-derived suspicious functions" report.
//!
//! Caveats are real and intentional — this is a *hint generator*, not
//! a verdict:
//!
//! - public functions reachable only from outside the workspace
//!   (library API, FFI, dynamic dispatch) look dead to the graph
//! - binaries' `main` looks dead (nothing calls it inside the graph)
//! - dispatcher pattern (e.g. `match name { "x" => handle_x() }`) hides
//!   the call from the AST-level :CALLS extractor, so handlers look
//!   dead
//!
//! Defaults therefore exclude `:Test`-labeled functions as candidates
//! (the test bodies *are* calls, but the test itself is the
//! entry-point) and `ignore_test_callers` is **off**: if a function is
//! only called by a test, it still counts as alive — flipping the
//! switch lets the agent find "covered-only-by-tests" code in a second
//! pass.

use std::collections::BTreeMap;

use codegraph_core::{escape_str, Db};
use serde_json::Value;

use crate::util::{err_text, ok_text};

pub fn handle_dead_code(db: &Db, params: &Value) -> Value {
    let exclude_tests = params
        .get("exclude_tests")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let ignore_test_callers = params
        .get("ignore_test_callers")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let kind_filter = params
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name_skip = params
        .get("name_skip")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(100)
        .clamp(1, 1000) as usize;

    // Candidate fetch. We always include file path + line so the output
    // is jumpable. `:Test` label is checked client-side because velr's
    // label-predicate-with-NOT pattern (see CLAUDE.md) is fragile.
    let mut wheres: Vec<String> = Vec::new();
    if !kind_filter.is_empty() {
        wheres.push(format!("f.kind = {}", escape_str(&kind_filter)));
    }
    let where_sql = if wheres.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wheres.join(" AND "))
    };

    let q = format!(
        "MATCH (f:Function){where_sql} \
         OPTIONAL MATCH (f)-[:DEFINED_IN]->(file:File) \
         RETURN f.qualified_name AS qn, f.name AS name, f.kind AS kind, \
                f.line_start AS line, file.path AS path, labels(f) AS lbls"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("dead_code candidate query failed: {e}")),
    };

    // Build a set of qualified_names that have *at least one* incoming
    // CALLS we care about. Two queries instead of one fat join because
    // velr's OPTIONAL MATCH + NOT predicate is the planner's least
    // favourite shape and we don't want a 20-GB WAL incident.
    let callers_q = if ignore_test_callers {
        // Only callers that are NOT :Test count as evidence of life.
        // Client-side filter via labels — keep the Cypher boring.
        "MATCH (caller:Function)-[:CALLS]->(callee:Function) \
         RETURN callee.qualified_name AS qn, labels(caller) AS caller_lbls"
            .to_string()
    } else {
        "MATCH (caller:Function)-[:CALLS]->(callee:Function) \
         RETURN callee.qualified_name AS qn, '' AS caller_lbls"
            .to_string()
    };
    let mut called: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(ct) = db.query(&callers_q) {
        for row in &ct.rows {
            let qn = row
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if qn.is_empty() {
                continue;
            }
            if ignore_test_callers {
                let lbls = row.get(1).and_then(|c| c.as_str()).unwrap_or("");
                if lbls.contains("Test") {
                    continue;
                }
            }
            called.insert(qn);
        }
    }

    let name_skip_lower = name_skip.to_ascii_lowercase();
    let mut dead_by_file: BTreeMap<String, Vec<(String, String, String, i64)>> = BTreeMap::new();
    let mut considered = 0u64;
    let mut total_dead = 0u64;
    for row in &t.rows {
        let qn = row
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let name = row
            .get(1)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let kind = row
            .get(2)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let line = row.get(3).and_then(|c| c.as_i64()).unwrap_or(0);
        let path = row
            .get(4)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let lbls = row
            .get(5)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        if qn.is_empty() {
            continue;
        }
        considered += 1;
        if exclude_tests && lbls.contains("Test") {
            continue;
        }
        if !name_skip_lower.is_empty() && name.to_ascii_lowercase().contains(&name_skip_lower) {
            continue;
        }
        if called.contains(&qn) {
            continue;
        }
        total_dead += 1;
        dead_by_file
            .entry(path)
            .or_default()
            .push((qn, name, kind, line));
    }

    let mut out = String::new();
    out.push_str("# dead_code\n\n");
    out.push_str(&format!(
        "- candidates considered: **{considered}**\n\
         - unreferenced after filters: **{total_dead}**\n\
         - showing: up to **{limit}** (sorted by file, then line)\n\n"
    ));
    if exclude_tests {
        out.push_str("_filter active: `:Test` candidates excluded_\n");
    }
    if ignore_test_callers {
        out.push_str("_filter active: test-only callers don't count as life_\n");
    }
    if !kind_filter.is_empty() {
        out.push_str(&format!("_filter active: kind = `{kind_filter}`_\n"));
    }
    if !name_skip.is_empty() {
        out.push_str(&format!("_filter active: name contains `{name_skip}`_\n"));
    }
    out.push('\n');

    if dead_by_file.is_empty() {
        out.push_str("_(nothing matched — repo looks clean by this heuristic)_\n");
        return ok_text(out);
    }

    out.push_str("| file:line | kind | qualified_name |\n");
    out.push_str("|-----------|------|----------------|\n");
    let mut shown = 0usize;
    'outer: for (path, mut items) in dead_by_file {
        items.sort_by_key(|(_, _, _, line)| *line);
        for (qn, _name, kind, line) in items {
            if shown >= limit {
                break 'outer;
            }
            out.push_str(&format!("| `{path}:{line}` | `{kind}` | `{qn}` |\n"));
            shown += 1;
        }
    }

    out.push_str(
        "\n> Heuristic only — `main`, public API, FFI, dynamic dispatch (e.g. \
         string-matched handlers) and trait impls look dead to the graph. \
         Verify before deleting.\n",
    );
    ok_text(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(v: &Value) -> String {
        v["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn seed() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (alive:Function {qualified_name: 'm::alive', name: 'alive', kind: 'Free', line_start: 10})").unwrap();
        db.run("CREATE (dead:Function {qualified_name: 'm::dead', name: 'dead', kind: 'Free', line_start: 20})").unwrap();
        db.run("CREATE (caller:Function {qualified_name: 'm::caller', name: 'caller', kind: 'Free', line_start: 30})").unwrap();
        db.run("CREATE (test:Function:Test {qualified_name: 'm::test_alive', name: 'test_alive', kind: 'Free', line_start: 100})").unwrap();
        db.run("CREATE (file:File {path: 'src/m.rs'})").unwrap();
        for qn in ["m::alive", "m::dead", "m::caller", "m::test_alive"] {
            db.run(&format!(
                "MATCH (f:Function {{qualified_name: '{qn}'}}), (file:File {{path: 'src/m.rs'}}) \
                 MERGE (f)-[:DEFINED_IN]->(file)"
            ))
            .unwrap();
        }
        db.run("MATCH (a:Function {qualified_name: 'm::caller'}), (b:Function {qualified_name: 'm::alive'}) MERGE (a)-[:CALLS]->(b)").unwrap();
        db.run("MATCH (t:Function {qualified_name: 'm::test_alive'}), (b:Function {qualified_name: 'm::dead'}) MERGE (t)-[:CALLS]->(b)").unwrap();
        db
    }

    #[test]
    fn default_filters_exclude_tests_and_count_test_callers_as_life() {
        let db = seed();
        let md = text(&handle_dead_code(&db, &json!({})));
        // alive: caller → alive, lives
        assert!(!md.contains("m::alive"), "alive shouldn't show: {md}");
        // dead: test calls dead → with default ignore_test_callers=false, dead is alive too
        assert!(!md.contains("m::dead"), "dead has test caller: {md}");
        // caller: nothing calls caller → dead
        assert!(md.contains("m::caller"), "caller should show: {md}");
        // test_alive: excluded (label :Test)
        assert!(!md.contains("test_alive"), "test excluded: {md}");
    }

    #[test]
    fn ignore_test_callers_flips_dead_status_for_only_test_called() {
        let db = seed();
        let md = text(&handle_dead_code(
            &db,
            &json!({"ignore_test_callers": true}),
        ));
        // dead is only called by test_alive, so now it counts as dead
        assert!(md.contains("m::dead"), "dead should show: {md}");
        assert!(md.contains("m::caller"), "caller should still show: {md}");
        // alive has non-test caller, still alive
        assert!(!md.contains("m::alive"), "alive still alive: {md}");
    }

    #[test]
    fn name_skip_filters_out_named_functions() {
        let db = seed();
        let md = text(&handle_dead_code(&db, &json!({"name_skip": "caller"})));
        assert!(!md.contains("m::caller"), "name_skip should hide: {md}");
    }

    #[test]
    fn empty_result_prints_clean_message() {
        let db = Db::open_in_memory().unwrap();
        db.run(
            "CREATE (a:Function {qualified_name: 'x::a', name: 'a', kind: 'Free', line_start: 1})",
        )
        .unwrap();
        db.run(
            "CREATE (b:Function {qualified_name: 'x::b', name: 'b', kind: 'Free', line_start: 2})",
        )
        .unwrap();
        db.run("MATCH (a:Function {qualified_name: 'x::a'}), (b:Function {qualified_name: 'x::b'}) MERGE (a)-[:CALLS]->(b) MERGE (b)-[:CALLS]->(a)").unwrap();
        let md = text(&handle_dead_code(&db, &json!({})));
        assert!(md.contains("repo looks clean"), "{md}");
    }
}
