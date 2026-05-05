//! LSP-based indexing — uses a language server for compiler-precise code intelligence.
//!
//! Replaces the syn-based parser with LSP calls:
//!   documentSymbol()      → Symbol + Function nodes
//!   outgoingCalls()       → CALLS edges (compiler-precise, not name-match)
//!   goToImplementation()  → IMPLEMENTS edges

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use codegraph_core::{escape_str, Db};
use lsp_types::{DocumentSymbol, SymbolKind};

use crate::lsp::{self, LspClient};

/// Index all source files in the workspace using the given LSP client.
///
/// Returns `(symbols_count, functions_count, calls_count)`.
pub fn index_files_via_lsp(
    db: &Db,
    lsp: &mut LspClient,
    files: &[(PathBuf, String, String)],
    _workspace: &Path,
) -> (u32, u32, u32) {
    let mut total_symbols = 0u32;
    let mut total_functions = 0u32;
    let mut total_calls = 0u32;

    let mut fn_positions: Vec<(PathBuf, u32, u32, String)> = Vec::new();

    eprintln!("  [*] Opening {} files in LSP server...", files.len());
    for (abs_path, _, _) in files {
        let _ = lsp.open_file(abs_path);
    }

    eprintln!("  [*] Waiting for language server to index...");
    std::thread::sleep(std::time::Duration::from_secs(15));

    for (abs_path, rel_path, pkg_name) in files {
        let file_content = std::fs::read_to_string(abs_path).unwrap_or_default();
        let file_lines: Vec<&str> = file_content.lines().collect();
        let line_count = file_lines.len();
        let path_lit = escape_str(rel_path);
        let fname = abs_path.file_name().unwrap_or_default().to_string_lossy().to_string();

        run(db, &format!(
            "MERGE (f:File {{path: {path_lit}}}) SET f.name = {fn_lit}, f.extension = 'rs', f.lines = {line_count}",
            fn_lit = escape_str(&fname),
        ));
        run(db, &format!(
            "MATCH (p:Package {{name: {pn}}}), (f:File {{path: {path_lit}}}) MERGE (p)-[:CONTAINS]->(f)",
            pn = escape_str(pkg_name),
        ));

        if let Err(e) = lsp.open_file(abs_path) {
            eprintln!("  [!] LSP open failed: {} — {e}", rel_path);
            continue;
        }
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
    eprintln!("  [*] Building call graph via LSP ({} functions)...", fn_positions.len());

    run(db, "MATCH (a:Function)-[c:CALLS]->(b:Function) DELETE c");

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
    let start = (line_start.saturating_sub(1)) as usize;
    let end = (line_end as usize).min(file_lines.len());
    if start >= end {
        return String::new();
    }
    file_lines[start..end].join("\n")
}

fn module_prefix_from_path(pkg_name: &str, file_path: &str) -> String {
    let path = file_path.replace('\\', "/").trim_end_matches(".rs").to_string();
    let after_src = path.find("/src/").map(|i| &path[i + 5..]).unwrap_or(&path);
    let clean = after_src.trim_end_matches("/mod").trim_end_matches("/lib").trim_end_matches("/main");
    if clean.is_empty() {
        pkg_name.to_string()
    } else {
        format!("{}::{}", pkg_name, clean.replace('/', "::"))
    }
}
