# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **velr bumped 0.2.16 → 0.2.17.** Two bugs we'd worked around are
  fixed upstream: unanchored `MATCH ()-[r:R]->() DELETE r` now drains
  in one pass (see `docs/velr-bugs/0001-…md`), and `MERGE` on a
  relationship is idempotent again (`MERGE (a)-[:X]->(b)` no longer
  stacks duplicate edges across runs). The defensive `DISTINCT` in
  `arch.rs` stays for regression safety. All `cargo test` green
  against 0.2.17.

### Added

- **`dead_code` MCP tool (nx-06).** Lists `:Function` nodes with no
  incoming `:CALLS` edges. Two-query design (candidates + callers,
  client-side set-diff) sidesteps velr's expensive `OPTIONAL MATCH +
  NOT` shape. Defaults exclude `:Test` candidates and count test
  callers as life; `ignore_test_callers=true` flips to a
  "covered-only-by-tests" sweep. `name_skip` filters obvious entry
  points (`main`, `handle_`, `phase_`). Output is `file:line` grouped
  so it's editor-jumpable, and the disclaimer about dynamic dispatch
  / FFI / pub-API false positives is part of the response.
- **Agent-driven architecture overlay (nx-18).** New
  `--with-arch-agent` CLI flag on `codegraph-indexer` runs an
  agent pass at the end of a `--full` reindex: gathers the
  workspace's `:Package` list, hot functions per package and
  cross-package `:CALLS` density, hands it to `claude -p` with a
  fixed JSON schema, and MERGEs the response back as `:ArchModule`
  nodes plus `[:CONTAINS]` → `:Package`, `[:GROUPS]` → `:Function`,
  `[:USES {semantic_kind}]` edges. Each module gets a
  `semantic_kind` (core / adapter / protocol / cli / lib / app /
  test / infra), short description and `layer_hint`. Modules can
  span multiple packages (1:N) and split a package by function
  set (`groups_functions`) — the prompt explicitly nudges 3–7
  modules instead of 1:1-with-`:Package`. Failure modes (missing
  CLI, bad JSON, exit-nonzero) all degrade silently — the
  previous overlay was wiped first, so the graph ends with no
  `:ArchModule` rather than a partial one. Visualise via
  `graph_export(label="ArchModule", key="name", value="<name>")`.
  Supersedes the heuristic experiment under nx-15.
- **`graph_export` MCP tool (nx-05).** Render a node-centered subgraph
  as a Mermaid `flowchart LR` (default) or Graphviz DOT diagram. BFS
  from the seed, clamped depth 1–3, capped at 200 nodes. Output is
  fenced (```mermaid / ```dot) so it round-trips into GitHub, chats
  and `:Note` bodies. Identity for neighbour nodes uses a coalesce
  bouquet (qualified_name / id / path / name / hash) so heterogeneous
  neighbours render with a useful label without per-label tables.
- **Commit-trailer → worklog auto-link (nx-09).** `phase_history`
  parses `Refs: nx-XX` (case-insensitive, comma- or space-separated,
  multiple per message) out of every commit message and MERGEs a
  `[:REFERENCES]` edge from the `:GitCommit` to each matching
  `:WorklogItem`. Unknown ids are silent no-ops via the join MATCH —
  cross-repo or stale trailers don't pollute the graph.

### Changed

- **`main.rs` test extraction (nx-04).** All `#[cfg(test)] mod tests`
  content moved out of `crates/codegraph-mcp/src/main.rs` into a
  sibling `tests.rs` module via `#[cfg(test)] mod tests;`. main.rs
  drops from ~1.7k to ~900 LoC, well under the 2k cap CLAUDE.md
  enforces. Per-sibling-module distribution is deferred.
- **`worklog_list` shows comment count + last activity (nx-08).** Two
  new columns: `comments` (per-item total) and `last_activity` (max of
  the latest status timestamp and the latest comment timestamp).
  Aggregated client-side via a second MATCH + HashMap fold so velr's
  thin COUNT/subquery surface is sidestepped.

### Added

- **Transaction-leak telemetry (nx-01).** Each MCP `begin` now assigns
  a monotonic `tx#N` id and tracks `opened_at`. `begin` / `commit` /
  `rollback` write a one-line stderr log with the id, query count and
  elapsed time. A `begin` issued while another tx is still open logs a
  WARNING — that pattern is the prime suspect for the sporadic WAL
  bloat we hit during long agent sessions.
- **DB / WAL / open-tx surface in `index_status` (nx-02).** A new
  `## Database files` block reports the velr DB, WAL and SHM file
  sizes (binary units) and any currently-open buffered transaction
  (tx#, age, pending count, optional message). Shown unconditionally —
  even without `--watch` — because the bloat is independent of the
  watcher. A ⚠ marker fires when the WAL crosses 100 MiB or an open
  tx is older than 30 s, so the bug is noticeable before it becomes a
  20 GB problem.

## [0.2.0-alpha.2] - 2026-05-16

Follow-up to alpha.1: adds Go support and gets CI fully green.

### Added

- **Go language support.** New `ProjectKind::Go` variant detected by
  presence of a `go.mod`. Uses `gopls` as the LSP (override with
  `--lsp`). `index_go_packages` parses the `module` directive and
  `require` block from `go.mod` into `:Package` + `[:DEPENDS_ON]`
  edges, just like the Cargo / npm / pyproject paths. Source discovery
  walks the module root recursively (Go's convention), skipping
  `vendor/`. New integration fixture at `examples/demo-go/` mirrors
  the shape of the Rust / Python / TS demos.

### Changed

- **MSRV bumped from 1.75 → 1.95.** velr 0.2.16's transitive deps
  (`blake3` → `cpufeatures`, `constant_time_eq`) now require
  `edition2024`, which Cargo 1.75 doesn't know. Bumping to current
  stable is the pragmatic fix; the CI `msrv` job now pins 1.95.

### Fixed

- **CI clippy 1.95** — removed a redundant `.into_iter()` in
  `markdown_index.rs` that tripped `clippy::useless_conversion`.
- **CI deny licenses** — `velr` publishes with
  `license = "non-standard"` which cargo-deny can't validate. Added
  `[[licenses.clarify]]` blocks per velr crate overriding with a
  local `LicenseRef-velr-non-standard` placeholder we allow.

## [0.2.0-alpha.1] - 2026-05-16

First versioned release after the 0.1.0 initial cut. The bulk of this
release is the agent-memory layer: the graph now persists not just
code structure but the agent's investigations and the project's own
worklog, all survived across `--full` reindex.

Companion release notes for the items below are also queryable from
the graph itself:

```cypher
MATCH (r:Release {version: '0.2.0-alpha.1'})-[:INCLUDES]->(w:WorklogItem)
RETURN w.kind, w.title, w.current_status_at ORDER BY w.kind, w.title
```

### Added

- **Graph-backed worklog.** New `:WorklogItem`, `:Status`, `:Comment`
  node labels store an append-only project worklog inside the graph
  itself. `:Status` is append-only (one node per transition); each
  `:Status` can carry many `:Comment` nodes (1:n). `:WorklogItem.kind`
  classifies the work (`bug` | `feature` | `task` | `refactor` | `perf`
  | `docs`, default `task`) — same vocab as Conventional Commits.
  MCP tools: `worklog_create`, `worklog_set_status`, `worklog_comment`,
  `worklog_list` (filters: `area`, `status`, `kind`), `worklog_md`.
  New CLI subcommand `codegraph-mcp report --db <p> --out <dir>`
  renders `ROADMAP.md` and `WORKLOG.md` from the graph. Worklog nodes
  are in the wipe-protected set, so they survive `--full` reindex like
  `:Note` / `:Concept` / `:View`.
- **Per-pass `[:CALLS]` scoping (bug fix).** `index_files_via_lsp`
  used to wipe *every* `[:CALLS]` edge in the graph before rebuilding
  for the changed-file set. In incremental and live mode that meant a
  single-file save left the whole codebase CALLS-less until the next
  full reindex. The wipe is now scoped to callers in the current pass,
  so unchanged files keep their call graph.
- **`explore` MCP tool** — token-budgeted graph exploration. BFS from a
  seed up to `max_depth`, score each candidate (`degree + 4·has_notes
  + 2·has_doc_mentions − 5·depth`), greedily fill a Markdown report
  until `char_budget` is exhausted, footer reports drops. Replaces the
  multi-`node_md`-call pattern with one bounded call.
- **`coverage_md` MCP tool** — single Markdown report of the graph's
  dim spots: orphan functions (no inbound `[:CALLS]`), untested
  functions ranked by `[:CALLS]` fan-in, files with no `:Note`s, and
  packages with zero doc-mentions. Onboarding hot list +
  refactor-risk hot list in one call.
- **Persistent LSP pool** (`codegraph_indexer::LspPool`). With
  `--watch`, the MCP server now keeps every language server alive
  across reindex batches — `rust-analyzer`, `typescript-language-server`,
  `pyright-langserver`, anything LSP-compliant. Each server pays its
  cold-start cost (~5s for rust-analyzer) on the **first** batch only;
  every subsequent save settles in roughly the LSP's incremental
  re-analyze time (often well under a second for single files).
- **`didChange`-aware reindex.** `index_files_via_lsp` now sends a
  proper `textDocument/didChange` notification with the new content
  for files the LSP already knows about, instead of a duplicate
  `didOpen`. Eliminates the `ERROR duplicate DidOpenTextDocument`
  noise from rust-analyzer and gives the LSP fresh content after a
  save. The 15s warm-up sleep is now skipped on subsequent passes
  (replaced by a 1s settle wait).
- **`--watch <workspace>` mode for `codegraph-mcp`.** When set, the
  MCP server spawns a `notify`-based filesystem watcher that
  re-runs the indexer on a debounced batch of file changes (default
  500ms). Uses **live mode** (`IndexOptions::with_paths`): only the
  changed files are re-parsed, the git-history phase is skipped, and
  the sidecar metadata is left untouched, so uncommitted edits show
  up as a draft overlay without polluting the persistent revision
  history. The MCP server's existing `db_mtime`-based reopen logic
  picks up the new graph state on the next tool call.
- **`index_status` MCP tool** — reports the live indexer's state
  (`idle` / `running`), last-run mode + duration, the workspace-
  relative paths from the most recent batch, and any error. Lets
  the agent wait for `state == "idle"` after a save before issuing
  fresh queries. Without `--watch`, returns a stub making the no-op
  explicit.
- **Indexer library refactor.** `pub fn run_indexer(opts: IndexOptions)
  -> Result<IndexStats, String>` now drives the pipeline; the
  `codegraph-indexer` binary is a ~60-line CLI wrapper. Embedders
  use `IndexOptions::new(workspace, db).with_paths(rel_paths)` for
  live-mode reindexing.
- **`watch` / `unwatch` / `list_watches` MCP tools** + indexer Phase 7.
  Mark a node as watched: the next indexer run diffs the current
  `body` against the captured baseline, and on change attaches a
  `:Note` tagged `watch-trigger` to the node, then re-baselines.
  Cross-session, asynchronous change notifications without polling.
- **`import_pr_notes` MCP tool** — bulk-imports PR / code-review
  comments as `:Note` nodes attached to any `:Function` they
  reference. Pulls every backtick-delimited identifier from each
  comment body, looks it up against `Function.name` and
  `Function.qualified_name`, and if any match, attaches one note to
  all matched functions with `tags='pr-comment'`. Pairs naturally
  with `gh pr view --json comments`.
- **`:Concept` layer** — user-curated subsystem labels with
  `define_concept`, `concept`, `list_concepts` MCP tools. A
  `:Concept` connects to its members via `[:DESCRIBES]`; the
  `concept(name)` dossier rolls up direct members, mentioned
  `:Function`s, covering `:Test`s, and attached `:Note`s into one
  Markdown report. Concepts survive `--full` reindex.
- **`:Test` label + `[:TESTS]` edges.** A post-LSP indexer phase tags
  every `:Function` whose body contains `#[test]` or `#[tokio::test]`
  with a `:Test` label, then materialises `(:Test)-[:TESTS]->(:Function)`
  for every `[:CALLS]` from a test into a non-test. Lets queries
  cleanly answer "which functions are tested?" and "which test covers
  this code?".
- **Ranked neighbours in `node_md`** — within each edge group,
  neighbours are now sorted by total degree (in + out) descending so
  the per-group cap surfaces the most load-bearing nodes first. Each
  row carries a `_(deg N)_` tag when degree is non-zero.
- **`diff_since` MCP tool** — walks the `:GitCommit` DAG between a
  baseline commit and HEAD, listing commits in range and `:File` /
  `:Function` nodes whose `first_seen_commit` lands inside it.
  Removals are not tracked (no tombstones); the output footer makes
  this explicit.
- **Saved views** — `save_view`, `view`, `list_views` MCP tools.
  Cypher queries are persisted as `:View` nodes (survive `--full`
  reindex), parameterised with `$tokens` that get substituted at run
  time via `escape_str`. Lets the agent build up a library of named,
  reusable queries instead of re-deriving Cypher each call.
- **`find_symbol` MCP tool** — fuzzy substring search across
  `:Function` and `:Symbol` nodes with relevance ranking (exact >
  startsWith on name > startsWith on qn > contains). Returns a
  Markdown table including `file:line` and the first line of `body`
  as a signature.
- **`impact` MCP tool** — transitive blast-radius report for a node.
  BFS over `[:CALLS]` (in + out) up to a bounded depth, plus one-hop for
  `[:MENTIONS]` and `[:IMPLEMENTED_BY]`. Returns a Markdown report with
  per-category counts and the top-N affected nodes.
- **Markdown-shaped MCP tools** for LLM-friendly output:
  - `cypher_md` — same as `cypher` but renders rows as a GitHub-flavoured
    Markdown table.
  - `node_md` — compact Markdown dossier for a single node (properties
    plus incoming/outgoing neighbours grouped by edge type, plus any
    attached `:Note`s).
  - `history` — list `:GitCommit` snapshots recorded in the graph.
- **`:Note` nodes** as long-lived annotations attached to any graph node
  via the new `write_note` and `list_notes` MCP tools. Notes survive
  `--full` reindex and surface automatically inside `node_md`.
- **Real revision history.** The indexer no longer wipes `:GitCommit` /
  `:Author` on `--full`. First-time / full runs backfill up to the last
  200 commits reachable from `HEAD`; incremental runs walk only the
  commits between the previously indexed `HEAD` and the new one. The
  full DAG is materialised as `(:GitCommit)-[:PARENT_OF]->(:GitCommit)`.
- **`first_seen_commit` / `last_seen_commit`** properties on `:File` and
  `:Function`, updated on every index pass.
- **Claude skill** (`examples/claude-skill/codegraph.md`) and
  repo-level `CLAUDE.md` instructing Claude Code to prefer the MCP
  graph for navigation and to persist findings as `:Note`s.

### Changed

- Full reindex wipe set is now `{File, Workspace, Package, APIEndpoint,
  APIType}`. Revision history (`:GitCommit`, `:Author`, `:Note`) is
  intentionally preserved.

- Initial workspace skeleton with three crates (`codegraph-core`,
  `codegraph-indexer`, `codegraph-mcp`) on top of [velr](https://crates.io/crates/velr).
- `codegraph-core`: thin velr adapter exposing `Db`, owned `Cell` / `Table`
  types, and a Cypher value escaper covering strings, numbers, booleans,
  lists and inline maps. Unit-tested.
- `codegraph-indexer`: incremental code-graph indexer supporting Rust
  (LSP via `rust-analyzer`), Node/TypeScript and Python projects, plus
  Markdown / Gherkin / OpenAPI / GraphQL SDL / Protobuf passes. Sidecar
  metadata file (`<db>.codegraph-meta.json`) tracks the last-indexed git
  commit so re-runs only re-parse changed files.
- `codegraph-indexer` also ships a `bdd-viz` binary that renders
  Package → Feature → Scenario → Step → Function as an interactive HTML
  graph (cytoscape + dagre).
- `codegraph-mcp`: JSON-RPC MCP server with `schema`, `cypher`, `begin`,
  `write`, `commit`, `rollback`, `explain` tools. Auto-reopens the velr
  handle when the on-disk database mtime advances.
- CI: `cargo fmt --check`, `cargo clippy -D warnings`, build+test on
  Linux & macOS, MSRV check on Rust 1.75, `cargo deny check`.

### Notes

- velr 0.2.x is alpha; backend behaviour (especially `MERGE`, `labels()`
  and `type()`) may change. The indexer / MCP server pin the entire
  feature set against velr 0.2.9 for now.

[Unreleased]:      https://github.com/ChrisRega/codegraph/compare/v0.2.0-alpha.2...HEAD
[0.2.0-alpha.2]:   https://github.com/ChrisRega/codegraph/compare/v0.2.0-alpha.1...v0.2.0-alpha.2
[0.2.0-alpha.1]:   https://github.com/ChrisRega/codegraph/compare/v0.1.0...v0.2.0-alpha.1
