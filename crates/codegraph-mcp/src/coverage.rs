//! `coverage_md` MCP tool — single Markdown report of the graph's
//! "dim spots": orphan functions, untested functions, files with no
//! notes, packages with zero doc-mentions.
//!
//! `collect_strings` and `collect_string_int_pairs` are kept private to
//! this module; they're handy enough that other modules will probably
//! want them too, at which point lift them into `util` or a `query`
//! sibling.

use codegraph_core::Db;
use serde_json::Value;

use crate::util::ok_text;

/// Run `query` and collect column `col` as a Vec<String>. Best-effort:
/// any error returns an empty Vec so a single failing section doesn't
/// kill the whole report.
fn collect_strings(db: &Db, query: &str, col: usize) -> Vec<String> {
    let t = match db.query(query) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    t.rows
        .iter()
        .filter_map(|r| r.get(col).and_then(|c| c.as_str()).map(str::to_string))
        .collect()
}

/// Run `query` and collect rows as `(String, i64)` pairs (col0 = key,
/// col1 = numeric metric). Used for ranked sections.
fn collect_string_int_pairs(db: &Db, query: &str) -> Vec<(String, i64)> {
    let t = match db.query(query) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    t.rows
        .iter()
        .filter_map(|r| {
            let s = r.first().and_then(|c| c.as_str())?.to_string();
            let n = r.get(1).and_then(|c| c.as_i64()).unwrap_or(0);
            Some((s, n))
        })
        .collect()
}

pub fn handle_coverage_md(db: &Db, params: &Value) -> Value {
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(15)
        .max(1) as usize;

    let mut out = String::new();
    out.push_str("# Graph coverage report\n\n");
    out.push_str(
        "_The dim spots of the graph: nodes that nothing touches, files \
         nobody has annotated, packages with no doc-mentions. Sorted by \
         impact where a fan-in metric is available._\n\n",
    );

    // ── Functions with no inbound CALLS ──────────────────────────────────────
    // Excludes :Test (test fns are entry points, lacking CALLS-in is normal).
    // velr 0.2.16 doesn't accept `WHERE NOT f:Test AND NOT (pattern)` in one
    // clause — its planner rejects the mixed label-predicate + existential.
    // So we collect all orphans and drop tests client-side.
    let test_qns: std::collections::HashSet<String> =
        collect_strings(db, "MATCH (t:Test) RETURN t.qualified_name AS qn", 0)
            .into_iter()
            .collect();
    let orphan_fns: Vec<String> = collect_strings(
        db,
        "MATCH (f:Function) WHERE NOT (f)<-[:CALLS]-(:Function) \
         RETURN f.qualified_name AS qn ORDER BY qn",
        0,
    )
    .into_iter()
    .filter(|qn| !test_qns.contains(qn))
    .collect();
    out.push_str(&format!(
        "## Orphan functions: no inbound `[:CALLS]` ({})\n\n",
        orphan_fns.len()
    ));
    if orphan_fns.is_empty() {
        out.push_str("_(none — every function is reachable)_\n\n");
    } else {
        out.push_str("_Either entry points (CLI `main`, public API) or genuinely dead code._\n\n");
        for qn in orphan_fns.iter().take(limit) {
            out.push_str(&format!("- `{qn}`\n"));
        }
        if orphan_fns.len() > limit {
            out.push_str(&format!("- _… {} more_\n", orphan_fns.len() - limit));
        }
        out.push('\n');
    }

    // ── Non-test functions with no inbound TESTS, ranked by fan-in ──────────
    // Highest-CALLS-fan-in untested functions surface first — those are the
    // refactor risks where a regression would cascade widest.
    // Same planner-shape constraint as orphans: drop the test filter from
    // Cypher, apply client-side.
    let untested: Vec<(String, i64)> = collect_string_int_pairs(
        db,
        "MATCH (f:Function) WHERE NOT (f)<-[:TESTS]-(:Test) \
         OPTIONAL MATCH (f)<-[c:CALLS]-(:Function) \
         RETURN f.qualified_name AS qn, count(c) AS fan_in \
         ORDER BY fan_in DESC, qn",
    )
    .into_iter()
    .filter(|(qn, _)| !test_qns.contains(qn))
    .collect();
    out.push_str(&format!(
        "## Untested functions, ranked by `[:CALLS]` fan-in ({})\n\n",
        untested.len()
    ));
    if untested.is_empty() {
        out.push_str("_(none — `:TESTS` covers every non-test function)_\n\n");
    } else {
        out.push_str(
            "_Higher fan-in = more callers depend on this; a regression here \
             cascades widest. Top of the list = best ROI for adding a test._\n\n",
        );
        out.push_str("| fan-in | qualified_name |\n| --- | --- |\n");
        for (qn, fan_in) in untested.iter().take(limit) {
            out.push_str(&format!("| {fan_in} | `{qn}` |\n"));
        }
        if untested.len() > limit {
            out.push_str(&format!(
                "\n_… {} more (raise `limit`)_\n",
                untested.len() - limit
            ));
        }
        out.push('\n');
    }

    // ── Files with no notes ─────────────────────────────────────────────────
    let no_note_files = collect_strings(
        db,
        "MATCH (f:File) WHERE NOT (f)<-[:NOTES]-(:Note) \
         RETURN f.path AS path ORDER BY path",
        0,
    );
    out.push_str(&format!(
        "## Files with no `:Note` ({})\n\n",
        no_note_files.len()
    ));
    if no_note_files.is_empty() {
        out.push_str("_(every file has at least one note)_\n\n");
    } else {
        for p in no_note_files.iter().take(limit) {
            out.push_str(&format!("- `{p}`\n"));
        }
        if no_note_files.len() > limit {
            out.push_str(&format!("- _… {} more_\n", no_note_files.len() - limit));
        }
        out.push('\n');
    }

    // ── Packages whose files have zero MENTIONS from any DocSection ─────────
    // velr 0.2.16 does not support `EXISTS { MATCH ... }` subqueries, so we
    // do the set-difference client-side: collect all packages, collect those
    // that *do* have a doc-mentioned function, and subtract.
    let all_packages: std::collections::BTreeSet<String> =
        collect_strings(db, "MATCH (p:Package) RETURN p.name AS name", 0)
            .into_iter()
            .collect();
    let documented_packages: std::collections::BTreeSet<String> = collect_strings(
        db,
        "MATCH (p:Package)-[:CONTAINS]->(:File)<-[:DEFINED_IN]-(:Function)<-[:MENTIONS]-(:DocSection) \
         RETURN DISTINCT p.name AS name",
        0,
    )
    .into_iter()
    .collect();
    let undoc_packages: Vec<String> = all_packages
        .difference(&documented_packages)
        .cloned()
        .collect();
    out.push_str(&format!(
        "## Packages with zero doc-mentions ({})\n\n",
        undoc_packages.len()
    ));
    if undoc_packages.is_empty() {
        out.push_str(
            "_(every package has at least one function mentioned in some `:DocSection`)_\n",
        );
    } else {
        for n in undoc_packages.iter().take(limit) {
            out.push_str(&format!("- `{n}`\n"));
        }
        if undoc_packages.len() > limit {
            out.push_str(&format!("- _… {} more_\n", undoc_packages.len() - limit));
        }
    }

    ok_text(out.trim_end().to_string())
}
