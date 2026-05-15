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
mod lsp;
mod lsp_index;
mod markdown_index;
mod meta;

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

const HELP: &str = "\
codegraph-indexer — projects a codebase into a velr graph database

USAGE:
    codegraph-indexer [OPTIONS]

OPTIONS:
    --workspace <path>   Project root to index (default: .)
    --db        <path>   velr database file to write to (default: code-graph.db)
    --lsp       <bin>    Override the language-server binary
    --full               Force a full re-index (ignore the sidecar metadata)
    -h, --help           Show this help and exit
    -V, --version        Print version and exit

The first run on a fresh DB does a full index; subsequent runs use git diff
between the last-indexed commit (recorded in <db>.codegraph-meta.json) and
HEAD to re-parse only changed files.
";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("codegraph-indexer {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let workspace_path = flag(&args, "--workspace").unwrap_or_else(|| ".".to_string());
    let db_path = flag(&args, "--db").unwrap_or_else(|| "code-graph.db".to_string());
    let force_full = args.iter().any(|a| a == "--full");
    let lsp_cmd_override = flag(&args, "--lsp");

    let workspace = Path::new(&workspace_path)
        .canonicalize()
        .unwrap_or_else(|e| {
            eprintln!("Cannot resolve workspace path '{}': {}", workspace_path, e);
            std::process::exit(1);
        });

    let kind = ProjectKind::detect(&workspace);
    let lsp_cmd = lsp_cmd_override.unwrap_or_else(|| kind.default_lsp().to_string());

    eprintln!("=== codegraph indexer ===");
    eprintln!("  workspace: {}", workspace.display());
    eprintln!("  project:   {:?} (lsp: {})", kind, lsp_cmd);
    eprintln!("  db:        {}", db_path);

    let db = Db::open(&db_path).unwrap_or_else(|e| {
        eprintln!("Failed to open database '{}': {}", db_path, e);
        std::process::exit(1);
    });

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

    let (changed_files, is_full) = match (&last_indexed_hash, head_hash.is_empty()) {
        (Some(prev_hash), false) => {
            if *prev_hash == head_hash {
                eprintln!("  [=] Already indexed at {}. Nothing to do.", head_short);
                return;
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
    };

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

    let files_to_index: Vec<&(PathBuf, String, String)> = if is_full {
        eprintln!("  [*] Clearing old code nodes...");
        run(&db, "MATCH (n:Symbol) DETACH DELETE n");
        run(&db, "MATCH (n:Function) DETACH DELETE n");
        run(&db, "MATCH (n:Field) DETACH DELETE n");
        run(&db, "MATCH (n:Parameter) DETACH DELETE n");
        run(&db, "MATCH (n:Import) DETACH DELETE n");
        rs_files.iter().collect()
    } else {
        let changed_set: HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();
        let to_reindex: Vec<&(PathBuf, String, String)> = rs_files
            .iter()
            .filter(|(_, rel, _)| changed_set.contains(rel.as_str()))
            .collect();

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

        to_reindex
    };

    eprintln!(
        "  [*] Indexing {} files via LSP ({lsp_cmd})...",
        files_to_index.len()
    );

    // ── Phase 3+4: Index files and build call graph via LSP ──────────────────
    let lsp_args = kind.lsp_args();
    let mut lsp = lsp::LspClient::start(&lsp_cmd, &lsp_args, &workspace).unwrap_or_else(|e| {
        eprintln!("  [!] LSP `{lsp_cmd}` failed to start: {e}");
        eprintln!(
            "  [!] Install one of: rust-analyzer, typescript-language-server (--stdio), pyright-langserver (--stdio)"
        );
        eprintln!("  [!] Or pass --lsp <binary> to override the default for this project kind.");
        std::process::exit(1);
    });
    let owned_files: Vec<(PathBuf, String, String)> = files_to_index
        .iter()
        .map(|(a, b, c)| (a.clone(), b.clone(), c.clone()))
        .collect();
    let (total_symbols, total_functions, call_edges) =
        lsp_index::index_files_via_lsp(&db, &mut lsp, &owned_files, &workspace);
    if let Err(e) = lsp.shutdown() {
        eprintln!("  [!] LSP shutdown: {e}");
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
    //
    // Strategy:
    //   * On a full reindex (or first-ever run) we backfill the last
    //     `HISTORY_BACKFILL_LIMIT` commits reachable from HEAD.
    //   * On an incremental run we walk only the commits between the
    //     previously indexed HEAD and the new HEAD.
    // Each commit becomes a `:GitCommit`, linked to its `:Author`, the
    // `:Workspace`, and to its parents via `:PARENT_OF`. Re-using `MERGE`
    // makes the operation idempotent across runs.
    if !head_hash.is_empty() {
        let range = match (&last_indexed_hash, is_full) {
            (Some(prev), false) if prev != &head_hash => format!("{prev}..HEAD"),
            _ => format!("-n {HISTORY_BACKFILL_LIMIT}"),
        };
        let commits = git_log_range(&workspace, &range);
        eprintln!(
            "  [+] History: recording {} commit{} in graph",
            commits.len(),
            if commits.len() == 1 { "" } else { "s" }
        );
        for c in &commits {
            run(
                &db,
                &format!(
                    "MERGE (a:Author {{email: {}}}) SET a.name = {}",
                    escape_str(&c.author_email),
                    escape_str(&c.author_name),
                ),
            );
            run(
                &db,
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
                &db,
                &format!(
                    "MATCH (a:Author {{email: {}}}), (c:GitCommit {{hash: {}}}) \
                     MERGE (a)-[:AUTHORED]->(c)",
                    escape_str(&c.author_email),
                    escape_str(&c.hash),
                ),
            );
            for parent in &c.parents {
                // We may not have inserted the parent yet; create a stub.
                run(
                    &db,
                    &format!("MERGE (:GitCommit {{hash: {}}})", escape_str(parent),),
                );
                run(
                    &db,
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
            &db,
            &format!(
                "MATCH (c:GitCommit {{hash: {}}}), (w:Workspace {{name: {}}}) \
                 MERGE (c)-[:SNAPSHOT_OF]->(w)",
                escape_str(&head_hash),
                escape_str(&ws_name),
            ),
        );

        // first_seen / last_seen tagging on Files and Functions.
        // last_seen is updated unconditionally; first_seen is only set if absent.
        let head_lit = escape_str(&head_hash);
        run(
            &db,
            &format!("MATCH (f:File) SET f.last_seen_commit = {head_lit}"),
        );
        run(
            &db,
            &format!(
                "MATCH (f:File) WHERE f.first_seen_commit IS NULL \
                 SET f.first_seen_commit = {head_lit}"
            ),
        );
        run(
            &db,
            &format!("MATCH (f:Function) SET f.last_seen_commit = {head_lit}"),
        );
        run(
            &db,
            &format!(
                "MATCH (f:Function) WHERE f.first_seen_commit IS NULL \
                 SET f.first_seen_commit = {head_lit}"
            ),
        );
    }

    // ── Phase 6: tag tests + materialise [:TESTS] edges ──────────────────────
    //
    // Heuristic: a `:Function` whose body contains `#[test]` or
    // `#[tokio::test]` (LSP `documentSymbol` ranges include the attributes)
    // is also a `:Test`. Then for every existing `[:CALLS]` edge from a test
    // to a non-test, materialise a `[:TESTS]` edge. The result is a
    // partial-but-honest test↔code wiring derivable purely from existing
    // index data — no new parser passes.
    {
        // velr 0.2.16: `WHERE a OR b` rewrites to UNION which applies SET
        // to *all* unioned rows, defeating the filter. Split into two stmts.
        run(
            &db,
            "MATCH (f:Function) WHERE f.body CONTAINS '#[test]' SET f:Test",
        );
        run(
            &db,
            "MATCH (f:Function) WHERE f.body CONTAINS '#[tokio::test]' SET f:Test",
        );
        // Wipe stale TESTS edges before re-deriving.
        run(&db, "MATCH ()-[r:TESTS]->() DELETE r");
        run(
            &db,
            "MATCH (t:Test)-[:CALLS]->(f:Function) WHERE NOT f:Test CREATE (t)-[:TESTS]->(f)",
        );
    }

    // ── Persist sidecar metadata ─────────────────────────────────────────────
    if !head_hash.is_empty() {
        if let Err(e) = meta::save(
            &meta_path,
            &meta::Meta {
                last_commit: head_hash.clone(),
                indexed_at: chrono_now_iso(),
            },
        ) {
            eprintln!("  [!] Could not write sidecar metadata: {e}");
        }
    }

    let mode = if is_full { "full" } else { "incremental" };
    eprintln!("\n=== Done ({mode}) ===");
    eprintln!("  Symbols:   {}", total_symbols);
    eprintln!("  Functions: {}", total_functions);
    eprintln!("  CALLS:     {}", call_edges);
    eprintln!("  DB:        {}", db_path);
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

/// Lightweight ISO-8601 timestamp without pulling in `chrono`.
fn chrono_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}Z", iso_from_unix(secs))
}

fn iso_from_unix(secs: u64) -> String {
    // Civil date conversion (Howard Hinnant). Avoids an external deps for one timestamp.
    let z = secs as i64 / 86400 + 719468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let secs_of_day = secs % 86400;
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", y, m, d, hh, mm, ss)
}

// ── DB helpers ───────────────────────────────────────────────────────────────

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .zip(args.iter().skip(1))
        .find(|(f, _)| f.as_str() == name)
        .map(|(_, v)| v.clone())
}

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

#[cfg(test)]
mod tests {
    use super::*;

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

        // Phase-6 statements (verbatim from main). The OR is split into two
        // statements because velr 0.2.16 rewrites `WHERE a OR b` into a UNION
        // and applies SET to all rows of the unioned result.
        db.run("MATCH (f:Function) WHERE f.body CONTAINS '#[test]' SET f:Test")
            .unwrap();
        db.run("MATCH (f:Function) WHERE f.body CONTAINS '#[tokio::test]' SET f:Test")
            .unwrap();
        db.run("MATCH ()-[r:TESTS]->() DELETE r").unwrap();
        db.run("MATCH (t:Test)-[:CALLS]->(f:Function) WHERE NOT f:Test CREATE (t)-[:TESTS]->(f)")
            .unwrap();

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
}
