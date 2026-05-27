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
    if params
        .get("by_package")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return render_by_package(db, limit);
    }

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

/// `coverage_md --by-package` (nx-7) — rollup of the same dim-spot
/// signals but aggregated to one row per internal `:Package`.
/// Assignment of `:Function`/`:File` to a package is by longest
/// matching `file.path` prefix against `package.path`, same heuristic
/// as `arch.rs`. Returns a sortable Markdown table — top of the list
/// = the package most in need of attention.
fn render_by_package(db: &Db, _limit: usize) -> Value {
    // Fetch internal packages with their source paths.
    let pkgs: Vec<(String, String)> = {
        let mut v: Vec<(String, String)> = Vec::new();
        if let Ok(t) = db.query(
            "MATCH (p:Package) WHERE p.is_external = false RETURN p.name AS name, p.path AS path",
        ) {
            for row in &t.rows {
                let n = row.first().and_then(|c| c.as_str()).unwrap_or("");
                let p = row.get(1).and_then(|c| c.as_str()).unwrap_or("");
                if !n.is_empty() && !p.is_empty() {
                    v.push((n.to_string(), p.to_string()));
                }
            }
        }
        v
    };
    if pkgs.is_empty() {
        return ok_text(
            "# Graph coverage report — by package\n\n_(no internal packages found)_".to_string(),
        );
    }
    // Sort by descending path length so longest-prefix match wins on
    // nested workspaces.
    let mut sorted = pkgs.clone();
    sorted.sort_by_key(|(_, p)| std::cmp::Reverse(p.len()));
    let assign = |path: &str| -> Option<&str> {
        sorted
            .iter()
            .find(|(_, p)| path.starts_with(p))
            .map(|(n, _)| n.as_str())
    };

    // Per-package counters.
    use std::collections::HashMap;
    struct Stats {
        total_fns: u32,
        orphan_fns: u32,
        untested_fns: u32,
        files: u32,
        files_no_notes: u32,
        doc_mentions: u32,
    }
    let mut stats: HashMap<String, Stats> = pkgs
        .iter()
        .map(|(n, _)| {
            (
                n.clone(),
                Stats {
                    total_fns: 0,
                    orphan_fns: 0,
                    untested_fns: 0,
                    files: 0,
                    files_no_notes: 0,
                    doc_mentions: 0,
                },
            )
        })
        .collect();

    // Functions + their inbound CALLS/TESTS via two helper sets.
    let test_qns: std::collections::HashSet<String> =
        collect_strings(db, "MATCH (t:Test) RETURN t.qualified_name AS qn", 0)
            .into_iter()
            .collect();
    let tested_qns: std::collections::HashSet<String> = collect_strings(
        db,
        "MATCH (t:Test)-[:TESTS]->(f:Function) RETURN DISTINCT f.qualified_name AS qn",
        0,
    )
    .into_iter()
    .collect();
    let called_qns: std::collections::HashSet<String> = collect_strings(
        db,
        "MATCH (:Function)-[:CALLS]->(callee:Function) RETURN DISTINCT callee.qualified_name AS qn",
        0,
    )
    .into_iter()
    .collect();

    let fn_rows = match db.query(
        "MATCH (f:Function)-[:DEFINED_IN]->(file:File) \
         RETURN DISTINCT f.qualified_name AS qn, file.path AS path",
    ) {
        Ok(t) => t,
        Err(e) => return crate::util::err_text(format!("by-package fn fetch failed: {e}")),
    };
    for row in &fn_rows.rows {
        let qn = row.first().and_then(|c| c.as_str()).unwrap_or("");
        let path = row.get(1).and_then(|c| c.as_str()).unwrap_or("");
        let Some(pkg) = assign(path) else { continue };
        let Some(s) = stats.get_mut(pkg) else {
            continue;
        };
        if test_qns.contains(qn) {
            // Test functions are intentionally entry points — don't
            // count them as orphans, but do count them in the total.
            s.total_fns += 1;
            continue;
        }
        s.total_fns += 1;
        if !called_qns.contains(qn) {
            s.orphan_fns += 1;
        }
        if !tested_qns.contains(qn) {
            s.untested_fns += 1;
        }
    }

    // Files + notes.
    let notes_per_file: std::collections::HashSet<String> = collect_strings(
        db,
        "MATCH (n:Note)-[:NOTES]->(f:File) RETURN DISTINCT f.path AS path",
        0,
    )
    .into_iter()
    .collect();
    let file_rows = match db.query("MATCH (f:File) RETURN DISTINCT f.path AS path") {
        Ok(t) => t,
        Err(e) => return crate::util::err_text(format!("by-package file fetch failed: {e}")),
    };
    for row in &file_rows.rows {
        let path = row.first().and_then(|c| c.as_str()).unwrap_or("");
        let Some(pkg) = assign(path) else { continue };
        let Some(s) = stats.get_mut(pkg) else {
            continue;
        };
        s.files += 1;
        if !notes_per_file.contains(path) {
            s.files_no_notes += 1;
        }
    }

    // Doc mentions: each :MENTIONS edge whose target Function lives in
    // a file under a package path.
    let mention_rows = match db.query(
        "MATCH (:DocSection)-[:MENTIONS]->(f:Function)-[:DEFINED_IN]->(file:File) \
         RETURN file.path AS path",
    ) {
        Ok(t) => t,
        Err(_) => codegraph_core::Table {
            columns: Vec::new(),
            rows: Vec::new(),
        },
    };
    for row in &mention_rows.rows {
        let path = row.first().and_then(|c| c.as_str()).unwrap_or("");
        let Some(pkg) = assign(path) else { continue };
        let Some(s) = stats.get_mut(pkg) else {
            continue;
        };
        s.doc_mentions += 1;
    }

    // Render — sort by "needs attention" first: orphan_fns + untested_fns desc.
    let mut rows: Vec<(&str, &Stats)> = stats.iter().map(|(k, v)| (k.as_str(), v)).collect();
    rows.sort_by(|a, b| {
        let score_a = a.1.orphan_fns + a.1.untested_fns;
        let score_b = b.1.orphan_fns + b.1.untested_fns;
        score_b.cmp(&score_a).then_with(|| a.0.cmp(b.0))
    });

    let mut out = String::new();
    out.push_str("# Graph coverage — by package\n\n");
    out.push_str(
        "_One row per internal `:Package`. Sorted by `orphan_fns + untested_fns` \
         desc — top = most in need of attention. Test functions excluded from \
         orphan/untested counters but still in `fns`._\n\n",
    );
    out.push_str("| package | fns | orphan | untested | files | no_note | doc_mentions |\n");
    out.push_str("|---------|----:|-------:|---------:|------:|--------:|-------------:|\n");
    for (name, s) in &rows {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} |\n",
            name,
            s.total_fns,
            s.orphan_fns,
            s.untested_fns,
            s.files,
            s.files_no_notes,
            s.doc_mentions,
        ));
    }
    ok_text(out.trim_end().to_string())
}
