# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
