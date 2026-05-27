//! `--watch` mode subsystem: notify-based filesystem watcher, status
//! mirror, and the `index_status` MCP tool. Owned by the watcher thread
//! the MCP server's `main` spawns when given `--watch <workspace>`.
//!
//! Conceptually independent of the JSON-RPC dispatch, so it lives in its
//! own file. The shared `SharedStatus` is the only thing the dispatcher
//! needs to read from this module.

use serde_json::Value;

use crate::tx::TxState;
use crate::util::{chrono_now_iso, ok_text};

/// Format a byte count with a binary-suffix unit (KiB, MiB, GiB) so the
/// `index_status` lines stay readable when the DB has grown past trivial
/// sizes. Sub-KiB stays in bytes so the unit is never misleading.
fn fmt_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

/// Read the file sizes of the velr DB plus its WAL / SHM sidecars. Missing
/// files report as 0 — they may not exist yet for a fresh DB, or may be
/// rolled away after a checkpoint. Returns `(db, wal, shm)` in bytes.
fn db_file_sizes(db_path: &str) -> (u64, u64, u64) {
    let len_of = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let db = len_of(db_path);
    let wal = len_of(&format!("{db_path}-wal"));
    let shm = len_of(&format!("{db_path}-shm"));
    (db, wal, shm)
}

/// WAL size at which we flag the DB section with a WARNING marker.
/// The 20 GB eskalations we hit are nowhere near this — the warning is
/// meant to fire while the bug is still recoverable, not after.
const WAL_WARN_BYTES: u64 = 100 * 1024 * 1024;
/// Buffered-tx age at which we flag the transaction as probably leaked.
/// 30 s is comfortably above any honest agent workflow.
const TX_WARN_SECS: u64 = 30;

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
    /// Total indexable file-change events accepted by the watcher
    /// since the server started. Increments live as events arrive,
    /// independent of pass boundaries — when `state` is `running` and
    /// this number is going up, the watcher is *receiving* edits even
    /// if the current pass hasn't finished yet.
    pub events_total: u64,
    /// Indexable file-change events received since the most recent
    /// completed pass. When `state` is `running` and this number is
    /// high, edits are stacking up behind the current pass and will
    /// land in the next batch. When `state` is `idle` and this is
    /// zero, the graph is fully caught up.
    pub events_pending: u64,
    /// Workspace-relative paths from events received since the most
    /// recent completed pass, capped. Confirms which files are queued
    /// while a pass is in flight.
    pub pending_paths: Vec<String>,
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
pub fn handle_index_status(
    status: &SharedStatus,
    watch_path: Option<&str>,
    tx: &TxState,
    db_path: &str,
) -> Value {
    let snap = match status.lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    };
    let mut out = String::new();
    out.push_str("# Indexer status\n\n");

    // DB / WAL section always shown, even without --watch — the bloat
    // bug is independent of the watcher and the size telemetry is what
    // turns "DB is huge" from a vague feeling into a number.
    let (db_size, wal_size, shm_size) = db_file_sizes(db_path);
    let wal_warn = wal_size > WAL_WARN_BYTES;
    out.push_str("## Database files\n\n");
    out.push_str(&format!("- **DB:** `{db_path}` ({})\n", fmt_bytes(db_size)));
    out.push_str(&format!(
        "- **WAL:** {}{}\n",
        fmt_bytes(wal_size),
        if wal_warn {
            "  ⚠ over 100 MiB — possible long-open transaction or missing checkpoint"
        } else {
            ""
        }
    ));
    out.push_str(&format!("- **SHM:** {}\n", fmt_bytes(shm_size)));
    if let Some(info) = tx.info() {
        let warn = if info.age_secs >= TX_WARN_SECS {
            "  ⚠ likely leaked — call `commit` or `rollback`"
        } else {
            ""
        };
        let msg = info
            .message
            .as_deref()
            .map(|m| format!(" — message: `{m}`"))
            .unwrap_or_default();
        out.push_str(&format!(
            "- **Open buffered tx:** tx#{}, {}s old, {} queries pending{msg}{warn}\n",
            info.tx_id, info.age_secs, info.pending
        ));
    } else {
        out.push_str("- **Open buffered tx:** none\n");
    }
    out.push('\n');

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
            out.push_str(&format!(
                "- **Events:** {} total, {} pending behind current/last batch\n",
                snap.events_total, snap.events_pending,
            ));
            if !snap.last_paths.is_empty() {
                out.push_str("\n**Last batch paths**\n\n");
                for p in &snap.last_paths {
                    out.push_str(&format!("- `{p}`\n"));
                }
            }
            if !snap.pending_paths.is_empty() {
                out.push_str(&format!(
                    "\n**Pending paths ({}, queued since last batch)**\n\n",
                    snap.pending_paths.len()
                ));
                for p in &snap.pending_paths {
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

        // Wrap the mpsc sender in an EventHandler that updates the live
        // `events_total` / `events_pending` / `pending_paths` counters as
        // events arrive — *before* the main loop blocks on recv. This is
        // what gives the agent visibility while a pass is in flight: the
        // counters tick up in real time, so `index_status` shows "10 events
        // pending behind running pass" instead of looking stuck.
        let (tx, rx) = channel::<notify::Result<notify::Event>>();
        let counter_status = status.clone();
        let counter_ws = canonical.clone();
        let event_handler = move |ev_res: notify::Result<notify::Event>| {
            if let Ok(ev) = &ev_res {
                for p in &ev.paths {
                    if is_indexable_event_path(p) {
                        if let Ok(mut s) = counter_status.lock() {
                            s.events_total = s.events_total.saturating_add(1);
                            s.events_pending = s.events_pending.saturating_add(1);
                            if s.pending_paths.len() < 50 {
                                if let Ok(rel) = p.strip_prefix(&counter_ws) {
                                    s.pending_paths.push(rel.to_string_lossy().into_owned());
                                }
                            }
                        }
                    }
                }
            }
            let _ = tx.send(ev_res);
        };
        let mut watcher = match notify::recommended_watcher(event_handler) {
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
                // Snapshot consumed: reset the per-pass queue counters.
                // Anything that arrives DURING this pass will tick them up
                // again (via the EventHandler in the notify thread),
                // surfacing as "queued for next pass" in `index_status`.
                s.events_pending = 0;
                s.pending_paths.clear();
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
