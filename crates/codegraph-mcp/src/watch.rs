//! `--watch` mode subsystem: notify-based filesystem watcher, status
//! mirror, and the `index_status` MCP tool. Owned by the watcher thread
//! the MCP server's `main` spawns when given `--watch <workspace>`.
//!
//! Conceptually independent of the JSON-RPC dispatch, so it lives in its
//! own file. The shared `SharedStatus` is the only thing the dispatcher
//! needs to read from this module.

use serde_json::Value;

use crate::util::{chrono_now_iso, ok_text};

// ── Path filter ──────────────────────────────────────────────────────────────

/// True for paths the indexer cares about (Rust / TS / Py / Markdown /
/// Gherkin / API specs). Drops obvious noise (editor swap files, vendored
/// trees) **and** the indexer's own outputs to break the feedback loop
/// where a successful run writes the sidecar and re-fires the watcher.
pub fn is_indexable_event_path(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    if s.contains("/.git/")
        || s.contains("/target/")
        || s.contains("/node_modules/")
        || s.contains("/.cargo/")
        || s.ends_with('~')
        || s.ends_with(".swp")
        || s.ends_with(".swx")
        || s.ends_with(".tmp")
    {
        return false;
    }
    if s.ends_with(".codegraph-meta.json")
        || s.ends_with(".db")
        || s.ends_with(".db-wal")
        || s.ends_with(".db-shm")
    {
        return false;
    }
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some(
            "rs" | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "md"
                | "feature"
                | "yaml"
                | "yml"
                | "json"
                | "graphql"
                | "proto"
                | "toml"
        )
    )
}

/// Convert a sequence of absolute paths into workspace-relative path
/// strings. Paths outside the workspace are dropped.
pub fn rel_paths_from(workspace: &std::path::Path, abs: &[std::path::PathBuf]) -> Vec<String> {
    abs.iter()
        .filter_map(|p| p.strip_prefix(workspace).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

// ── Status mirror ────────────────────────────────────────────────────────────

/// Live status of the background indexer thread, observable via the
/// `index_status` MCP tool. Wrapped in a `Mutex` so the dispatch loop and
/// the watcher thread can both touch it.
#[derive(Clone, Default)]
pub struct IndexStatus {
    /// `"idle"` | `"starting"` | `"running"`
    pub state: String,
    /// ISO-8601 timestamp of the last completed run.
    pub last_run_at: String,
    /// `"live"` | `"incremental"` | `"full"` | `"noop"` | `""` (never run).
    pub last_run_mode: String,
    pub last_run_duration_ms: u64,
    /// Workspace-relative paths from the most recent debounced batch.
    /// Capped — the agent uses these to confirm the right files were picked up.
    pub last_paths: Vec<String>,
    pub last_error: String,
    pub runs_total: u64,
    /// HEAD commit hash from the last successful run (or empty).
    pub head_hash: String,
    /// Names of LSP commands currently held alive by the pool.
    pub live_lsps: Vec<String>,
}

pub type SharedStatus = std::sync::Arc<std::sync::Mutex<IndexStatus>>;

pub fn new_shared_status() -> SharedStatus {
    std::sync::Arc::new(std::sync::Mutex::new(IndexStatus {
        state: "idle".to_string(),
        ..Default::default()
    }))
}

// ── index_status tool handler ────────────────────────────────────────────────

/// Render the live indexer status as Markdown. Safe to call whether or
/// not `--watch` was supplied: without a watcher, returns a stub making
/// the no-op explicit so an LLM doesn't poll forever.
pub fn handle_index_status(status: &SharedStatus, watch_path: Option<&str>) -> Value {
    let snap = match status.lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    };
    let mut out = String::new();
    out.push_str("# Indexer status\n\n");
    match watch_path {
        None => {
            out.push_str("_Live indexer is **not running** — start the MCP server with `--watch <workspace>` to enable._\n");
        }
        Some(ws) => {
            out.push_str(&format!("- **Watching:** `{ws}`\n"));
            out.push_str(&format!("- **State:** `{}`\n", snap.state));
            out.push_str(&format!("- **Runs total:** {}\n", snap.runs_total));
            if !snap.last_run_at.is_empty() {
                out.push_str(&format!("- **Last run at:** `{}`\n", snap.last_run_at));
                out.push_str(&format!("- **Last mode:** `{}`\n", snap.last_run_mode));
                out.push_str(&format!(
                    "- **Last duration:** {}ms\n",
                    snap.last_run_duration_ms
                ));
                if !snap.head_hash.is_empty() {
                    let short = if snap.head_hash.len() > 8 {
                        &snap.head_hash[..8]
                    } else {
                        &snap.head_hash
                    };
                    out.push_str(&format!("- **HEAD at last run:** `{short}`\n"));
                }
            } else {
                out.push_str("- _no runs yet — waiting for the first file change_\n");
            }
            if !snap.live_lsps.is_empty() {
                out.push_str(&format!(
                    "- **Persistent LSP processes:** {} (`{}`)\n",
                    snap.live_lsps.len(),
                    snap.live_lsps.join("`, `")
                ));
            }
            if !snap.last_paths.is_empty() {
                out.push_str("\n**Last batch paths**\n\n");
                for p in &snap.last_paths {
                    out.push_str(&format!("- `{p}`\n"));
                }
            }
            if !snap.last_error.is_empty() {
                out.push_str(&format!("\n**Last error:** `{}`\n", snap.last_error));
            }
        }
    }
    ok_text(out.trim_end().to_string())
}

// ── Watcher thread ───────────────────────────────────────────────────────────

/// Spawn a background thread that watches `workspace` recursively and, on
/// every debounced batch of indexable file changes, runs the indexer in
/// **live mode** against the changed paths only. Status is mirrored into
/// the shared `SharedStatus` so the `index_status` MCP tool can report it.
///
/// Live mode skips git history (no `:GitCommit`/`:Author` writes) and the
/// sidecar metadata bump, so uncommitted edits show up as a draft overlay
/// without polluting the persistent revision history. The MCP server's
/// `db_mtime`-based reopen logic picks up the new graph state on the next
/// tool call.
pub fn spawn_indexer_watcher(
    workspace: String,
    db_path: String,
    debounce_ms: u64,
    status: SharedStatus,
) {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    std::thread::spawn(move || {
        // One LspPool per watcher thread — owned here so its Drop fires when
        // the thread exits (server shutdown). Each language server is started
        // lazily on first need and reused across every subsequent batch.
        let mut lsp_pool = codegraph_indexer::LspPool::new();
        let workspace_path = std::path::PathBuf::from(&workspace);
        let canonical = match workspace_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[watch] cannot resolve workspace '{workspace}': {e}");
                if let Ok(mut s) = status.lock() {
                    s.last_error = format!("workspace canonicalize failed: {e}");
                }
                return;
            }
        };
        eprintln!(
            "[watch] watching {} (debounce {}ms, live mode)",
            canonical.display(),
            debounce_ms
        );

        let (tx, rx) = channel::<notify::Result<notify::Event>>();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[watch] could not create watcher: {e}");
                if let Ok(mut s) = status.lock() {
                    s.last_error = format!("watcher create failed: {e}");
                }
                return;
            }
        };
        if let Err(e) = watcher.watch(&canonical, RecursiveMode::Recursive) {
            eprintln!("[watch] could not start watching: {e}");
            if let Ok(mut s) = status.lock() {
                s.last_error = format!("watch start failed: {e}");
            }
            return;
        }
        // Keep the watcher alive for the duration of this thread.
        let _watcher_keepalive = watcher;

        let debounce = Duration::from_millis(debounce_ms);
        loop {
            // Block until the first event of a new batch.
            let first = match rx.recv() {
                Ok(ev) => ev,
                Err(_) => return, // sender dropped, exit thread
            };
            let mut batch = vec![first];
            // Drain the channel for `debounce` ms so a flurry of writes coalesces.
            let deadline = Instant::now() + debounce;
            loop {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or_default();
                match rx.recv_timeout(remaining) {
                    Ok(ev) => batch.push(ev),
                    Err(_) => break,
                }
            }
            // Collect the relevant paths from this batch.
            let mut indexable_abs: std::collections::BTreeSet<std::path::PathBuf> =
                std::collections::BTreeSet::new();
            for ev in batch.iter().flatten() {
                for p in &ev.paths {
                    if is_indexable_event_path(p) {
                        indexable_abs.insert(p.clone());
                    }
                }
            }
            if indexable_abs.is_empty() {
                continue;
            }
            let abs_vec: Vec<std::path::PathBuf> = indexable_abs.into_iter().collect();
            let rel_paths = rel_paths_from(&canonical, &abs_vec);
            eprintln!(
                "[watch] {} indexable file(s) changed: {}",
                rel_paths.len(),
                rel_paths
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            if let Ok(mut s) = status.lock() {
                s.state = "running".to_string();
                s.last_paths = rel_paths.iter().take(20).cloned().collect();
            }
            let started = Instant::now();
            let opts = codegraph_indexer::IndexOptions::new(canonical.clone(), &db_path)
                .with_paths(rel_paths.clone());
            // Persistent LSP pool: each language server pays its cold-start
            // cost on the first batch only. Subsequent batches reuse the
            // live process, send `didChange` for known files, and skip
            // most of the warm-up sleep.
            let result = codegraph_indexer::run_indexer_with_pool(opts, &mut lsp_pool);
            let elapsed = started.elapsed();
            if let Ok(mut s) = status.lock() {
                s.live_lsps = lsp_pool.live_commands();
            }

            if let Ok(mut s) = status.lock() {
                s.state = "idle".to_string();
                s.last_run_at = chrono_now_iso();
                s.last_run_duration_ms = elapsed.as_millis() as u64;
                s.runs_total = s.runs_total.saturating_add(1);
                match &result {
                    Ok(stats) => {
                        s.last_run_mode = stats.mode.to_string();
                        s.head_hash = stats.head_hash.clone();
                        s.last_error.clear();
                    }
                    Err(e) => {
                        s.last_run_mode = "error".to_string();
                        s.last_error = e.clone();
                    }
                }
            }
            match result {
                Ok(stats) => eprintln!(
                    "[watch] reindex done in {}ms: mode={} symbols={} functions={} calls={}",
                    elapsed.as_millis(),
                    stats.mode,
                    stats.symbols,
                    stats.functions,
                    stats.call_edges
                ),
                Err(e) => eprintln!("[watch] reindex failed: {e}"),
            }
        }
    });
}
