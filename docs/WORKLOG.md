# Worklog

_Generated from the graph by `codegraph-mcp report`. Append-only history._

## [WIP] `[feature]` Graph-backed worklog (items / statuses / comments) + report subcommand
_id `wl-graph-worklog` · area `mcp` · created 2026-05-16T11:30:00Z_

### `in_progress` — 2026-05-16T12:00:00Z

> **claude** (2026-05-16T11:30:00Z):
>
> User proposed: separate markdowns by topic (architecture / roadmap / worklog), done items kept with date, agent remarks attached as comments. Then upgraded to: model it in the graph itself — :WorklogItem -[:HAS_STATUS]-> :Status -[:HAS_COMMENT]-> :Comment, with :Status append-only and 1:n status→comments.

> **claude** (2026-05-16T12:05:00Z):
>
> Phase A (schema + 5 MCP CRUD tools + wipe protection) and Phase B (report CLI subcommand rendering ROADMAP.md + WORKLOG.md) shipped in commit 98d3aef. Workspace 44/44 mcp tests pass, clippy clean. Phase C (migrating TODO.md + journal.md content into the graph) is what this commit does.

---

## [DONE] `[perf]` Pipeline outgoingCalls in chunks of 32 (parallel LSP)
_id `wl-pipeline-outgoing-calls` · area `indexer` · created 2026-05-16T10:35:00Z_

### `done` — 2026-05-16T10:41:04Z

> **claude** (2026-05-16T10:41:04Z):
>
> Sequential LSP requests left rust-analyzer mostly idle. Pipeline in chunks of 32 in-flight to keep the server saturated without queue-explosion. Commit 5e3ffad.

---

## [DONE] `[perf]` Skip outgoingCalls LSP requests for unchanged-body fns
_id `wl-body-hash-skip` · area `indexer` · created 2026-05-16T10:20:00Z_

### `done` — 2026-05-16T10:27:40Z

> **claude** (2026-05-16T10:27:40Z):
>
> Even after content-hash file skip, files that DO change still trigger outgoingCalls per fn at 10-50ms each. Per-fn body_hash lets us skip the LSP roundtrip for unchanged-body fns inside a changed file. Commit 42698c2.

---

## [DONE] `[perf]` Skip unchanged files via FNV-1a content hash
_id `wl-content-hash-skip` · area `indexer` · created 2026-05-16T10:00:00Z_

### `done` — 2026-05-16T10:15:56Z

> **claude** (2026-05-16T10:15:56Z):
>
> Watcher events vastly outnumber real semantic changes (17k+ events vs ~10 real edits per session). Per-file content_hash via FNV-1a 64-bit lets the indexer short-circuit unchanged files. Commit 4f3bf2a.

---

## [DONE] `[feature]` Watcher visibility — surface pending events during a pass
_id `wl-watcher-visibility` · area `mcp` · created 2026-05-16T09:30:00Z_

### `done` — 2026-05-16T10:06:57Z

> **claude** (2026-05-16T10:06:57Z):
>
> User noticed state: running for 45s+ during big batches with no indication new edits were being received. Real issue: events queued silently in the mpsc channel between loop iterations — counter never ticked unless the loop was awake. Fix: wrap mpsc sender in notify::EventHandler closure that updates events_total/pending BEFORE forwarding. index_status now always renders an Events: N total, M pending line. Commit 879d00f.

---

## [DONE] `[bug]` K8 — preserve [:NOTES] across reindex via snapshot/restore
_id `wl-k8-notes-survive` · area `indexer` · created 2026-05-16T09:00:00Z_

### `done` — 2026-05-16T10:03:30Z

> **claude** (2026-05-16T10:03:30Z):
>
> The earlier skill-file workaround (attach notes to :File or :Package instead) was wrong — :File and :Package are also wiped on --full. Fix: snapshot (note_id, target_kind, target_identity) for every [:NOTES]-> edge BEFORE any wipe, then MERGE back after LSP rebuild. Notes whose target was renamed-away stay as orphans, still discoverable via list_notes. Two contract tests. Commit c5f167b.

---

## [DONE] `[feature]` Phase 8 — :GitCommit:WorkingTree overlay for uncommitted edits
_id `wl-phase8-overlay` · area `indexer` · created 2026-05-16T08:00:00Z_

### `in_progress` — 2026-05-16T08:00:00Z

### `done` — 2026-05-16T10:49:33Z

> **claude** (2026-05-16T10:49:33Z):
>
> Design: dont pollute persistent history. Project the working tree onto a pseudo :GitCommit:WorkingTree {hash: WORKING-TREE} node updated every indexer pass. Dirty: MERGE overlay, move :SNAPSHOT_OF from HEAD onto it, add :PARENT_OF from HEAD → overlay. Clean: DETACH DELETE overlay, re-anchor on HEAD. diff_since needs zero changes — its existing query picks up the overlay because :WorkingTree carries :GitCommit too. Commit e149996.

> **claude** (2026-05-16T10:55:00Z):
>
> Bug caught by the test: cmd_output trims the whole git output, stripping the leading-space status char from worktree-modified files. First parser sliced at byte 3 and lost the first char of every modified path. Replaced with split-on-first-space.

---

## [DONE] `[refactor]` K6 — per-tool MCP handler split into sibling modules
_id `wl-k6-handler-split` · area `refactor` · created 2026-05-15T22:30:00Z_

### `done` — 2026-05-15T23:08:27Z

> **claude** (2026-05-15T23:08:27Z):
>
> main.rs went from ~4000 to ~1600 LoC over 5 commits (44c8f1e through 81e9a91). Each tool now lives in its own sibling module: concepts.rs, coverage.rs, diff.rs, explore.rs, find.rs, history.rs, impact.rs, notes.rs, pr_notes.rs, views.rs, watch_tools.rs. Set the per-tool convention used by all future tool additions (incl. this very worklog feature).

---

## [DONE] `[bug]` K9 fix — [:TESTS] edge duplication via velr MERGE bug
_id `wl-k9-tests-dedup` · area `indexer` · created 2026-05-15T22:00:00Z_

### `pending` — 2026-05-15T22:00:00Z

> **claude** (2026-05-15T22:05:00Z):
>
> Surfaced via dogfooding: handle_diff_since showed 38 incoming [:TESTS] from one test function. Initial hypothesis: Phase 6 needs MERGE instead of CREATE.

### `done` — 2026-05-16T11:07:14Z

> **claude** (2026-05-16T11:07:14Z):
>
> Discovered velr 0.2.16 MERGE-on-relationship does NOT deduplicate (start, end, type) triples — strengthened test (2 upstream :CALLS → 12 [:TESTS] via MERGE) caught it. Fix: anchored wipe + client-side HashSet dedup + CREATE. Commit 11de394.

> **claude** (2026-05-16T11:30:00Z):
>
> Lesson: trivially-green tests are not contracts. Tests that mimic the failure shape are. The original idempotency test passed even with broken MERGE because it only seeded one matching row.

---

## [DONE] `[bug]` K10 fix — :DocSection / :Doc accumulation across reindexes
_id `wl-k10-doc-wipe` · area `indexer` · created 2026-05-15T22:00:00Z_

### `pending` — 2026-05-15T22:00:00Z

> **claude** (2026-05-15T22:05:00Z):
>
> Surfaced via dogfooding: refactoring.md#why showed 253 incoming [:MENTIONS] on the same function. Distorted every node_md neighbour ranking and explore score.

### `done` — 2026-05-16T11:07:14Z

> **claude** (2026-05-16T11:07:14Z):
>
> Root cause: full wipe set excluded :Doc/:DocSection, live mode had no per-file markdown wipe. Fix: per-file DETACH DELETE keyed on qualified_name and section prefix before write_doc_node/write_section_node. Verified clean: 16 docs / 175 sections / 103 mentions (was 6024/6024/N).

---

## [TODO] `[refactor]` IndexCtx / tools::Ctx structs
_id `wl-k7-ctx-structs` · area `refactor` · created 2026-05-15T20:00:00Z_

### `pending` — 2026-05-15T20:00:00Z

> **claude** (2026-05-15T20:00:00Z):
>
> TODO.md K7 — skipped intentionally. Current per-tool signatures are not painful. Pull this forward when we add a cross-cutting concern (per-call timing, logging) that justifies the indirection. Document the trigger condition in CLAUDE.md so the next maintainer knows when to flip it from pending to in_progress.

---

## [DONE] `[feature]` I2 — codegraph-mcp --watch mode (live reindex on save)
_id `wl-i2-watch-mode` · area `mcp` · created 2026-05-15T00:00:00Z_

### `done` — 2026-05-15T18:00:00Z

> **claude** (2026-05-15T18:00:00Z):
>
> Spawns a notify-based fs watcher in a background thread. Debounced 500ms reindex of only the changed paths. Live mode skips git history + sidecar advancement — persistent revision history only moves on actual git commit. This is what makes the MCP-as-IDE story actually work for the agent: save a file, query the new graph state.

---

## [DONE] `[feature]` H4 — diff_since(commit) MCP tool
_id `wl-h4-diff-since` · area `mcp` · created 2026-05-14T00:00:00Z_

### `done` — 2026-05-14T20:00:00Z

> **claude** (2026-05-14T20:00:00Z):
>
> Walks :GitCommit :PARENT_OF DAG from given commit to HEAD. Lists commits in the range and :File/:Function nodes added during the range (via first_seen_commit). Removals not tracked — indexer keeps no tombstones. PR-prep / changelog generator.

---

## [DONE] `[feature]` H1 — impact MCP tool (transitive blast radius)
_id `wl-h1-impact` · area `mcp` · created 2026-05-14T00:00:00Z_

### `done` — 2026-05-14T18:00:00Z

> **claude** (2026-05-14T18:00:00Z):
>
> Walks CALLS outwards (callees) + inwards (callers) up to depth, plus one-hop MENTIONS and IMPLEMENTED_BY. Markdown report with counts per category and top-N affected nodes. Replaces the "who uses this" crawl before refactors.

---

## [DONE] `[feature]` G3 — real revision history (:GitCommit + :Author + :PARENT_OF)
_id `wl-g3-git-history` · area `indexer` · created 2026-05-13T00:00:00Z_

### `done` — 2026-05-13T18:00:00Z

> **claude** (2026-05-13T18:00:00Z):
>
> :GitCommit + :Author no longer wiped on --full. First run backfills up to 200 commits; incremental runs walk only the new range. Full DAG via :PARENT_OF. :File and :Function get first_seen_commit / last_seen_commit properties. Parser unit-tested against a temp repo. Enables diff_since and any time-windowed query.

---

## [TODO] `[task]` Replace placeholder repository URL in Cargo.toml
_id `wl-c2-repo-url` · area `release` · created 2026-05-10T00:00:00Z_

### `pending` — 2026-05-10T00:00:00Z

> **claude** (2026-05-10T00:00:00Z):
>
> TODO.md C2 — needs the actual GitHub repo URL once the public repo exists. Same for homepage. Trivial change but cannot be done until the URL is decided.

---

## [TODO] `[task]` Verify rust-version = 1.75 locally
_id `wl-c5-msrv-verify` · area `release` · created 2026-05-10T00:00:00Z_

### `pending` — 2026-05-10T00:00:00Z

> **claude** (2026-05-10T00:00:00Z):
>
> TODO.md C5 — needs an installed Rust 1.75 toolchain to verify locally. CI msrv job catches regressions on push, so risk is low; ship-blocker only if CI gets disabled.

---

## [TODO] `[feature]` Pre-built release binaries via cargo-dist
_id `wl-f2-cargo-dist` · area `release` · created 2026-05-10T00:00:00Z_

### `pending` — 2026-05-10T00:00:00Z

> **claude** (2026-05-10T00:00:00Z):
>
> TODO.md F2 — nice-to-have, not a blocker. Wire cargo-dist or hand-roll a GitHub release workflow that produces tar.gz per target.

---

## [TODO] `[feature]` bdd-viz rendering from velr (skip JSON intermediate)
_id `wl-f3-bdd-viz-velr` · area `viz` · created 2026-05-10T00:00:00Z_

### `pending` — 2026-05-10T00:00:00Z

> **claude** (2026-05-10T00:00:00Z):
>
> TODO.md F3 — only worth doing if the dataset grows past the point where the JSON round-trip matters. Today the materialised JSON is tens of KB and renders instantly; revisit when a real user has thousands of scenarios.

---

