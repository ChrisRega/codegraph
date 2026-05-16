//! LSP-based indexing — uses a language server for compiler-precise code intelligence.
//!
//! Replaces the syn-based parser with LSP calls:
//!   documentSymbol()      → Symbol + Function nodes
//!   outgoingCalls()       → CALLS edges (compiler-precise, not name-match)
//!   goToImplementation()  → IMPLEMENTS edges

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use codegraph_core::{escape_str, Db};
use lsp_types::{DocumentSymbol, SymbolKind};

use crate::lsp::{self, LspClient};

/// `didOpen` if not yet known to the LSP, otherwise `didChange` with the
/// next version. Tracks state in `opened: path → next_version`.
fn open_or_change(
    lsp: &mut LspClient,
    opened: &mut HashMap<PathBuf, i32>,
    abs_path: &Path,
) -> Result<(), String> {
    let path_buf = abs_path.to_path_buf();
    if let Some(next) = opened.get_mut(&path_buf) {
        let v = *next;
        *next += 1;
        lsp.change_file(abs_path, v)
    } else {
        lsp.open_file(abs_path)?;
        opened.insert(path_buf, 1);
        Ok(())
    }
}

/// Index all source files in the workspace using the given LSP client.
///
/// `opened` tracks which files have been `didOpen`-ed on this client so
/// subsequent passes use `didChange` instead — avoids the duplicate-
/// `didOpen` warning rust-analyzer logs and gives the LSP fresh content
/// after a save. Pass an empty (or fresh) map for a one-shot client.
///
/// `initial_warmup` is `true` on the first pass against a freshly-spawned
/// LSP — controls whether we sleep 15s after the bulk open (cold start)
/// or just 1s (server already idle).
///
/// Returns `(symbols_count, functions_count, calls_count)`.
pub fn index_files_via_lsp(
    db: &Db,
    lsp: &mut LspClient,
    opened: &mut HashMap<PathBuf, i32>,
    initial_warmup: bool,
    files: &[(PathBuf, String, String)],
    _workspace: &Path,
) -> (u32, u32, u32) {
    let mut total_symbols = 0u32;
    let mut total_functions = 0u32;
    let mut total_calls = 0u32;

    let mut fn_positions: Vec<(PathBuf, u32, u32, String)> = Vec::new();

    eprintln!("  [*] Opening {} files in LSP server...", files.len());
    for (abs_path, _, _) in files {
        let _ = open_or_change(lsp, opened, abs_path);
    }

    // Wait for the LSP to actually finish indexing the open files. Replaces
    // the previous blind `thread::sleep` — most workspaces settle well
    // below the cap, and a warm pool typically returns in ~1s.
    let (silence_ms, max_ms) = if initial_warmup {
        (1500, 30_000)
    } else {
        (400, 3_000)
    };
    eprintln!(
        "  [*] Waiting for language server to {} (silence_ms={silence_ms}, max_ms={max_ms})...",
        if initial_warmup {
            "index workspace"
        } else {
            "settle"
        }
    );
    lsp.wait_until_idle(silence_ms, max_ms);

    for (abs_path, rel_path, pkg_name) in files {
        let file_content = std::fs::read_to_string(abs_path).unwrap_or_default();
        let file_lines: Vec<&str> = file_content.lines().collect();
        let line_count = file_lines.len();
        let path_lit = escape_str(rel_path);
        let fname = abs_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        run(db, &format!(
            "MERGE (f:File {{path: {path_lit}}}) SET f.name = {fn_lit}, f.extension = 'rs', f.lines = {line_count}",
            fn_lit = escape_str(&fname),
        ));
        run(db, &format!(
            "MATCH (p:Package {{name: {pn}}}), (f:File {{path: {path_lit}}}) MERGE (p)-[:CONTAINS]->(f)",
            pn = escape_str(pkg_name),
        ));

        // Already opened/changed in the bulk pass above; do not re-fire the
        // notification per file (would either double-open or emit a redundant
        // didChange and confuse the LSP's version counter).
        let symbols = match lsp.document_symbols(abs_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [!] LSP documentSymbol failed: {} — {e}", rel_path);
                continue;
            }
        };

        let module_prefix = module_prefix_from_path(pkg_name, rel_path);
        walk_symbols(
            db,
            &symbols,
            &module_prefix,
            rel_path,
            abs_path,
            &file_lines,
            &mut fn_positions,
            &mut total_symbols,
            &mut total_functions,
        );
    }

    // ── Build call graph via LSP outgoingCalls ───────────────────────────────
    eprintln!(
        "  [*] Building call graph via LSP ({} functions)...",
        fn_positions.len()
    );

    // Scope the CALLS wipe to functions in THIS pass. The previous
    // unconditional `MATCH (a:Function)-[c:CALLS]->(b:Function) DELETE c`
    // nuked the entire call graph on every incremental pass, so a single-
    // file save in --watch live mode left the rest of the codebase
    // CALLS-less until the next full reindex. With per-pass scoping, only
    // the changed files' callers are rewritten; everything else stays put.
    //
    // Chunked because velr 0.2.16's planner explodes (multi-GB heap, no
    // forward progress) when the `IN [...]` list grows past a few hundred
    // entries combined with a `DELETE` clause. 100 per chunk is empirically
    // safe; lower if a future caller is observed to OOM.
    if !fn_positions.is_empty() {
        let current_qns: HashSet<&str> = fn_positions
            .iter()
            .map(|(_, _, _, qn)| qn.as_str())
            .collect();
        let qns: Vec<&&str> = current_qns.iter().collect();
        for chunk in qns.chunks(100) {
            let in_list = chunk
                .iter()
                .map(|qn| escape_str(qn))
                .collect::<Vec<_>>()
                .join(",");
            run(
                db,
                &format!(
                    "MATCH (a:Function)-[c:CALLS]->(b:Function) \
                     WHERE a.qualified_name IN [{in_list}] DELETE c"
                ),
            );
        }
    }

    for (abs_path, line, character, caller_qn) in &fn_positions {
        let calls = match lsp.outgoing_calls(abs_path, *line, *character) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let mut seen = HashSet::new();
        for call in &calls {
            let callee_name = &call.to.name;
            if !seen.insert(callee_name.clone()) {
                continue;
            }
            run(
                db,
                &format!(
                    "MATCH (a:Function {{qualified_name: {caller}}}), (b:Function {{name: {callee}}}) CREATE (a)-[:CALLS]->(b)",
                    caller = escape_str(caller_qn),
                    callee = escape_str(callee_name),
                ),
            );
            total_calls += 1;
        }
    }

    (total_symbols, total_functions, total_calls)
}

#[allow(clippy::too_many_arguments)]
fn walk_symbols(
    db: &Db,
    symbols: &[DocumentSymbol],
    prefix: &str,
    rel_file_path: &str,
    abs_path: &Path,
    file_lines: &[&str],
    fn_positions: &mut Vec<(PathBuf, u32, u32, String)>,
    total_symbols: &mut u32,
    total_functions: &mut u32,
) {
    for sym in symbols {
        let name = &sym.name;
        let kind = sym.kind;
        let kind_str = lsp::symbol_kind_str(kind);
        let line_start = lsp::line_1based(&sym.range.start);
        let line_end = lsp::line_1based(&sym.range.end);
        let qualified_name = format!("{prefix}::{name}");
        let path_lit = escape_str(rel_file_path);
        let qn_lit = escape_str(&qualified_name);
        let name_lit = escape_str(name);

        match kind {
            SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::CONSTRUCTOR => {
                let is_test = name.starts_with("test_");
                let fn_kind = if is_test {
                    "Test"
                } else if kind == SymbolKind::METHOD {
                    "Method"
                } else {
                    "Free"
                };

                let body = slice_body(file_lines, line_start, line_end);
                run(db, &format!(
                    "CREATE (fn:Function {{qualified_name: {qn_lit}, name: {name_lit}, kind: {fk}, line_start: {line_start}, line_end: {line_end}, body: {body_lit}}})",
                    fk = escape_str(fn_kind),
                    body_lit = escape_str(&body),
                ));
                run(db, &format!(
                    "MATCH (f:File {{path: {path_lit}}}), (fn:Function {{qualified_name: {qn_lit}}}) CREATE (fn)-[:DEFINED_IN]->(f)"
                ));

                fn_positions.push((
                    abs_path.to_path_buf(),
                    sym.selection_range.start.line,
                    sym.selection_range.start.character,
                    qualified_name.clone(),
                ));
                *total_functions += 1;
            }
            SymbolKind::STRUCT
            | SymbolKind::ENUM
            | SymbolKind::INTERFACE
            | SymbolKind::TYPE_PARAMETER
            | SymbolKind::CONSTANT
            | SymbolKind::VARIABLE => {
                let body = slice_body(file_lines, line_start, line_end);
                run(db, &format!(
                    "CREATE (s:Symbol {{qualified_name: {qn_lit}, name: {name_lit}, kind: {ks}, line_start: {line_start}, line_end: {line_end}, body: {body_lit}}})",
                    ks = escape_str(kind_str),
                    body_lit = escape_str(&body),
                ));
                run(db, &format!(
                    "MATCH (f:File {{path: {path_lit}}}), (s:Symbol {{qualified_name: {qn_lit}}}) CREATE (s)-[:DEFINED_IN]->(f)"
                ));
                *total_symbols += 1;
            }
            SymbolKind::MODULE => {
                let child_prefix = format!("{prefix}::{name}");
                if let Some(children) = &sym.children {
                    walk_symbols(
                        db,
                        children,
                        &child_prefix,
                        rel_file_path,
                        abs_path,
                        file_lines,
                        fn_positions,
                        total_symbols,
                        total_functions,
                    );
                }
                continue;
            }
            _ => {}
        }

        if let Some(children) = &sym.children {
            let child_prefix = format!("{prefix}::{name}");
            walk_symbols(
                db,
                children,
                &child_prefix,
                rel_file_path,
                abs_path,
                file_lines,
                fn_positions,
                total_symbols,
                total_functions,
            );
        }
    }
}

fn run(db: &Db, cypher: &str) {
    if let Err(e) = db.run(cypher) {
        eprintln!("  [!] Query failed: {}\n      {}", e, cypher);
    }
}

fn slice_body(file_lines: &[&str], line_start: u32, line_end: u32) -> String {
    let mut start = (line_start.saturating_sub(1)) as usize;
    let end = (line_end as usize).min(file_lines.len());
    // Walk backwards over preceding attribute lines so test markers
    // (`#[test]`, `#[tokio::test]`) and other attributes (`#[derive(...)]`)
    // end up inside `body`. rust-analyzer's `DocumentSymbol.range.start`
    // points at the `fn`/`struct` keyword for items, not at the attribute
    // line above — without this back-scan, Phase 6's `body CONTAINS
    // '#[test]'` check never matches and we miss every test fn.
    while start > 0 {
        let prev = file_lines[start - 1].trim_start();
        if prev.starts_with("#[") || prev.starts_with("#![") {
            start -= 1;
        } else {
            break;
        }
    }
    if start >= end {
        return String::new();
    }
    file_lines[start..end].join("\n")
}

fn module_prefix_from_path(pkg_name: &str, file_path: &str) -> String {
    let path = file_path
        .replace('\\', "/")
        .trim_end_matches(".rs")
        .to_string();
    let after_src = path.find("/src/").map(|i| &path[i + 5..]).unwrap_or(&path);
    let clean = after_src
        .trim_end_matches("/mod")
        .trim_end_matches("/lib")
        .trim_end_matches("/main");
    if clean.is_empty() {
        pkg_name.to_string()
    } else {
        format!("{}::{}", pkg_name, clean.replace('/', "::"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<&str> {
        s.lines().collect()
    }

    #[test]
    fn slice_body_includes_preceding_attribute_lines() {
        let src = "\
some other line
#[test]
fn it_works() {
    assert!(true);
}";
        let ls = lines(src);
        // rust-analyzer's `range.start` would point at the `fn` keyword
        // (line 3 in 1-based terms) — back-scan should pull in the
        // #[test] attribute.
        let body = slice_body(&ls, 3, 5);
        assert!(body.contains("#[test]"), "got: {body}");
        assert!(body.contains("fn it_works"));
        assert!(!body.contains("some other line"));
    }

    #[test]
    fn slice_body_back_scans_multiple_attributes() {
        let src = "\
preamble
#[allow(unused)]
#[tokio::test(flavor = \"multi_thread\")]
async fn async_test() {
    body
}";
        let ls = lines(src);
        let body = slice_body(&ls, 4, 6);
        assert!(body.contains("#[tokio::test"), "{body}");
        assert!(body.contains("#[allow(unused)]"), "{body}");
        assert!(!body.contains("preamble"));
    }

    #[test]
    fn slice_body_handles_no_attributes() {
        let src = "header\nfn plain() { 1 }";
        let ls = lines(src);
        let body = slice_body(&ls, 2, 2);
        assert_eq!(body, "fn plain() { 1 }");
    }

    #[test]
    fn slice_body_stops_at_blank_or_code() {
        let src = "\
fn earlier() {}

#[test]
fn target() {}";
        let ls = lines(src);
        let body = slice_body(&ls, 4, 4);
        assert!(body.contains("#[test]"));
        // Blank line above #[test] stops the back-scan.
        assert!(!body.contains("fn earlier"));
    }
}
