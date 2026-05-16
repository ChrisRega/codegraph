//! codegraph indexer — walks a workspace and projects it into a velr graph.
//!
//! Pipeline:
//!   1. **Bootstrap** — parse the workspace manifest (`Cargo.toml` /
//!      `package.json` / `pyproject.toml`), emit `:Workspace` and `:Package`
//!      nodes, discover member packages and source roots.
//!   2. **LSP indexing** — for every source file, request `documentSymbol`
//!      from the LSP and project the symbol tree into `:File` / `:Symbol` /
//!      `:Function` nodes plus `DEFINED_IN` edges. Calls graph via
//!      `callHierarchy/outgoingCalls`.
//!   3. **API specs** — OpenAPI / GraphQL SDL / Protobuf into `:APIEndpoint` /
//!      `:APIType` nodes.
//!   4. **BDD post-processing** — Gherkin walker over `*.feature` plus a syn
//!      pass over test files extracting `#[given/when/then(regex = "…")]`
//!      decorators; the linker matches `Step.text` to `step_regex` and writes
//!      `IMPLEMENTED_BY` edges.
//!   5. **Markdown ↔ code linking** — every `.md` projected into `:Doc` /
//!      `:DocSection` with `MENTIONS` / `LINKS_TO` edges.
//!
//! ### Incremental indexing
//!
//! velr (unlike cypherlite) has no built-in versioning. The indexer keeps a
//! sidecar file next to the database (`<db>.codegraph-meta.json`) recording
//! the last-indexed git commit. On the next run, `git diff` between that
//! commit and HEAD identifies changed files; only those are re-parsed and
//! their stale subgraph is rewritten. `--full` forces a clean rebuild.
//!
//! Usage:
//!   codegraph-indexer --workspace /path/to/project --db code-graph.db [--full] [--lsp BIN]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use codegraph_core::{escape_str, Db, Value};
use walkdir::WalkDir;

mod api_spec;
mod bdd_steps;
mod gherkin;
pub mod lsp;
mod lsp_index;
mod markdown_index;
pub mod meta;

pub use lsp::LspPool;

/// Public entry point for `codegraph-indexer`. Mirrors the CLI flags.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub workspace: PathBuf,
    pub db_path: String,
    pub lsp_cmd_override: Option<String>,
    pub force_full: bool,
    /// Live-indexing override: when `Some`, skip git diff and only
    /// re-process the supplied workspace-relative paths. Also skips
    /// the git-history phase and the sidecar metadata write — the
    /// sidecar still tracks the last *committed* pass so the next
    /// CLI run picks up where it left off. Used by the MCP server's
    /// `--watch` mode to reflect uncommitted edits in the graph.
    pub path_set: Option<Vec<String>>,
}

impl IndexOptions {
    pub fn new(workspace: PathBuf, db_path: impl Into<String>) -> Self {
        Self {
            workspace,
            db_path: db_path.into(),
            lsp_cmd_override: None,
            force_full: false,
            path_set: None,
        }
    }
    /// Switch to live-indexing mode against the given relative paths.
    pub fn with_paths(mut self, paths: Vec<String>) -> Self {
        self.path_set = Some(paths);
        self
    }
}

/// Summary returned by [`run_indexer`].
#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    /// `"full"`, `"incremental"`, or `"noop"` (already up-to-date).
    pub mode: &'static str,
    pub symbols: usize,
    pub functions: usize,
    pub call_edges: usize,
    pub head_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ProjectKind {
    Rust,
    Node,
    Python,
}

impl ProjectKind {
    fn detect(workspace: &Path) -> Self {
        if workspace.join("Cargo.toml").exists() {
            ProjectKind::Rust
        } else if workspace.join("package.json").exists() {
            ProjectKind::Node
        } else if workspace.join("pyproject.toml").exists()
            || workspace.join("requirements.txt").exists()
            || workspace.join("setup.py").exists()
        {
            ProjectKind::Python
        } else {
            ProjectKind::Rust
        }
    }
    #[allow(dead_code)]
    fn language(&self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Node => "TypeScript",
            Self::Python => "Python",
        }
    }
    fn default_lsp(&self) -> &'static str {
        match self {
            Self::Rust => "rust-analyzer",
            Self::Node => "typescript-language-server",
            Self::Python => "pyright-langserver",
        }
    }
    fn lsp_args(&self) -> Vec<&'static str> {
        match self {
            Self::Rust => vec![],
            Self::Node => vec!["--stdio"],
            Self::Python => vec!["--stdio"],
        }
    }
    fn extensions(&self) -> &[&str] {
        match self {
            Self::Rust => &["rs"],
            Self::Node => &["ts", "tsx", "js", "jsx"],
            Self::Python => &["py"],
        }
    }
}

/// Run a single indexer pass with a transient LSP client (started fresh,
/// shut down at the end). Suitable for the CLI binary.
pub fn run_indexer(opts: IndexOptions) -> Result<IndexStats, String> {
    run_indexer_inner(opts, None)
}

/// Run a single indexer pass against `pool`'s persistent LSP clients —
/// the per-language LSP is started lazily on first use and reused across
/// every subsequent call. Saves the (~5s) cold-start cost on each batch
/// and switches per-file notifications to `didChange` after the first
/// `didOpen`.
pub fn run_indexer_with_pool(opts: IndexOptions, pool: &mut LspPool) -> Result<IndexStats, String> {
    run_indexer_inner(opts, Some(pool))
}

/// Shared implementation — the only difference between the two public
/// entry points is whether the LSP client is owned by the pool or by
/// this function.
fn run_indexer_inner(opts: IndexOptions, pool: Option<&mut LspPool>) -> Result<IndexStats, String> {
    let IndexOptions {
        workspace: workspace_input,
        db_path,
        lsp_cmd_override,
        force_full,
        path_set,
    } = opts;
    // Live mode: caller supplied an explicit path set. Skip git diff,
    // skip history phase, do not advance sidecar metadata.
    let live_paths = path_set;

    let workspace_display = workspace_input.display().to_string();
    let workspace = workspace_input.canonicalize().map_err(|e| {
        format!(
            "Cannot resolve workspace path '{}': {}",
            workspace_display, e
        )
    })?;

    let kind = ProjectKind::detect(&workspace);
    let lsp_cmd = lsp_cmd_override.unwrap_or_else(|| kind.default_lsp().to_string());

    eprintln!("=== codegraph indexer ===");
    eprintln!("  workspace: {}", workspace.display());
    eprintln!("  project:   {:?} (lsp: {})", kind, lsp_cmd);
    eprintln!("  db:        {}", db_path);

    let db = Db::open(&db_path).map_err(|e| format!("Failed to open database '{db_path}': {e}"))?;

    let head_hash = git_head_hash(&workspace);
    let head_short = head_hash.chars().take(7).collect::<String>();
    let head_message = git_head_message(&workspace);

    if !head_hash.is_empty() {
        eprintln!(
            "  commit:    {} {}",
            head_short,
            head_message.chars().take(60).collect::<String>()
        );
    }

    // Sidecar metadata for incremental runs.
    let meta_path = meta::sidecar_path(&db_path);
    let prev_meta = meta::load(&meta_path);
    let last_indexed_hash = if force_full {
        None
    } else {
        prev_meta
            .as_ref()
            .map(|m| m.last_commit.clone())
            .filter(|s| !s.is_empty())
    };

    let (changed_files, is_full) = if let Some(paths) = &live_paths {
        eprintln!("  [~] Live: {} explicit path(s)", paths.len());
        (paths.clone(), false)
    } else {
        match (&last_indexed_hash, head_hash.is_empty()) {
            (Some(prev_hash), false) => {
                if *prev_hash == head_hash {
                    eprintln!("  [=] Already indexed at {}. Nothing to do.", head_short);
                    return Ok(IndexStats {
                        mode: "noop",
                        head_hash: head_hash.clone(),
                        ..Default::default()
                    });
                }
                let changed = git_changed_files(&workspace, prev_hash);
                let prev_short = prev_hash.chars().take(7).collect::<String>();
                eprintln!(
                    "  [~] Incremental: {}..{} ({} changed files)",
                    prev_short,
                    head_short,
                    changed.len()
                );
                (changed, false)
            }
            _ => {
                eprintln!("  [*] Full index (no previous commit recorded or --full)");
                (Vec::new(), true)
            }
        }
    };
    let is_live = live_paths.is_some();

    // On a full reindex wipe everything the indexer owns, in addition to the
    // per-pass wipes that happen later (BDD / Markdown / code nodes). Without
    // this, re-running --full on top of an old DB stacks duplicate Workspace /
    // Package / API* nodes via the MERGE statements.
    //
    // `:GitCommit`, `:Author` and `:Note` are intentionally excluded — they
    // form the persistent revision history and user-attached annotations,
    // and are kept across reindexes so we accumulate a real timeline.
    if is_full {
        for label in ["File", "Workspace", "Package", "APIEndpoint", "APIType"] {
            run(&db, &format!("MATCH (n:{label}) DETACH DELETE n"));
        }
    }

    // ── K8: snapshot `[:NOTES]` edges by target identity ──────────────────────
    //
    // The wipes that follow (`--full` clears all :Function/:Symbol/:File; live
    // mode clears per-file functions) use `DETACH DELETE`, which removes
    // `[:NOTES]->` edges as a side effect. The `:Note` nodes themselves
    // survive (they're not in the wipe set) but become orphaned.
    //
    // Snapshot now, restore after the LSP rebuild has re-created the targets
    // with the same identifying property — notes effectively follow renames
    // by qualified_name / path.
    let preserved_notes: Vec<(String, String, String)> = collect_preserved_notes(&db);
    if !preserved_notes.is_empty() {
        eprintln!(
            "  [+] Will preserve {} [:NOTES] edge(s) across the wipe",
            preserved_notes.len()
        );
    }

    // ── Phase 1: Workspace + Package nodes ───────────────────────────────────
    let ws_name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    let ws_root = workspace.to_string_lossy().to_string();

    run(
        &db,
        &format!(
            "MERGE (w:Workspace {{name: {}}}) SET w.root_path = {}",
            escape_str(&ws_name),
            escape_str(&ws_root),
        ),
    );

    let (_members, source_files) = match kind {
        ProjectKind::Rust => {
            let ws_toml: toml::Value = std::fs::read_to_string(workspace.join("Cargo.toml"))
                .unwrap_or_else(|e| {
                    eprintln!("Cannot read Cargo.toml: {e}");
                    std::process::exit(1);
                })
                .parse()
                .unwrap_or_else(|e| {
                    eprintln!("Cannot parse Cargo.toml: {e}");
                    std::process::exit(1);
                });
            let members = extract_members(&ws_toml, &workspace);
            index_packages(&db, &members, &workspace, &ws_name);
            let files = collect_source_files(&members, &workspace, kind);
            (members, files)
        }
        ProjectKind::Node => {
            let members = vec![workspace.to_path_buf()];
            index_node_packages(&db, &workspace, &ws_name);
            let files = collect_source_files(&members, &workspace, kind);
            (members, files)
        }
        ProjectKind::Python => {
            let members = vec![workspace.to_path_buf()];
            index_python_packages(&db, &workspace, &ws_name);
            let files = collect_source_files(&members, &workspace, kind);
            (members, files)
        }
    };

    let rs_files = source_files;

    let (files_to_index, calls_snapshot): (
        Vec<&(PathBuf, String, String)>,
        lsp_index::CallsSnapshot,
    ) = if is_full {
        eprintln!("  [*] Clearing old code nodes...");
        run(&db, "MATCH (n:Symbol) DETACH DELETE n");
        run(&db, "MATCH (n:Function) DETACH DELETE n");
        run(&db, "MATCH (n:Field) DETACH DELETE n");
        run(&db, "MATCH (n:Parameter) DETACH DELETE n");
        run(&db, "MATCH (n:Import) DETACH DELETE n");
        // Full mode is a clean rebuild — no body-hash matches possible.
        (rs_files.iter().collect(), lsp_index::CallsSnapshot::new())
    } else {
        let changed_set: HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();
        let candidates: Vec<&(PathBuf, String, String)> = rs_files
            .iter()
            .filter(|(_, rel, _)| changed_set.contains(rel.as_str()))
            .collect();
        // Perf: drop files whose on-disk content hash matches the
        // `content_hash` already stored on the `:File`. notify (and a
        // chatty rust-analyzer) fires phantom events all the time —
        // skipping unchanged files cuts a 30-file batch down to the 1–2
        // files that actually changed, killing the 45 s pass time.
        let (to_reindex, skipped_unchanged) = filter_unchanged_by_hash(&db, candidates);
        if skipped_unchanged > 0 {
            eprintln!("  [=] Skipped {skipped_unchanged} file(s) with unchanged content hash");
        }
        // Perf: snapshot (qn -> body_hash + CALLS callees) for every
        // function about to be wiped. After the LSP re-creates them
        // with fresh body_hash, the CALLS-rebuild loop replays edges
        // for unchanged-body fns without burning `outgoingCalls` round-
        // trips — typically the dominant cost.
        let snapshot = snapshot_calls_by_qn(
            &db,
            to_reindex.iter().map(|(_, rel, _)| rel.as_str()).collect(),
        );
        if !snapshot.is_empty() {
            eprintln!(
                "  [+] Snapshotted CALLS for {} fn(s) (skip LSP for unchanged bodies)",
                snapshot.len()
            );
        }

        for (_, rel_path, _) in &to_reindex {
            let p = escape_str(rel_path);
            run(
                &db,
                &format!("MATCH (f:File {{path: {p}}})<-[:DEFINED_IN]-(n) DETACH DELETE n"),
            );
            run(
                &db,
                &format!("MATCH (f:File {{path: {p}}})-[:HAS_IMPORT]->(i:Import) DETACH DELETE i"),
            );
        }

        let exts = kind.extensions();
        for changed_file in &changed_files {
            if exts
                .iter()
                .any(|e| changed_file.ends_with(&format!(".{e}")))
                && !rs_files.iter().any(|(_, rel, _)| rel == changed_file)
            {
                let p = escape_str(changed_file);
                run(
                    &db,
                    &format!("MATCH (f:File {{path: {p}}})<-[:DEFINED_IN]-(n) DETACH DELETE n"),
                );
                run(
                    &db,
                    &format!(
                        "MATCH (f:File {{path: {p}}})-[:HAS_IMPORT]->(i:Import) DETACH DELETE i"
                    ),
                );
                run(
                    &db,
                    &format!("MATCH (f:File {{path: {p}}}) DETACH DELETE f"),
                );
                eprintln!("  [-] Deleted: {}", changed_file);
            }
        }

        (to_reindex, snapshot)
    };

    eprintln!(
        "  [*] Indexing {} files via LSP ({lsp_cmd})...",
        files_to_index.len()
    );

    // ── Phase 3+4: Index files and build call graph via LSP ──────────────────
    let lsp_args = kind.lsp_args();
    let owned_files: Vec<(PathBuf, String, String)> = files_to_index
        .iter()
        .map(|(a, b, c)| (a.clone(), b.clone(), c.clone()))
        .collect();
    let (total_symbols, total_functions, call_edges) = match pool {
        Some(pool) => {
            // Pool-aware path: reuse the long-lived LSP client.
            let pc = pool
                .get_or_start(&lsp_cmd, &lsp_args, &workspace)
                .map_err(|e| {
                    format!(
                        "LSP `{lsp_cmd}` failed to start: {e}\nInstall one of: \
                         rust-analyzer, typescript-language-server (--stdio), \
                         pyright-langserver (--stdio); or pass --lsp <binary>."
                    )
                })?;
            let initial_warmup = !pc.warmed_up;
            let result = lsp_index::index_files_via_lsp(
                &db,
                &mut pc.client,
                &mut pc.opened,
                initial_warmup,
                &owned_files,
                &calls_snapshot,
                &workspace,
            );
            pc.warmed_up = true;
            // No shutdown — the pool keeps the client alive for the next pass.
            result
        }
        None => {
            // Transient path: start + shut down per call.
            let mut lsp =
                lsp::LspClient::start(&lsp_cmd, &lsp_args, &workspace).unwrap_or_else(|e| {
                    eprintln!("  [!] LSP `{lsp_cmd}` failed to start: {e}");
                    eprintln!(
                        "  [!] Install one of: rust-analyzer, typescript-language-server (--stdio), pyright-langserver (--stdio)"
                    );
                    eprintln!("  [!] Or pass --lsp <binary> to override the default for this project kind.");
                    std::process::exit(1);
                });
            let mut opened: std::collections::HashMap<PathBuf, i32> =
                std::collections::HashMap::new();
            let result = lsp_index::index_files_via_lsp(
                &db,
                &mut lsp,
                &mut opened,
                true,
                &owned_files,
                &calls_snapshot,
                &workspace,
            );
            if let Err(e) = lsp.shutdown() {
                eprintln!("  [!] LSP shutdown: {e}");
            }
            result
        }
    };

    // ── K8: restore preserved [:NOTES] edges ─────────────────────────────────
    // Re-attach each `:Note` to the current node carrying the same
    // identifying property. `MERGE` so re-runs (or notes already wired by
    // the wipe-resistant labels) stay idempotent. Targets that no longer
    // exist (renamed-away functions, deleted files) silently produce no
    // edge — the note remains as a discoverable orphan in `list_notes`.
    if !preserved_notes.is_empty() {
        let restored = restore_preserved_notes(&db, &preserved_notes);
        eprintln!(
            "  [+] Restored {restored}/{} [:NOTES] edges (orphaned: {})",
            preserved_notes.len(),
            preserved_notes.len().saturating_sub(restored),
        );
    }

    // ── Phase 4.5: API specs ────────────────────────────────────────────────
    let pkg_for_specs = match kind {
        ProjectKind::Rust => ws_name.clone(),
        _ => {
            let pkg_json = workspace.join("package.json");
            let pyproject = workspace.join("pyproject.toml");
            if pkg_json.exists() {
                std::fs::read_to_string(&pkg_json)
                    .ok()
                    .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                    .and_then(|v| v.get("name")?.as_str().map(String::from))
                    .unwrap_or_else(|| ws_name.clone())
            } else if pyproject.exists() {
                std::fs::read_to_string(&pyproject)
                    .ok()
                    .and_then(|c| c.parse::<toml::Value>().ok())
                    .and_then(|t| t.get("project")?.get("name")?.as_str().map(String::from))
                    .unwrap_or_else(|| ws_name.clone())
            } else {
                ws_name.clone()
            }
        }
    };
    let (_api_endpoints, _api_types) = api_spec::index_api_specs(&db, &workspace, &pkg_for_specs);

    // ── Phase 4.6: Scan BDD feature files (.feature) ─────────────────────────
    let (feature_count, scenario_count, step_count, step_link_count) =
        index_feature_files(&db, &workspace, &changed_files, is_full);
    if feature_count > 0 {
        eprintln!(
            "  [+] BDD: {} features / {} scenarios / {} steps ({} linked to impls)",
            feature_count, scenario_count, step_count, step_link_count
        );
    }

    // ── Phase 4.7: Markdown ↔ code linking ───────────────────────────────────
    let (docs, doc_sections, doc_mentions, doc_links) =
        markdown_index::index_markdown_files(&db, &workspace, is_full);
    if docs > 0 {
        eprintln!(
            "  [+] Markdown: {docs} docs / {doc_sections} sections / {doc_mentions} mentions / {doc_links} file-links"
        );
    }

    // ── Phase 5: GitCommit + Author history ──────────────────────────────────
    if !head_hash.is_empty() && !is_live {
        phase_history(
            &db,
            &workspace,
            &head_hash,
            &ws_name,
            last_indexed_hash.as_deref(),
            is_full,
        );
    }

    // ── Phase 6: tag tests + materialise [:TESTS] edges ──────────────────────
    phase_test_tagging(&db);

    // ── Phase 7: fire watch triggers ─────────────────────────────────────────
    phase_watch_triggers(&db, &head_hash);

    // ── Persist sidecar metadata ─────────────────────────────────────────────
    if !head_hash.is_empty() && !is_live {
        save_sidecar(&meta_path, &head_hash);
    }

    let mode = if is_live {
        "live"
    } else if is_full {
        "full"
    } else {
        "incremental"
    };
    eprintln!("\n=== Done ({mode}) ===");
    eprintln!("  Symbols:   {}", total_symbols);
    eprintln!("  Functions: {}", total_functions);
    eprintln!("  CALLS:     {}", call_edges);
    eprintln!("  DB:        {}", db_path);

    Ok(IndexStats {
        mode,
        symbols: total_symbols as usize,
        functions: total_functions as usize,
        call_edges: call_edges as usize,
        head_hash,
    })
}

// ── Phase helpers (extracted from `run_indexer_inner`) ───────────────────────

/// Phase 5 — record `:GitCommit` + `:Author` history and tag
/// `first_seen_commit` / `last_seen_commit` on `:File` and `:Function`.
///
/// Skipped in live mode by the orchestrator (uncommitted edits don't deserve
/// a `:GitCommit` node, and we'd just re-MERGE the existing HEAD on every
/// save).
///
/// Strategy:
///   * On a full reindex (or first-ever run) backfill the last
///     `HISTORY_BACKFILL_LIMIT` commits reachable from HEAD.
///   * On an incremental run walk only the commits between the previously
///     indexed HEAD and the new HEAD.
// Drop files whose on-disk content hash matches the `:File.content_hash`
// property already stored on the graph node. Returns
// `(retained, skipped_count)` — the retained set is what actually needs
// re-indexing.
//
// Best-effort: if the file can't be read or the hash query fails, we
// retain the file (no false-skip), trading slightly more re-indexing
// for guaranteed correctness on read errors.
fn filter_unchanged_by_hash<'a>(
    db: &Db,
    candidates: Vec<&'a (PathBuf, String, String)>,
) -> (Vec<&'a (PathBuf, String, String)>, usize) {
    let mut retained = Vec::with_capacity(candidates.len());
    let mut skipped = 0usize;
    for cand in candidates {
        let (abs, rel, _) = cand;
        let on_disk = match std::fs::read_to_string(abs) {
            Ok(s) => lsp_index::fnv1a_64(s.as_bytes()) as i64,
            Err(_) => {
                retained.push(cand);
                continue;
            }
        };
        let q = format!(
            "MATCH (f:File {{path: {p}}}) RETURN f.content_hash AS h LIMIT 1",
            p = escape_str(rel),
        );
        let stored: Option<i64> = db
            .query(&q)
            .ok()
            .and_then(|t| t.rows.into_iter().next())
            .and_then(|r| r.into_iter().next())
            .and_then(|c| c.as_i64());
        if stored == Some(on_disk) {
            skipped += 1;
        } else {
            retained.push(cand);
        }
    }
    (retained, skipped)
}

// Snapshot `(qualified_name -> (body_hash, [callee_names]))` for every
// `:Function` defined in any of `paths`, capturing both the body hash
// and the outgoing CALLS that exist *now*. The CALLS-rebuild loop in
// `index_files_via_lsp` uses this to skip the `outgoingCalls` LSP
// round-trip for any fn whose body hash didn't change — typical
// single-fn edits leave 9/10 fns per file untouched.
fn snapshot_calls_by_qn(db: &Db, paths: Vec<&str>) -> lsp_index::CallsSnapshot {
    let mut snap: lsp_index::CallsSnapshot = lsp_index::CallsSnapshot::new();
    if paths.is_empty() {
        return snap;
    }
    let in_list = paths
        .iter()
        .map(|p| escape_str(p))
        .collect::<Vec<_>>()
        .join(",");
    let q = format!(
        "MATCH (f:Function)-[:DEFINED_IN]->(file:File) \
         WHERE file.path IN [{in_list}] \
         OPTIONAL MATCH (f)-[:CALLS]->(c:Function) \
         RETURN f.qualified_name AS qn, f.body_hash AS h, c.name AS callee"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(_) => return snap,
    };
    for row in &t.rows {
        let qn = row
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let h = row.get(1).and_then(|c| c.as_i64()).unwrap_or(0);
        let callee = row.get(2).and_then(|c| c.as_str()).map(|s| s.to_string());
        if qn.is_empty() {
            continue;
        }
        let entry = snap.entry(qn).or_insert((h, Vec::new()));
        if let Some(name) = callee {
            entry.1.push(name);
        }
    }
    snap
}

// K8: snapshot `(note_id, target_kind, target_identity)` for every
// `[:NOTES]->` edge before a wipe. `target_kind` selects which
// property to use as the identity on restore — `qualified_name` for
// `:Function` and `:Symbol`, `path` for `:File`, `name` for
// `:Package`. Notes pointing at other labels (`:DocSection`,
// `:Workspace`, …) are not preserved by this snapshot; extend the
// kind list when a new label needs the same treatment.
fn collect_preserved_notes(db: &Db) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    for (kind, prop) in [
        ("Function", "qualified_name"),
        ("Symbol", "qualified_name"),
        ("File", "path"),
        ("Package", "name"),
    ] {
        let q = format!(
            "MATCH (n:Note)-[:NOTES]->(t:{kind}) \
             RETURN n.id AS note_id, t.{prop} AS ident"
        );
        if let Ok(t) = db.query(&q) {
            for row in &t.rows {
                let nid = row
                    .first()
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                let ident = row
                    .get(1)
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                if !nid.is_empty() && !ident.is_empty() {
                    out.push((nid, kind.to_string(), ident));
                }
            }
        }
    }
    out
}

// K8: re-attach `[:NOTES]` for every snapshotted pair whose target
// still exists post-rebuild. Returns the count of edges actually
// created.
fn restore_preserved_notes(db: &Db, preserved: &[(String, String, String)]) -> usize {
    let mut restored = 0;
    for (note_id, kind, ident) in preserved {
        let prop = match kind.as_str() {
            "Function" | "Symbol" => "qualified_name",
            "File" => "path",
            "Package" => "name",
            _ => continue,
        };
        let q = format!(
            "MATCH (n:Note {{id: {nid}}}), (t:{kind} {{{prop}: {ival}}}) \
             MERGE (n)-[:NOTES]->(t)",
            nid = escape_str(note_id),
            ival = escape_str(ident),
        );
        if db.run(&q).is_ok() {
            // Count restored only if the target still exists. Best-effort:
            // we can't distinguish "MERGE created" from "MERGE matched
            // existing" via velr's result type, so we count any successful
            // statement. The aggregate is still useful for diagnostics.
            restored += 1;
        }
    }
    restored
}

fn phase_history(
    db: &Db,
    workspace: &Path,
    head_hash: &str,
    ws_name: &str,
    last_indexed_hash: Option<&str>,
    is_full: bool,
) {
    let range = match (last_indexed_hash, is_full) {
        (Some(prev), false) if prev != head_hash => format!("{prev}..HEAD"),
        _ => format!("-n {HISTORY_BACKFILL_LIMIT}"),
    };
    let commits = git_log_range(workspace, &range);
    eprintln!(
        "  [+] History: recording {} commit{} in graph",
        commits.len(),
        if commits.len() == 1 { "" } else { "s" }
    );
    for c in &commits {
        run(
            db,
            &format!(
                "MERGE (a:Author {{email: {}}}) SET a.name = {}",
                escape_str(&c.author_email),
                escape_str(&c.author_name),
            ),
        );
        run(
            db,
            &format!(
                "MERGE (c:GitCommit {{hash: {}}}) \
                 SET c.short_hash = {}, c.message = {}, c.timestamp = {}",
                escape_str(&c.hash),
                escape_str(&c.short_hash),
                escape_str(&c.message),
                escape_str(&c.timestamp),
            ),
        );
        run(
            db,
            &format!(
                "MATCH (a:Author {{email: {}}}), (c:GitCommit {{hash: {}}}) \
                 MERGE (a)-[:AUTHORED]->(c)",
                escape_str(&c.author_email),
                escape_str(&c.hash),
            ),
        );
        for parent in &c.parents {
            run(
                db,
                &format!("MERGE (:GitCommit {{hash: {}}})", escape_str(parent),),
            );
            run(
                db,
                &format!(
                    "MATCH (p:GitCommit {{hash: {}}}), (c:GitCommit {{hash: {}}}) \
                     MERGE (p)-[:PARENT_OF]->(c)",
                    escape_str(parent),
                    escape_str(&c.hash),
                ),
            );
        }
    }
    // HEAD ↔ Workspace pointer: keep a single SNAPSHOT_OF on the head.
    run(
        db,
        &format!(
            "MATCH (c:GitCommit {{hash: {}}}), (w:Workspace {{name: {}}}) \
             MERGE (c)-[:SNAPSHOT_OF]->(w)",
            escape_str(head_hash),
            escape_str(ws_name),
        ),
    );

    // first_seen / last_seen tagging on Files and Functions.
    let head_lit = escape_str(head_hash);
    run(
        db,
        &format!("MATCH (f:File) SET f.last_seen_commit = {head_lit}"),
    );
    run(
        db,
        &format!(
            "MATCH (f:File) WHERE f.first_seen_commit IS NULL \
             SET f.first_seen_commit = {head_lit}"
        ),
    );
    run(
        db,
        &format!("MATCH (f:Function) SET f.last_seen_commit = {head_lit}"),
    );
    run(
        db,
        &format!(
            "MATCH (f:Function) WHERE f.first_seen_commit IS NULL \
             SET f.first_seen_commit = {head_lit}"
        ),
    );
}

/// Phase 6 — tag every `:Function` whose body contains `#[test]` or
/// `#[tokio::test]` with `:Test`, then materialise `[:TESTS]` from each
/// test fn to every non-test it `[:CALLS]`.
///
/// velr 0.2.16: `WHERE a OR b` rewrites to UNION which applies SET to all
/// unioned rows, defeating the filter. Two single-CONTAINS statements
/// instead.
fn phase_test_tagging(db: &Db) {
    run(
        db,
        "MATCH (f:Function) WHERE f.body CONTAINS '#[test]' SET f:Test",
    );
    run(
        db,
        "MATCH (f:Function) WHERE f.body CONTAINS '#[tokio::test]' SET f:Test",
    );
    run(db, "MATCH ()-[r:TESTS]->() DELETE r");
    run(
        db,
        "MATCH (t:Test)-[:CALLS]->(f:Function) WHERE NOT f:Test CREATE (t)-[:TESTS]->(f)",
    );
}

/// Persist sidecar metadata so the next CLI/Auto run starts from the
/// commit we just indexed. Skipped in live mode by the orchestrator —
/// the sidecar only tracks the last *committed* pass.
fn save_sidecar(meta_path: &Path, head_hash: &str) {
    if let Err(e) = meta::save(
        meta_path,
        &meta::Meta {
            last_commit: head_hash.to_string(),
            indexed_at: chrono_now_iso(),
        },
    ) {
        eprintln!("  [!] Could not write sidecar metadata: {e}");
    }
}

// ── Git helpers ──────────────────────────────────────────────────────────────

/// How many commits to backfill into the graph the first time we index a
/// repository (or on `--full`). Bounds the work; later incremental runs only
/// add commits between the previous and current HEAD.
const HISTORY_BACKFILL_LIMIT: usize = 200;

#[derive(Debug, Clone)]
struct CommitRecord {
    hash: String,
    short_hash: String,
    message: String,
    timestamp: String,
    author_name: String,
    author_email: String,
    parents: Vec<String>,
}

/// Walk the given git revision range and parse one `CommitRecord` per commit.
///
/// `range` is appended verbatim to the `git log` invocation, so it can be
/// either `"-n 50"` for the first N commits reachable from HEAD or
/// `"<prev>..HEAD"` for an incremental delta.
fn git_log_range(workspace: &Path, range: &str) -> Vec<CommitRecord> {
    // Custom format with a record separator so we can split reliably even when
    // commit messages contain embedded newlines.
    const RS: &str = "<<<CGREC>>>";
    const FS: &str = "<<<CGFLD>>>";
    let format = format!("--pretty=format:{RS}%H{FS}%h{FS}%P{FS}%an{FS}%ae{FS}%aI{FS}%s");
    let mut args: Vec<String> = vec!["log".into(), format];
    for tok in range.split_whitespace() {
        args.push(tok.to_string());
    }
    let arg_refs: Vec<&str> = std::iter::once("git")
        .chain(args.iter().map(String::as_str))
        .collect();
    let raw = cmd_output(workspace, &arg_refs);
    let mut out = Vec::new();
    for chunk in raw.split(RS) {
        let trimmed = chunk.trim_start_matches('\n').trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.splitn(7, FS).collect();
        if parts.len() < 7 {
            continue;
        }
        let parents: Vec<String> = parts[2].split_whitespace().map(|s| s.to_string()).collect();
        out.push(CommitRecord {
            hash: parts[0].to_string(),
            short_hash: parts[1].to_string(),
            parents,
            author_name: parts[3].to_string(),
            author_email: parts[4].to_string(),
            timestamp: parts[5].to_string(),
            message: parts[6].to_string(),
        });
    }
    out
}

fn git_head_hash(workspace: &Path) -> String {
    cmd_output(workspace, &["git", "rev-parse", "HEAD"])
}

fn git_head_message(workspace: &Path) -> String {
    cmd_output(workspace, &["git", "log", "-1", "--format=%s"])
}

fn git_changed_files(workspace: &Path, since_hash: &str) -> Vec<String> {
    let output = cmd_output(
        workspace,
        &["git", "diff", "--name-only", &format!("{since_hash}..HEAD")],
    );
    output
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn cmd_output(dir: &Path, args: &[&str]) -> String {
    Command::new(args[0])
        .args(&args[1..])
        .current_dir(dir)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// Now sourced from `codegraph_core::time::now_iso` — see refactoring 1b.
use codegraph_core::time::now_iso as chrono_now_iso;

// ── DB helpers ───────────────────────────────────────────────────────────────

/// Run a Cypher write and report errors to stderr. Used by every mutation site.
pub(crate) fn run(db: &Db, cypher: &str) {
    if let Err(e) = db.run(cypher) {
        eprintln!("  [!] Query failed: {}\n      {}", e, cypher);
    }
}

// ── BDD feature-file pipeline ────────────────────────────────────────────────

fn index_feature_files(
    db: &Db,
    workspace: &Path,
    changed_files: &[String],
    is_full: bool,
) -> (usize, usize, usize, usize) {
    let feature_files: Vec<(PathBuf, String)> = WalkDir::new(workspace)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "feature")
                .unwrap_or(false)
        })
        .filter(|e| !e.path().components().any(|c| c.as_os_str() == "target"))
        .map(|e| {
            let abs = e.path().to_path_buf();
            let rel = abs
                .strip_prefix(workspace)
                .unwrap_or(&abs)
                .to_string_lossy()
                .to_string();
            (abs, rel)
        })
        .collect();

    if feature_files.is_empty() {
        return (0, 0, 0, 0);
    }

    let to_index: Vec<&(PathBuf, String)> = if is_full {
        run(db, "MATCH (n:Step) DETACH DELETE n");
        run(db, "MATCH (n:Scenario) DETACH DELETE n");
        run(db, "MATCH (n:Feature) DETACH DELETE n");
        feature_files.iter().collect()
    } else {
        let changed_set: HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();
        let touched: Vec<&(PathBuf, String)> = feature_files
            .iter()
            .filter(|(_, rel)| changed_set.contains(rel.as_str()))
            .collect();
        for (_, rel) in &touched {
            let p = escape_str(rel);
            run(db, &format!(
                "MATCH (f:Feature {{file_path: {p}}})-[:HAS_SCENARIO]->(sc:Scenario)-[:HAS_STEP]->(st:Step) DETACH DELETE st"
            ));
            run(db, &format!(
                "MATCH (f:Feature {{file_path: {p}}})-[:HAS_SCENARIO]->(sc:Scenario) DETACH DELETE sc"
            ));
            run(
                db,
                &format!("MATCH (f:Feature {{file_path: {p}}}) DETACH DELETE f"),
            );
        }
        touched
    };

    let mut features = 0usize;
    let mut scenarios = 0usize;
    let mut steps = 0usize;

    for (abs, rel) in &to_index {
        let Ok(src) = std::fs::read_to_string(abs) else {
            continue;
        };
        let items = gherkin::parse_feature_file(&src, rel);
        let mut current_feature_qn: Option<String> = None;
        let mut current_scenario_qn: Option<String> = None;

        for item in items {
            match item {
                gherkin::FeatureItem::Feature {
                    name,
                    file_path,
                    line,
                    tags,
                } => {
                    let feature_qn = format!("{file_path}::{name}");
                    run(
                        db,
                        &format!(
                            "CREATE (:Feature {{qualified_name: {qn}, name: {n}, file_path: {fp}, line: {line}, tags: {tags}}})",
                            qn = escape_str(&feature_qn),
                            n = escape_str(&name),
                            fp = escape_str(&file_path),
                            tags = escape_str(&tags.join(",")),
                        ),
                    );
                    current_feature_qn = Some(feature_qn);
                    current_scenario_qn = None;
                    features += 1;
                }
                gherkin::FeatureItem::Scenario {
                    feature_name: _,
                    name,
                    line,
                    tags,
                    id: _,
                } => {
                    let Some(ref f_qn) = current_feature_qn else {
                        continue;
                    };
                    let scenario_qn = format!("{f_qn}::{name}@{line}");
                    run(
                        db,
                        &format!(
                            "CREATE (:Scenario {{qualified_name: {qn}, name: {n}, line: {line}, tags: {tags}}})",
                            qn = escape_str(&scenario_qn),
                            n = escape_str(&name),
                            tags = escape_str(&tags.join(",")),
                        ),
                    );
                    run(
                        db,
                        &format!(
                            "MATCH (f:Feature {{qualified_name: {fqn}}}), (sc:Scenario {{qualified_name: {qn}}}) CREATE (f)-[:HAS_SCENARIO]->(sc)",
                            fqn = escape_str(f_qn),
                            qn = escape_str(&scenario_qn),
                        ),
                    );
                    current_scenario_qn = Some(scenario_qn);
                    scenarios += 1;
                }
                gherkin::FeatureItem::Step {
                    scenario_id: _,
                    order,
                    kind,
                    text,
                    line,
                } => {
                    let Some(ref sc_qn) = current_scenario_qn else {
                        continue;
                    };
                    let step_qn = format!("{sc_qn}#{order}");
                    run(
                        db,
                        &format!(
                            "CREATE (:Step {{qualified_name: {qn}, kind: {k}, text: {t}, step_order: {order}, line: {line}}})",
                            qn = escape_str(&step_qn),
                            k = escape_str(&kind),
                            t = escape_str(&text),
                        ),
                    );
                    run(
                        db,
                        &format!(
                            "MATCH (sc:Scenario {{qualified_name: {scqn}}}), (st:Step {{qualified_name: {qn}}}) CREATE (sc)-[:HAS_STEP]->(st)",
                            scqn = escape_str(sc_qn),
                            qn = escape_str(&step_qn),
                        ),
                    );
                    steps += 1;
                }
            }
        }
    }

    let promoted_step_impls = promote_step_impls(db, workspace);
    if promoted_step_impls > 0 {
        eprintln!("  [+] BDD: {promoted_step_impls} step impls promoted from LSP Function nodes");
    }

    run(db, "MATCH (:Step)-[r:IMPLEMENTED_BY]->(:Function) DELETE r");

    let step_table = db
        .query("MATCH (st:Step) RETURN st.qualified_name AS qn, st.text AS text, st.kind AS kind")
        .ok();
    let fn_table = db
        .query("MATCH (fn:Function) WHERE fn.kind = 'Step' RETURN fn.qualified_name AS qn, fn.step_regex AS sr, fn.step_kind AS sk")
        .ok();

    let Some(step_t) = step_table else {
        return (features, scenarios, steps, 0);
    };
    let Some(fn_t) = fn_table else {
        return (features, scenarios, steps, 0);
    };

    let step_tuples = string_triples(&step_t, "qn", "text", "kind");
    let fn_tuples = string_triples(&fn_t, "qn", "sr", "sk");

    let compiled: Vec<(String, regex::Regex, String)> = fn_tuples
        .into_iter()
        .filter_map(|(qn, pat, kind)| Some((qn, regex::Regex::new(&pat).ok()?, kind)))
        .collect();

    let mut links = 0usize;
    for (step_qn, step_text, step_kind) in step_tuples {
        for (fn_qn, re, fn_kind) in &compiled {
            if fn_kind != &step_kind {
                continue;
            }
            if re.is_match(&step_text) {
                run(
                    db,
                    &format!(
                        "MATCH (st:Step {{qualified_name: {stqn}}}), (fn:Function {{qualified_name: {fnqn}}}) CREATE (st)-[:IMPLEMENTED_BY]->(fn)",
                        stqn = escape_str(&step_qn),
                        fnqn = escape_str(fn_qn),
                    ),
                );
                links += 1;
                break;
            }
        }
    }

    (features, scenarios, steps, links)
}

fn promote_step_impls(db: &Db, workspace: &Path) -> usize {
    let test_files = db
        .query("MATCH (f:File) WHERE f.path CONTAINS '/tests/' RETURN f.path AS path")
        .map(|t| t.column_strings("path"))
        .unwrap_or_default();

    let mut promoted = 0usize;
    for rel_path in test_files {
        let abs = workspace.join(&rel_path);
        let Ok(source) = std::fs::read_to_string(&abs) else {
            continue;
        };
        for impl_ in bdd_steps::extract_step_impls_from_file(&source) {
            run(
                db,
                &format!(
                    "MATCH (fn:Function {{name: {name}}})-[:DEFINED_IN]->(f:File {{path: {fp}}}) SET fn.kind = 'Step', fn.step_kind = {sk}, fn.step_regex = {sr}",
                    name = escape_str(&impl_.fn_name),
                    fp = escape_str(&rel_path),
                    sk = escape_str(&impl_.step_kind),
                    sr = escape_str(&impl_.step_regex),
                ),
            );
            promoted += 1;
        }
    }
    promoted
}

fn string_triples(
    t: &codegraph_core::Table,
    a: &str,
    b: &str,
    c: &str,
) -> Vec<(String, String, String)> {
    let ai = t.col(a);
    let bi = t.col(b);
    let ci = t.col(c);
    let (Some(ai), Some(bi), Some(ci)) = (ai, bi, ci) else {
        return Vec::new();
    };
    t.rows
        .iter()
        .filter_map(|row| {
            Some((
                row.get(ai)?.as_str()?.to_string(),
                row.get(bi)?.as_str()?.to_string(),
                row.get(ci)?.as_str()?.to_string(),
            ))
        })
        .collect()
}

// ── Workspace parsing ────────────────────────────────────────────────────────

fn extract_members(ws_toml: &toml::Value, ws_root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Some(members) = ws_toml
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for member in members {
            if let Some(pattern) = member.as_str() {
                let path = ws_root.join(pattern);
                if path.exists() {
                    result.push(path);
                }
            }
        }
    }
    if result.is_empty() {
        result.push(ws_root.to_path_buf());
    }
    result
}

fn index_packages(db: &Db, members: &[PathBuf], workspace: &Path, ws_name: &str) {
    for member_path in members {
        let cargo_toml = member_path.join("Cargo.toml");
        let Ok(content) = std::fs::read_to_string(&cargo_toml) else {
            continue;
        };
        let Ok(pkg_toml) = content.parse::<toml::Value>() else {
            continue;
        };

        let name = pkg_toml
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        let version = pkg_toml
            .get("package")
            .and_then(|p| p.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0");
        let edition = pkg_toml
            .get("package")
            .and_then(|p| p.get("edition"))
            .and_then(|v| v.as_str())
            .unwrap_or("2021");
        let rel_path = member_path
            .strip_prefix(workspace)
            .unwrap_or(member_path)
            .to_string_lossy()
            .to_string();

        run(
            db,
            &format!(
                "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = {pa}, p.language = 'Rust', p.edition = {e}, p.is_external = false",
                n = escape_str(name),
                v = escape_str(version),
                pa = escape_str(&rel_path),
                e = escape_str(edition),
            ),
        );
        run(
            db,
            &format!(
                "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
                ws = escape_str(ws_name),
                n = escape_str(name),
            ),
        );

        for dep_key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = pkg_toml.get(dep_key).and_then(|d| d.as_table()) {
                let kind = match dep_key {
                    "dev-dependencies" => "Dev",
                    "build-dependencies" => "Build",
                    _ => "Normal",
                };
                for (dep_name, dep_val) in deps {
                    let is_ws = dep_val.get("path").is_some()
                        || dep_val
                            .get("workspace")
                            .and_then(|w| w.as_bool())
                            .unwrap_or(false);
                    if !is_ws {
                        run(
                            db,
                            &format!(
                                "MERGE (ext:Package {{name: {n}}}) SET ext.is_external = true, ext.language = 'Rust'",
                                n = escape_str(dep_name),
                            ),
                        );
                    }
                    run(
                        db,
                        &format!(
                            "MATCH (a:Package {{name: {an}}}), (b:Package {{name: {bn}}}) CREATE (a)-[:DEPENDS_ON {{kind: {k}}}]->(b)",
                            an = escape_str(name),
                            bn = escape_str(dep_name),
                            k = escape_str(kind),
                        ),
                    );
                }
            }
        }
        eprintln!("  [+] Package: {} ({})", name, rel_path);
    }
}

fn collect_source_files(
    members: &[PathBuf],
    workspace: &Path,
    kind: ProjectKind,
) -> Vec<(PathBuf, String, String)> {
    let mut files = Vec::new();
    let extensions = kind.extensions();

    for member_path in members {
        let (src_dirs, pkg_name) = match kind {
            ProjectKind::Rust => {
                let src = member_path.join("src");
                let tests = member_path.join("tests");
                let name = std::fs::read_to_string(member_path.join("Cargo.toml"))
                    .ok()
                    .and_then(|c| c.parse::<toml::Value>().ok())
                    .and_then(|t| t.get("package")?.get("name")?.as_str().map(String::from))
                    .unwrap_or_default();
                let mut dirs = vec![src];
                if tests.is_dir() {
                    dirs.push(tests);
                }
                (dirs, name)
            }
            ProjectKind::Node => {
                let name = std::fs::read_to_string(member_path.join("package.json"))
                    .ok()
                    .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                    .and_then(|v| v.get("name")?.as_str().map(String::from))
                    .unwrap_or_else(|| {
                        member_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    });
                let mut dirs = vec![];
                for d in ["src", "lib", "app", "pages", "components", "."] {
                    let p = member_path.join(d);
                    if p.exists() {
                        dirs.push(p);
                        break;
                    }
                }
                if dirs.is_empty() {
                    dirs.push(member_path.clone());
                }
                (dirs, name)
            }
            ProjectKind::Python => {
                let name = std::fs::read_to_string(member_path.join("pyproject.toml"))
                    .ok()
                    .and_then(|c| c.parse::<toml::Value>().ok())
                    .and_then(|t| {
                        t.get("project")
                            .and_then(|p| p.get("name"))
                            .or_else(|| {
                                t.get("tool")
                                    .and_then(|t| t.get("poetry"))
                                    .and_then(|p| p.get("name"))
                            })
                            .and_then(|n| n.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| {
                        member_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    });
                let mut dirs = vec![];
                for d in ["src", "lib", "app", "."] {
                    let p = member_path.join(d);
                    if p.exists() {
                        dirs.push(p);
                        break;
                    }
                }
                if dirs.is_empty() {
                    dirs.push(member_path.clone());
                }
                (dirs, name)
            }
        };

        let skip_dirs = [
            "node_modules",
            ".git",
            "dist",
            "build",
            "target",
            "__pycache__",
            ".venv",
            "venv",
            ".tox",
            ".mypy_cache",
            ".pytest_cache",
            "egg-info",
            ".eggs",
        ];

        for src_dir in &src_dirs {
            if !src_dir.exists() {
                continue;
            }
            for entry in WalkDir::new(src_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    !path.components().any(|c| {
                        let s = c.as_os_str().to_string_lossy();
                        skip_dirs.iter().any(|d| s.as_ref() == *d || s.ends_with(d))
                    }) && e.path().extension().is_some_and(|ext| {
                        let ext_str = ext.to_string_lossy();
                        extensions.iter().any(|e| *e == ext_str.as_ref())
                    })
                })
            {
                let abs = entry.path().to_path_buf();
                let rel = abs
                    .strip_prefix(workspace)
                    .unwrap_or(&abs)
                    .to_string_lossy()
                    .to_string();
                files.push((abs, rel, pkg_name.clone()));
            }
        }
    }
    files
}

fn index_node_packages(db: &Db, workspace: &Path, ws_name: &str) {
    let pkg_path = workspace.join("package.json");
    let Ok(content) = std::fs::read_to_string(&pkg_path) else {
        eprintln!("  [!] Cannot read package.json");
        return;
    };
    let Ok(pkg): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
        eprintln!("  [!] Cannot parse package.json");
        return;
    };

    let name = pkg
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("unknown");
    let version = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");

    run(
        db,
        &format!(
            "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = '.', p.language = 'TypeScript', p.is_external = false",
            n = escape_str(name),
            v = escape_str(version),
        ),
    );
    run(
        db,
        &format!(
            "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
            ws = escape_str(ws_name),
            n = escape_str(name),
        ),
    );

    for (dep_key, kind) in [
        ("dependencies", "Normal"),
        ("devDependencies", "Dev"),
        ("peerDependencies", "Normal"),
    ] {
        if let Some(deps) = pkg.get(dep_key).and_then(|d| d.as_object()) {
            for dep_name in deps.keys() {
                run(
                    db,
                    &format!(
                        "MERGE (ext:Package {{name: {n}}}) SET ext.is_external = true, ext.language = 'TypeScript'",
                        n = escape_str(dep_name),
                    ),
                );
                run(
                    db,
                    &format!(
                        "MATCH (a:Package {{name: {an}}}), (b:Package {{name: {bn}}}) CREATE (a)-[:DEPENDS_ON {{kind: {k}}}]->(b)",
                        an = escape_str(name),
                        bn = escape_str(dep_name),
                        k = escape_str(kind),
                    ),
                );
            }
        }
    }

    if let Some(workspaces) = pkg.get("workspaces").and_then(|w| w.as_array()) {
        for ws_pattern in workspaces {
            if let Some(pattern) = ws_pattern.as_str() {
                let base = pattern.trim_end_matches("/*");
                let base_path = workspace.join(base);
                if !base_path.is_dir() {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(&base_path) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let sub_pkg = entry.path().join("package.json");
                    if !sub_pkg.exists() {
                        continue;
                    }
                    let Ok(sub_content) = std::fs::read_to_string(&sub_pkg) else {
                        continue;
                    };
                    let Ok(sub_json): Result<serde_json::Value, _> =
                        serde_json::from_str(&sub_content)
                    else {
                        continue;
                    };
                    let sub_name = sub_json
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown");
                    let sub_version = sub_json
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0.0.0");
                    let rel_path = entry
                        .path()
                        .strip_prefix(workspace)
                        .unwrap_or(&entry.path())
                        .to_string_lossy()
                        .to_string();
                    run(
                        db,
                        &format!(
                            "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = {pa}, p.language = 'TypeScript', p.is_external = false",
                            n = escape_str(sub_name),
                            v = escape_str(sub_version),
                            pa = escape_str(&rel_path),
                        ),
                    );
                    run(
                        db,
                        &format!(
                            "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
                            ws = escape_str(ws_name),
                            n = escape_str(sub_name),
                        ),
                    );
                    eprintln!("  [+] Package: {} ({})", sub_name, rel_path);
                }
            }
        }
    }
    eprintln!("  [+] Package: {} (.)", name);
}

fn index_python_packages(db: &Db, workspace: &Path, ws_name: &str) {
    let pyproject_path = workspace.join("pyproject.toml");
    let reqs_path = workspace.join("requirements.txt");

    if let Ok(content) = std::fs::read_to_string(&pyproject_path) {
        if let Ok(toml) = content.parse::<toml::Value>() {
            let name = toml
                .get("project")
                .and_then(|p| p.get("name"))
                .or_else(|| {
                    toml.get("tool")
                        .and_then(|t| t.get("poetry"))
                        .and_then(|p| p.get("name"))
                })
                .and_then(|n| n.as_str())
                .unwrap_or_else(|| {
                    workspace
                        .file_name()
                        .unwrap_or_default()
                        .to_str()
                        .unwrap_or("unknown")
                });
            let version = toml
                .get("project")
                .and_then(|p| p.get("version"))
                .or_else(|| {
                    toml.get("tool")
                        .and_then(|t| t.get("poetry"))
                        .and_then(|p| p.get("version"))
                })
                .and_then(|v| v.as_str())
                .unwrap_or("0.0.0");

            run(
                db,
                &format!(
                    "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = '.', p.language = 'Python', p.is_external = false",
                    n = escape_str(name),
                    v = escape_str(version),
                ),
            );
            run(
                db,
                &format!(
                    "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
                    ws = escape_str(ws_name),
                    n = escape_str(name),
                ),
            );

            if let Some(deps) = toml
                .get("project")
                .and_then(|p| p.get("dependencies"))
                .and_then(|d| d.as_array())
            {
                for dep in deps {
                    if let Some(dep_str) = dep.as_str() {
                        let dep_name = dep_str
                            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                            .next()
                            .unwrap_or(dep_str);
                        emit_python_dep(db, name, dep_name, "Normal");
                    }
                }
            }

            if let Some(deps) = toml
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.get("dependencies"))
                .and_then(|d| d.as_table())
            {
                for dep_name in deps.keys() {
                    if dep_name == "python" {
                        continue;
                    }
                    emit_python_dep(db, name, dep_name, "Normal");
                }
            }

            if let Some(deps) = toml
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.get("group"))
                .and_then(|g| g.get("dev"))
                .and_then(|d| d.get("dependencies"))
                .and_then(|d| d.as_table())
            {
                for dep_name in deps.keys() {
                    emit_python_dep(db, name, dep_name, "Dev");
                }
            }

            eprintln!("  [+] Package: {} (.) via pyproject.toml", name);
            return;
        }
    }

    if let Ok(content) = std::fs::read_to_string(&reqs_path) {
        let name = workspace
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        run(
            db,
            &format!(
                "MERGE (p:Package {{name: {n}}}) SET p.path = '.', p.language = 'Python', p.is_external = false",
                n = escape_str(&name),
            ),
        );
        run(
            db,
            &format!(
                "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
                ws = escape_str(ws_name),
                n = escape_str(&name),
            ),
        );
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
                continue;
            }
            let dep_name = line
                .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .next()
                .unwrap_or(line);
            if dep_name.is_empty() {
                continue;
            }
            emit_python_dep(db, &name, dep_name, "Normal");
        }
        eprintln!("  [+] Package: {} (.) via requirements.txt", name);
    }

    let _ = Value::Null;
}

fn emit_python_dep(db: &Db, pkg_name: &str, dep_name: &str, kind: &str) {
    run(
        db,
        &format!(
            "MERGE (ext:Package {{name: {n}}}) SET ext.is_external = true, ext.language = 'Python'",
            n = escape_str(dep_name),
        ),
    );
    run(
        db,
        &format!(
            "MATCH (a:Package {{name: {an}}}), (b:Package {{name: {bn}}}) CREATE (a)-[:DEPENDS_ON {{kind: {k}}}]->(b)",
            an = escape_str(pkg_name),
            bn = escape_str(dep_name),
            k = escape_str(kind),
        ),
    );
}

/// Walk every `:Watch` node, compare current `body` against
/// `watch_baseline_body`, and on mismatch attach a `:Note` describing the
/// change before re-baselining. Best-effort: a per-node failure is logged
/// to stderr but does not abort indexing.
fn phase_watch_triggers(db: &Db, head_hash: &str) {
    let q = "MATCH (w:Watch) \
             WHERE w.watch_baseline_body IS NOT NULL AND w.body IS NOT NULL \
                   AND w.body <> w.watch_baseline_body \
             RETURN w.qualified_name AS qn, w.path AS path, w.name AS name, \
                    w.watch_set_at_commit AS prev_commit \
             LIMIT 500";
    let t = match db.query(q) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  [!] watch trigger query failed: {e}");
            return;
        }
    };
    if t.rows.is_empty() {
        return;
    }
    let now = chrono_now_iso();
    let head_short = if head_hash.len() > 8 {
        &head_hash[..8]
    } else {
        head_hash
    };
    let mut fired = 0;
    for row in &t.rows {
        let qn = row.first().and_then(|c| c.as_str()).unwrap_or("");
        let path = row.get(1).and_then(|c| c.as_str()).unwrap_or("");
        let name = row.get(2).and_then(|c| c.as_str()).unwrap_or("");
        let prev = row.get(3).and_then(|c| c.as_str()).unwrap_or("");
        let prev_short = if prev.len() > 8 { &prev[..8] } else { prev };

        let identifier = if !qn.is_empty() { qn } else { name };
        if identifier.is_empty() {
            continue;
        }

        let title = format!("watch trigger: {identifier}");
        let md = format!(
            "Body of `{identifier}` changed between `{prev_short}` and `{head_short}`.\n\n\
             Path: `{path}`.\n\n\
             _Re-baseline applied automatically; this note records the diff event._"
        );
        let note_id = format!(
            "watch-{}-{}",
            identifier.replace([':', '/', ' '], "_"),
            now.replace([':', '.'], "-")
        );
        // Create the note + edge keyed off the identifier we have. Try qn first.
        let select = if !qn.is_empty() {
            format!(
                "MATCH (t:Watch {{qualified_name: {q}}})",
                q = escape_str(qn)
            )
        } else if !path.is_empty() {
            format!("MATCH (t:Watch {{path: {p}}})", p = escape_str(path))
        } else {
            format!("MATCH (t:Watch {{name: {n}}})", n = escape_str(name))
        };
        let create_q = format!(
            "{select} \
             MERGE (n:Note {{id: {id}}}) \
             SET n.title = {title}, n.author = 'codegraph-indexer', n.tags = 'watch-trigger', \
                 n.created_at = {now}, n.markdown = {md} \
             CREATE (n)-[:NOTES]->(t)",
            id = escape_str(&note_id),
            title = escape_str(&title),
            now = escape_str(&now),
            md = escape_str(&md),
        );
        if let Err(e) = db.run(&create_q) {
            eprintln!("  [!] watch trigger note for {identifier} failed: {e}");
            continue;
        }
        // Re-baseline so the next pass only fires on the next change.
        let rebase = format!(
            "{select} SET t.watch_baseline_body = t.body, t.watch_set_at_commit = {head}",
            head = escape_str(head_hash),
        );
        let _ = db.run(&rebase);
        fired += 1;
    }
    if fired > 0 {
        eprintln!("  [w] Fired {fired} watch trigger note(s)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_calls_by_qn_collects_body_hash_and_callees() {
        let db = codegraph_core::Db::open_in_memory().unwrap();
        // Seed: file with two functions; one of them calls another fn
        // (callee) defined elsewhere, plus an unrelated fn we DON'T
        // want surfaced.
        db.run("CREATE (:File {path: 'src/x.rs', content_hash: 0})")
            .unwrap();
        db.run("CREATE (:Function {qualified_name: 'm::foo', name: 'foo', body_hash: 11})")
            .unwrap();
        db.run("CREATE (:Function {qualified_name: 'm::bar', name: 'bar', body_hash: 22})")
            .unwrap();
        // Callee defined elsewhere — body_hash irrelevant, we only need its `name`.
        db.run("CREATE (:Function {qualified_name: 'lib::callee', name: 'callee'})")
            .unwrap();
        for fn_qn in ["m::foo", "m::bar"] {
            db.run(&format!(
                "MATCH (f:Function {{qualified_name: '{fn_qn}'}}), \
                       (file:File {{path: 'src/x.rs'}}) \
                 CREATE (f)-[:DEFINED_IN]->(file)"
            ))
            .unwrap();
        }
        db.run(
            "MATCH (a:Function {qualified_name: 'm::foo'}), \
                   (b:Function {qualified_name: 'lib::callee'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();

        let snap = snapshot_calls_by_qn(&db, vec!["src/x.rs"]);
        assert_eq!(snap.len(), 2);
        let (foo_h, foo_callees) = snap.get("m::foo").expect("foo missing");
        assert_eq!(*foo_h, 11);
        assert_eq!(foo_callees, &vec!["callee".to_string()]);
        let (bar_h, bar_callees) = snap.get("m::bar").expect("bar missing");
        assert_eq!(*bar_h, 22);
        assert!(bar_callees.is_empty(), "bar has no CALLS");
    }

    #[test]
    fn filter_unchanged_by_hash_drops_matches_retains_misses() {
        use std::io::Write;
        let db = codegraph_core::Db::open_in_memory().unwrap();

        // Write two temp files we can read back.
        let tmp = std::env::temp_dir().join(format!("cg-filter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let p_same = tmp.join("same.rs");
        let p_diff = tmp.join("diff.rs");
        let mut f1 = std::fs::File::create(&p_same).unwrap();
        f1.write_all(b"// stable content\n").unwrap();
        drop(f1);
        let mut f2 = std::fs::File::create(&p_diff).unwrap();
        f2.write_all(b"// new content\n").unwrap();
        drop(f2);

        // Seed :File rows: `same.rs` already has the matching content_hash;
        // `diff.rs` has a stale (different) hash.
        let same_hash = lsp_index::fnv1a_64(b"// stable content\n") as i64;
        let stale_hash = lsp_index::fnv1a_64(b"// older content\n") as i64;
        db.run(&format!(
            "CREATE (:File {{path: 'same.rs', content_hash: {same_hash}}})"
        ))
        .unwrap();
        db.run(&format!(
            "CREATE (:File {{path: 'diff.rs', content_hash: {stale_hash}}})"
        ))
        .unwrap();

        let owned = [
            (p_same.clone(), "same.rs".to_string(), "pkg".to_string()),
            (p_diff.clone(), "diff.rs".to_string(), "pkg".to_string()),
        ];
        let candidates: Vec<&(PathBuf, String, String)> = owned.iter().collect();
        let (retained, skipped) = filter_unchanged_by_hash(&db, candidates);
        assert_eq!(skipped, 1, "same.rs should be skipped");
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].1, "diff.rs");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// K8 contract: a `:Note` attached to a `:Function` should survive a
    /// `DETACH DELETE` + recreate cycle on the function, as long as the
    /// recreated function has the same `qualified_name`.
    #[test]
    fn preserved_notes_survive_function_wipe_recreate() {
        let db = codegraph_core::Db::open_in_memory().unwrap();
        // Seed: a function + a note pointing at it.
        db.run("CREATE (:Function {qualified_name: 'm::foo', name: 'foo', body: 'fn foo() {}'})")
            .unwrap();
        db.run(
            "CREATE (:Note {id: 'n-1', title: 'remember', author: 'claude', \
             markdown: 'body', tags: 't', created_at: '2026-05-16'})",
        )
        .unwrap();
        db.run(
            "MATCH (n:Note {id: 'n-1'}), (f:Function {qualified_name: 'm::foo'}) \
             CREATE (n)-[:NOTES]->(f)",
        )
        .unwrap();

        // Snapshot (mirrors what run_indexer_inner does pre-wipe).
        let preserved = collect_preserved_notes(&db);
        assert_eq!(
            preserved.len(),
            1,
            "should have snapshotted one :NOTES edge"
        );
        assert_eq!(preserved[0].1, "Function");
        assert_eq!(preserved[0].2, "m::foo");

        // Simulate the wipe + recreate (without LSP).
        db.run("MATCH (f:Function {qualified_name: 'm::foo'}) DETACH DELETE f")
            .unwrap();
        // Note still there:
        let n = db
            .query("MATCH (n:Note) RETURN count(n) AS c")
            .unwrap()
            .rows[0][0]
            .as_i64()
            .unwrap();
        assert_eq!(n, 1, "note should survive function wipe");
        // …but with no edge anywhere:
        let e = db
            .query("MATCH (:Note)-[:NOTES]->() RETURN count(*) AS c")
            .unwrap()
            .rows[0][0]
            .as_i64()
            .unwrap();
        assert_eq!(e, 0, "edge should have been nuked by DETACH DELETE");

        // Recreate the function (mirrors LSP rebuild).
        db.run("CREATE (:Function {qualified_name: 'm::foo', name: 'foo', body: 'fn foo() {}'})")
            .unwrap();

        // Restore.
        let restored = restore_preserved_notes(&db, &preserved);
        assert_eq!(restored, 1);
        let e2 = db
            .query("MATCH (:Note)-[:NOTES]->(:Function) RETURN count(*) AS c")
            .unwrap()
            .rows[0][0]
            .as_i64()
            .unwrap();
        assert_eq!(e2, 1, ":NOTES edge should be re-attached");
    }

    /// K8: notes targeting a renamed-away (now-missing) function survive as
    /// orphaned `:Note` nodes — no edge gets re-attached, but the note
    /// stays discoverable via `list_notes`.
    #[test]
    fn preserved_notes_for_deleted_target_become_orphan() {
        let db = codegraph_core::Db::open_in_memory().unwrap();
        db.run("CREATE (:Function {qualified_name: 'm::gone', name: 'gone'})")
            .unwrap();
        db.run(
            "CREATE (:Note {id: 'n-2', title: 't', author: 'a', markdown: 'm', tags: '', \
             created_at: '2026-05-16'})",
        )
        .unwrap();
        db.run(
            "MATCH (n:Note {id: 'n-2'}), (f:Function {qualified_name: 'm::gone'}) \
             CREATE (n)-[:NOTES]->(f)",
        )
        .unwrap();

        let preserved = collect_preserved_notes(&db);
        db.run("MATCH (f:Function {qualified_name: 'm::gone'}) DETACH DELETE f")
            .unwrap();
        // Don't recreate — simulating a function being renamed/deleted.
        restore_preserved_notes(&db, &preserved);

        let edges = db
            .query("MATCH (:Note)-[:NOTES]->() RETURN count(*) AS c")
            .unwrap()
            .rows[0][0]
            .as_i64()
            .unwrap();
        assert_eq!(edges, 0, "no edge should exist for missing target");
        let notes = db
            .query("MATCH (n:Note) RETURN count(n) AS c")
            .unwrap()
            .rows[0][0]
            .as_i64()
            .unwrap();
        assert_eq!(notes, 1, "the :Note node itself is still discoverable");
    }

    #[test]
    fn index_options_with_paths_switches_to_live_mode() {
        let opts = IndexOptions::new(PathBuf::from("/x"), "db")
            .with_paths(vec!["src/a.rs".into(), "src/b.rs".into()]);
        assert!(opts.path_set.is_some());
        assert_eq!(opts.path_set.as_ref().unwrap().len(), 2);
        // Sanity: the auto-mode flags are independent.
        assert!(!opts.force_full);
    }

    /// Verify the scoped CALLS wipe (used between passes in
    /// `index_files_via_lsp`) leaves CALLS originating from unchanged
    /// callers intact. The pre-fix bug was that an unconditional global
    /// DELETE wiped the whole call graph on every incremental pass.
    #[test]
    fn scoped_calls_wipe_preserves_unchanged_callers() {
        let db = codegraph_core::Db::open_in_memory().unwrap();
        for n in ["caller_changed", "caller_unchanged", "callee_a", "callee_b"] {
            db.run(&format!(
                "CREATE (:Function {{qualified_name: 'm::{n}', name: '{n}'}})"
            ))
            .unwrap();
        }
        for (a, b) in [
            ("caller_changed", "callee_a"),
            ("caller_unchanged", "callee_b"),
        ] {
            db.run(&format!(
                "MATCH (a:Function {{qualified_name: 'm::{a}'}}), \
                       (b:Function {{qualified_name: 'm::{b}'}}) \
                 CREATE (a)-[:CALLS]->(b)"
            ))
            .unwrap();
        }

        // Mimic the scoped wipe: only `caller_changed` is in the current pass.
        let in_list = format!("'{}'", "m::caller_changed");
        db.run(&format!(
            "MATCH (a:Function)-[c:CALLS]->(b:Function) \
             WHERE a.qualified_name IN [{in_list}] DELETE c"
        ))
        .unwrap();

        let t = db
            .query("MATCH (a)-[:CALLS]->(b) RETURN a.qualified_name AS a, b.qualified_name AS b")
            .unwrap();
        assert_eq!(t.rows.len(), 1, "exactly one CALLS edge should survive");
        assert_eq!(
            t.rows[0][0].as_str().unwrap_or(""),
            "m::caller_unchanged",
            "unchanged caller's CALLS should remain"
        );
    }

    #[test]
    fn lsp_pool_starts_empty() {
        let pool = LspPool::new();
        assert_eq!(pool.live_count(), 0);
        assert!(pool.live_commands().is_empty());
    }

    #[test]
    fn lsp_pool_remembers_failed_commands() {
        let mut pool = LspPool::new();
        let r1 = pool.get_or_start(
            "definitely-not-a-real-lsp-binary-xyz",
            &[],
            std::path::Path::new("/tmp"),
        );
        assert!(r1.is_err());
        // Second attempt short-circuits without trying to spawn again.
        let r2 = pool.get_or_start(
            "definitely-not-a-real-lsp-binary-xyz",
            &[],
            std::path::Path::new("/tmp"),
        );
        let msg = match r2 {
            Err(e) => e,
            Ok(_) => panic!("expected error, got Ok"),
        };
        assert!(msg.contains("previously failed to start"), "got: {msg}");
        assert_eq!(pool.live_count(), 0);
    }

    /// Build a tiny throw-away git repo with two commits and verify the
    /// log-range parser returns both, with parents wired up.
    #[test]
    fn git_log_range_parses_commits_and_parents() {
        let tmp =
            std::env::temp_dir().join(format!("codegraph-history-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&tmp)
                .output()
                .unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(tmp.join("a.txt"), "1").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(tmp.join("a.txt"), "2").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "second"]);

        let commits = git_log_range(&tmp, "-n 10");
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(commits.len(), 2, "got: {commits:#?}");
        // Newest first.
        assert_eq!(commits[0].message, "second");
        assert_eq!(commits[1].message, "first");
        // Second commit has the first as its parent.
        assert_eq!(commits[0].parents, vec![commits[1].hash.clone()]);
        // Initial commit has no parent.
        assert!(commits[1].parents.is_empty());
        for c in &commits {
            assert_eq!(c.author_email, "test@example.com");
            assert!(!c.short_hash.is_empty() && c.short_hash.len() < c.hash.len());
        }
    }

    /// Verify the Phase-6 Cypher tags `:Test` and materialises `[:TESTS]`
    /// edges from a body-content heuristic, mirroring what runs at the end
    /// of `main`.
    #[test]
    fn phase6_tags_tests_and_links_them() {
        let db = codegraph_core::Db::open_in_memory().unwrap();
        // A test fn (with the attribute in the body), a regular fn, and a
        // tokio test.
        db.run(
            "CREATE (:Function {qualified_name: 'm::test_foo', name: 'test_foo', \
             body: '#[test]\\nfn test_foo() { foo(); }'})",
        )
        .unwrap();
        db.run("CREATE (:Function {qualified_name: 'm::foo', name: 'foo', body: 'fn foo() {}'})")
            .unwrap();
        db.run(
            "CREATE (:Function {qualified_name: 'm::test_async', name: 'test_async', \
             body: '#[tokio::test]\\nasync fn test_async() { foo(); }'})",
        )
        .unwrap();
        // The CALLS edges that the LSP pass would have produced.
        db.run(
            "MATCH (a:Function {qualified_name: 'm::test_foo'}), \
                   (b:Function {qualified_name: 'm::foo'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();
        db.run(
            "MATCH (a:Function {qualified_name: 'm::test_async'}), \
                   (b:Function {qualified_name: 'm::foo'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();

        // Drive the actual phase function — this is the contract test.
        phase_test_tagging(&db);

        // Both test fns now carry the :Test label.
        let t = db
            .query("MATCH (n:Test) RETURN n.qualified_name AS qn ORDER BY qn")
            .unwrap();
        let names: Vec<String> = t
            .rows
            .iter()
            .filter_map(|r| r.first().and_then(|c| c.as_str()).map(str::to_string))
            .collect();
        assert_eq!(
            names,
            vec!["m::test_async".to_string(), "m::test_foo".into()]
        );

        // foo received TESTS edges from both tests; nothing else.
        let t = db
            .query("MATCH (a)-[:TESTS]->(b) RETURN a.qualified_name AS a, b.qualified_name AS b ORDER BY a")
            .unwrap();
        assert_eq!(t.rows.len(), 2);
    }

    /// Watch trigger fires when body differs from baseline; subsequent runs
    /// without further change are no-ops (re-baseline took effect).
    #[test]
    fn phase_watch_triggers_creates_note_and_rebaselines() {
        let db = codegraph_core::Db::open_in_memory().unwrap();
        // A watched function with mismatched body / baseline.
        db.run(
            "CREATE (:Function:Watch {qualified_name: 'm::foo', name: 'foo', \
             body: 'fn foo() { 2 }', watch_baseline_body: 'fn foo() { 1 }', \
             watch_set_at_commit: 'aaa11111'})",
        )
        .unwrap();
        // An unrelated unwatched function — must not produce a note.
        db.run("CREATE (:Function {qualified_name: 'm::bar', name: 'bar', body: 'fn bar() {}'})")
            .unwrap();

        phase_watch_triggers(&db, "bbb22222");

        // Note attached to foo
        let t = db
            .query("MATCH (n:Note)-[:NOTES]->(f:Function {qualified_name: 'm::foo'}) RETURN n.title AS title, n.tags AS tags")
            .unwrap();
        assert_eq!(t.rows.len(), 1, "expected exactly one trigger note");
        assert!(t.rows[0][0]
            .as_str()
            .unwrap_or("")
            .contains("watch trigger"));
        assert_eq!(t.rows[0][1].as_str().unwrap_or(""), "watch-trigger");

        // Baseline was updated to current body.
        let t2 = db
            .query(
                "MATCH (f:Function {qualified_name: 'm::foo'}) RETURN f.watch_baseline_body AS b",
            )
            .unwrap();
        assert_eq!(t2.rows[0][0].as_str().unwrap_or(""), "fn foo() { 2 }");

        // Second run with no further change ⇒ no new notes.
        phase_watch_triggers(&db, "ccc33333");
        let t3 = db
            .query("MATCH (n:Note)-[:NOTES]->(f:Function {qualified_name: 'm::foo'}) RETURN count(n) AS c")
            .unwrap();
        assert_eq!(t3.rows[0][0].as_i64().unwrap(), 1);
    }
}
