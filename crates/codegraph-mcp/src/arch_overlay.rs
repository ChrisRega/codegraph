//! `arch_overlay` MCP tool — synchronously invoke
//! `codegraph_indexer::arch::phase_arch_agent` on the live DB.
//!
//! This is the in-session counterpart to the CLI
//! `codegraph-indexer --full --with-arch-agent`. Useful when you want
//! to refresh the architecture view without restarting the server or
//! shelling out. Cost: one `claude -p` subprocess call (real money,
//! real seconds). Off by default in the sense that the user has to
//! call this tool explicitly — the watcher never triggers it.
//!
//! The tool needs a workspace name for the prompt context. We derive
//! it from the `--watch` path the server was started with; if the
//! server is running without `--watch`, the caller must pass
//! `workspace_name` explicitly.

use codegraph_core::Db;
use codegraph_indexer::arch::phase_arch_agent;
use serde_json::Value;

use crate::util::{err_text, ok_text};

pub fn handle_arch_overlay(db: &Db, watch_path: Option<&str>, params: &Value) -> Value {
    let derived = watch_path
        .and_then(|p| std::path::Path::new(p).file_name())
        .map(|n| n.to_string_lossy().to_string());
    let workspace_name = params
        .get("workspace_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(derived)
        .unwrap_or_else(|| "workspace".to_string());

    eprintln!("[arch_overlay] invoking agent for workspace `{workspace_name}` …");
    // phase_arch_agent already wipes :ArchModule first, logs to stderr,
    // and degrades silently on subprocess / parse failures. So we just
    // call it and then read back what landed.
    phase_arch_agent(db, &workspace_name);

    // Summarise the post-state so the agent that called the tool has
    // visible feedback rather than a generic "ran".
    let mod_count = db
        .query("MATCH (a:ArchModule) RETURN count(a) AS n")
        .ok()
        .and_then(|t| {
            t.rows
                .first()
                .and_then(|r| r.first().and_then(|c| c.as_i64()))
        })
        .unwrap_or(0);
    let edge_count = db
        .query("MATCH (:ArchModule)-[r:USES]->(:ArchModule) RETURN count(r) AS n")
        .ok()
        .and_then(|t| {
            t.rows
                .first()
                .and_then(|r| r.first().and_then(|c| c.as_i64()))
        })
        .unwrap_or(0);
    if mod_count == 0 {
        return err_text(format!(
            "arch_overlay: agent returned no modules (workspace=`{workspace_name}`). \
             Check stderr for the failure mode — most likely `claude` CLI missing, \
             not logged in, or JSON parse error. The previous overlay was wiped \
             before the call, so the graph currently has no :ArchModule."
        ));
    }
    ok_text(format!(
        "# arch_overlay\n\n\
         - workspace: `{workspace_name}`\n\
         - `:ArchModule` count: **{mod_count}**\n\
         - `[:USES]` edges between modules: **{edge_count}**\n\n\
         Visualise via:\n\
         ```\n\
         graph_export(label=\"ArchModule\", key=\"name\", value=\"<name>\", depth=1)\n\
         ```\n\
         List names via:\n\
         ```\n\
         cypher(\"MATCH (a:ArchModule) RETURN a.name AS name, a.semantic_kind AS kind, a.layer_hint AS layer ORDER BY a.layer_hint, a.name\")\n\
         ```"
    ))
}
