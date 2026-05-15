# refactoring.md — staged plan

Honest assessment from `mcp__codegraph__coverage_md` + a few `cypher_md`
queries against the current build.

## Why

| File / Function | LoC | Verdict |
| --- | --- | --- |
| `crates/codegraph-mcp/src/main.rs` | 4256 | 🚩 monolith — dispatcher + 25 handlers + 4 renderers + watcher + status + tool schemas + tests in one file |
| `crates/codegraph-indexer/src/lib.rs` | 1912 | 🚩 large; one 495-LoC orchestrator buried inside |
| `codegraph-indexer::lib::run_indexer_inner` | 495 | 🚩 god function: 7 phases inline, no isolation |
| Largest handler (`handle_diff_since`) | 188 | ⚠️ approaching pain |
| `tool_list` | 300 | ✅ JSON literal, fine |
| Workspace tests | 66 | ✅ |

Smaller smells:
- `chrono_now_iso` lives twice (in `codegraph-mcp` and `codegraph-indexer`).
- `parse_node_address` was extracted but most call sites still inline the
  `label` / `key` / `value` parsing.
- Watcher subsystem (`spawn_indexer_watcher` + `IndexStatus` + pool wiring,
  ~250 LoC) is mixed into the JSON-RPC dispatch file.
- Markdown rendering primitives (`md_cell`, `format_table_md`,
  `render_notes_rows`, `render_neighbours`) are scattered across the
  monolith.

The code is **correct and stylistically consistent** — the refactor is
about future-friction, not bug-risk.

## Non-goals

- No trait-based pipeline for indexer phases. Phases have different
  signatures and ordering matters.
- No macros for handler definitions. Removes IDE goto.
- No config-file system. CLI flags are fine.
- No async / tokio refactor. JSON-RPC + stdio + sync velr is the right
  shape at this scope.

## Tier 1 — module hygiene (3–4 commits, low risk)

### 1a. Split `mcp/src/main.rs` into modules

Target layout:

```
crates/codegraph-mcp/src/
  main.rs              CLI args + dispatch loop only (~150 LoC)
  dispatch.rs          match on req.method.as_str()
  util.rs              chrono_now_iso, ok_text, err_text, safe_ident
  render.rs            md_cell, format_table, format_table_md,
                       render_notes_rows
  tx.rs                TxState + handle_begin/write/commit/rollback
  watch/
    mod.rs
    fsnotify.rs        spawn_indexer_watcher + path filter
    status.rs          IndexStatus, SharedStatus, handle_index_status
  tools/
    mod.rs             tool_list() + public re-exports
    schema.rs          handle_schema
    cypher.rs          handle_cypher, handle_cypher_md, handle_explain
    node.rs            handle_node_md, parse_node_address, neighbour
                       ranking
    notes.rs           write_note, list_notes
    impact.rs          handle_impact + bfs helpers
    find.rs            handle_find_symbol
    views.rs           save_view, view, list_views,
                       substitute_view_params
    diff.rs            handle_diff_since
    concepts.rs        define_concept, concept, list_concepts
    coverage.rs        handle_coverage_md
    explore.rs         handle_explore + ExploreCandidate
    pr_notes.rs        handle_import_pr_notes
    watch_tools.rs     handle_watch / unwatch / list_watches
    history.rs         handle_history
```

Tests stay in `#[cfg(test)] mod tests` next to their handlers. Net
goal: no source file > ~400 LoC, every tool refactor lives in one
small file.

### 1b. `chrono_now_iso` → `codegraph-core::time`

Single source of truth. Both crates import from there. Removes the
silent risk that the two implementations drift.

### 1c. Use `parse_node_address` everywhere

The helper exists; ~5 handlers still parse label/key/value inline.
Replace with `let (label, key, value) = parse_node_address(params)?;`.

## Tier 2 — architecture (2–3 commits, medium risk)

### 2a. Split `run_indexer_inner` into phase functions — **partial**

Done: extracted `phase_history` (Phase 5, ~95 LoC), `phase_test_tagging`
(Phase 6), `phase_watch_triggers` (renamed from `fire_watch_triggers`
for naming consistency), and `save_sidecar` into named helpers. The
orchestrator's tail dropped from ~145 LoC inline to ~10 LoC of named
calls. Existing `phase6_tags_tests_and_links_them` test now drives
`phase_test_tagging` directly instead of duplicating the SQL — proper
contract test.

Deferred: extracting Phase 1+2 (workspace + packages + LSP file-wipe)
and Phase 3+4 (LSP indexing). Those are entangled with the
pool-vs-transient branch and need an `IndexCtx` struct to factor
cleanly. Not worth doing without a concrete next caller that needs
them isolated.

Target shape (signature sketch — bodies move, behaviour stays):

```rust
pub fn run_indexer_inner(opts, pool) -> Result<IndexStats> {
    let ctx = bootstrap(opts)?;
    maybe_full_wipe(&ctx);
    let pkgs = phase_packages(&ctx);
    let stats = phase_lsp(&ctx, &pkgs, pool)?;
    phase_api_specs(&ctx, &pkgs);
    phase_bdd(&ctx);
    phase_markdown(&ctx);
    if !ctx.is_live { phase_history(&ctx); }
    phase_test_tagging(&ctx);
    phase_watch_triggers(&ctx);
    if !ctx.is_live { save_sidecar(&ctx)?; }
    Ok(stats)
}
```

Each `phase_*` is independently testable against an in-memory DB plus
a minimal `IndexCtx`. Ordering stays explicit at the call site.

### 2b. `tools::Ctx` struct — **deferred**

Reviewing the dispatcher: most handlers are already `(db, params)` and a
few are `(db, tx, params)` — the argument-threading isn't actually
painful at this scope. Wrapping them in a `Ctx` struct earns its keep
only when we want cross-cutting concerns (per-call timing, logging,
tracing) executed in one place around dispatch. We don't yet, so
introducing the indirection now would be busywork.

Pulled forward when we add the first cross-cutting concern (likely
per-call timing surfaced via `index_status`-style introspection).

## Tier 3 — nice-to-have (defer)

- End-to-end smoke test that spawns the binary and talks JSON-RPC over
  stdin/stdout. Currently we test handler functions directly, which
  doesn't catch wiring breakage.
- `write_note` calls on the source files — we ship the tool, we should
  use it. 16/16 source files currently have zero notes.
- Consolidate the ~6 BTreeSet-based "collect distinct strings from a
  query" patterns into one helper in `render.rs`.

## Execution order

1. **1b** first (smallest, kills duplication, no module shuffle yet).
2. **1c** (small, prepares 1a — fewer handlers to touch after split).
3. **1a** (the big mechanical move; tests should stay green throughout).
4. **2b** (introduce `Ctx`, threads through dispatcher).
5. **2a** (phase-split inside the now-clean indexer crate).

After each commit: `cargo fmt && cargo clippy --workspace --all-targets
-- -D warnings && cargo test --workspace`. No commit lands if any of
those fail.
