# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Per-pass `[:CALLS]` scoping (bug fix).** `index_files_via_lsp`
  used to wipe *every* `[:CALLS]` edge in the graph before rebuilding
  for the changed-file set. In incremental and live mode that meant a
  single-file save left the whole codebase CALLS-less until the next
  full reindex. The wipe is now scoped to callers in the current pass,
  so unchanged files keep their call graph.
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

[Unreleased]: https://github.com/codegraph/codegraph/commits/main
