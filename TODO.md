# codegraph TODO

Tracks the remaining work before a public open-source release. Items are
grouped by category and roughly ordered by priority within each group.
Strike through (`~~text~~`) when done.

## A. Code correctness / functional gaps

- [x] **A1** — `explain` MCP tool now routes through `EXPLAIN <query>` via
  `Db::query_many` (multi-table velr `exec`) instead of the
  unprintable `ExplainTrace`.
- [x] **A2** — Verified `labels(n)` and `type(r)` work in velr 0.2.9.
  Locked in by `crates/codegraph-core/tests/cypher_intro.rs`.
- [x] **A3** — Smoke tests exist for each crate
  (`tests/velr_roundtrip.rs`, `tests/cypher_intro.rs`,
  `tests/explain_probe.rs`, `tests/merge_semantics.rs` for `core`;
  `meta::tests` for indexer; `format_table` / `tool_list` tests for
  mcp). 27 tests, all green.
- [x] **A4** — `bdd_steps`, `gherkin`, `markdown_index`, `meta`, mcp's
  `format_table` / `tool_list` all carry inline tests.
- [x] **A5** — Full reindex now wipes `:File`, `:Workspace`, `:Package`,
  `:GitCommit`, `:Author`, `:APIEndpoint`, `:APIType` in addition to
  the per-pass code-node cleanup.
- [x] **A6** — `db_mtime` in `codegraph-mcp` now picks the max mtime
  across the velr file plus its `-wal` / `-shm` SQLite sidecars.

## B. Tooling and repo hygiene

- [x] **B1** — `.github/workflows/ci.yml` runs fmt, clippy, build/test on
  Linux + macOS, MSRV (1.75) build, and `cargo deny check`.
- [x] **B2** — `rustfmt.toml` shipped, `cargo fmt --all` clean.
- [x] **B3** — `CONTRIBUTING.md`.
- [x] **B4** — `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1).
- [x] **B5** — `CHANGELOG.md` in Keep-a-Changelog format.
- [x] **B6** — `SECURITY.md` with reporting address.

## C. Cargo metadata / publish

- [x] **C1** — Each crate now has `keywords`, `categories`, `readme`,
  `documentation`, `homepage` populated.
- [ ] **C2** — Replace placeholder `repository` URL
  (`github.com/codegraph/codegraph`) with the real one once the GitHub
  repo exists. Same for `homepage`.
- [x] **C3** — `authors` populated on the workspace package metadata.
- [x] **C4** — `deny.toml` with license allow-list, wired into CI.
- [ ] **C5** — `rust-version = "1.75"` set but **not** verified against
  an installed 1.75 toolchain locally. CI's `msrv` job will catch
  any regressions on the next push.

## D. Documentation

- [x] **D1** — README expanded with feature blurb, schema overview,
  example queries, MCP wiring example.
- [x] **D2** — `docs/schema.md` enumerates every node label and edge
  type, including properties and example queries.
- [x] **D3** — `docs/mcp-tools.md` documents each MCP tool, its inputs,
  and the auto-reopen behaviour.
- [x] **D4** — Both binaries handle `--help` and `--version`.

## E. velr-specific risks

- [x] **E1** — README carries an alpha-status disclaimer pointing at
  velr's own alpha state.
- [x] **E2** — `docs/velr-notes.md` documents the connection-affine
  threading model, the indexer-vs-MCP coexistence behaviour, and the
  `db_mtime` reopen logic. What's not covered remains explicitly
  listed.
- [x] **E3** — `MERGE` idempotency verified by
  `crates/codegraph-core/tests/merge_semantics.rs`.

## F. Nice-to-have

- [x] **F1** — `examples/demo-rust/` with a tiny crate (greet + math
  modules), one Markdown doc, and one Gherkin feature. Excluded from
  the workspace so the indexer can be pointed at it.
- [ ] **F2** — Pre-built binaries via `cargo-dist` or a release
  workflow.
- [ ] **F3** — `bdd-viz` rendering directly from velr instead of
  materialising the JSON intermediate (only worth doing if the dataset
  grows past a point where the JSON round-trip matters).

## G. LLM-facing usability (added post-release-readiness)

- [x] **G1** — Markdown-shaped MCP tools: `cypher_md`, `node_md`,
  `history`. All output GFM tables / Markdown dossiers ready to drop
  into a chat reply. Tested in `crates/codegraph-mcp/src/main.rs`.
- [x] **G2** — `:Note` persistence layer with `write_note` /
  `list_notes` MCP tools. Notes are attached via `(:Note)-[:NOTES]->(t)`,
  rejected when the MATCH binds zero targets, and survive `--full`
  reindex.
- [x] **G3** — Real revision history: `:GitCommit` + `:Author` no longer
  wiped on `--full`. First run backfills up to 200 commits; incremental
  runs walk only the new range. Full DAG via `:PARENT_OF`. `:File` and
  `:Function` get `first_seen_commit` / `last_seen_commit`. Parser is
  unit-tested against a temp repo.
- [x] **G4** — Claude skill at `examples/claude-skill/codegraph.md`
  plus repo-level `CLAUDE.md` instructing Claude Code to prefer
  `codegraph` MCP tools over `grep`/`find` and to persist findings as
  notes.

## H. LLM-facing depth (in flight)

These build directly on the G-series Markdown / Notes / revision
foundation. Goal: make `codegraph` the *first* thing the agent reaches
for, with the lowest-token answer for each question shape.

- [x] **H1** — `impact` MCP tool. Transitive blast radius of a node
  via `CALLS*`, `IMPLEMENTED_BY`, `MENTIONS`, `DEFINED_IN`. Returns a
  Markdown report with counts per category and the top-N affected
  nodes. Replaces the "who uses this" crawl before refactors.
- [x] **H2** — `find_symbol(query)`. Fuzzy / substring lookup over
  `:Function` / `:Symbol` qualified names returning a Markdown table of
  `qualified_name`, `file:line`, `signature`. The graph equivalent of
  ⌘-T.
- [x] **H3** — Saved views. `save_view(name, cypher)` MERGEs a
  `:View {name, cypher}` node; `view(name, params)` runs it and
  returns Markdown. Reusable named queries with zero Cypher reasoning
  on the agent side.
- [x] **H4** — `diff_since(commit)`. Walk the `:GitCommit`
  `:PARENT_OF` DAG and list functions/files added/changed/removed
  since the given commit, as a Markdown table. PR-prep / changelog
  generator.
- [x] **H5** — Ranked neighbours in `node_md`. Sort outgoing /
  incoming edges by importance (fan-in/out, recent commit churn) and
  cap at top-N per edge type. Hubs no longer blow up the dossier.
- [x] **H6** — `:Test` label + `[:TESTS]` edge. Discover Rust
  `#[test]` / `#[tokio::test]` functions and link them to the
  function-under-test where derivable. Enables "what changed without
  test coverage" queries.
- [x] **H7** — `:Concept` layer. Cluster `:DocSection`s into
  `:Concept`s; expose `concept(name)` returning a subsystem dossier
  (functions + docs + tests + open notes).
- [x] **H8** — Auto-notes from PR comments. `gh pr view --comments`
  parsed into `:Note`s attached to referenced symbols. Long-term
  memory from existing review activity.
- [x] **H9** — `watch_node` triggers. Mark a node as watched; the next
  indexer run writes a `:Note` describing what changed, so the agent
  is notified asynchronously across sessions.

## I. Live indexing & MCP plumbing (post-H)

- [x] **I1** — `codegraph-indexer` library refactor.
  `pub fn run_indexer(opts: IndexOptions) -> Result<IndexStats, String>`
  is the entry point; `main.rs` is a ~60 LoC CLI wrapper. Embedders
  call `IndexOptions::new(workspace, db).with_paths(rel_paths)` for
  live-mode reindexing.
- [x] **I2** — `--watch <workspace>` mode for `codegraph-mcp`. Spawns a
  `notify`-based filesystem watcher in a background thread. Debounced
  (default 500ms) reindex of only the changed paths. Live mode skips
  git history and the sidecar — the persistent revision history only
  advances on actual `git commit`.
- [x] **I3** — Persistent `LspPool` reused across watch passes. Each
  language server pays its cold-start cost (~5s rust-analyzer init
  + 15s workspace index) on the first batch only; subsequent batches
  reuse the live process, send `didChange` for known files, and skip
  most of the warm-up sleep. `index_status` exposes `live_lsps`.
- [x] **I4** — `index_status` MCP tool. `state` / `last_run_at` /
  `last_run_mode` / `last_run_duration_ms` / `head_hash` /
  `last_paths` / `last_error`. Lets the agent wait for `idle` after a
  save before issuing fresh queries.
- [x] **I5** — Per-pass `[:CALLS]` scoping (bug fix). The
  unconditional global `[:CALLS]` wipe nuked the whole call graph on
  every incremental pass; now scoped to current-pass callers, so
  unchanged files keep their CALLS edges in live mode.

## J. Future-ideas reach-down (post-I)

- [x] **J1** — `coverage_md` MCP tool. Single Markdown report of the
  graph's dim spots: orphan functions, untested functions ranked by
  fan-in, files with no notes, packages with zero doc-mentions.
- [x] **J2** — `explore` (token-budgeted) MCP tool. BFS from a seed,
  score `degree + 4·has_notes + 2·has_mentions − 5·depth`, greedily
  fill a Markdown report until `char_budget` is exhausted; footer
  reports drops.
- [x] **J3** — Sidecar feedback-loop filter on the watcher path
  filter. `*.codegraph-meta.json` and `*.db*` no longer trigger
  reindex — closes the obvious feedback loop.

## K. Refactor pass (post-J)

- [x] **K1** — Plan staged refactor in [`refactoring.md`](refactoring.md).
- [x] **K2** — `chrono_now_iso` extracted into `codegraph-core::time`.
- [x] **K3** — `parse_node_address(_with_defaults)` consistent across
  `node_md` / `impact` / `explore` / `watch` / `unwatch`.
- [x] **K4** — `mcp/src/main.rs` split into `util` / `render` / `tx` /
  `watch` modules. Down from 4256 → 3522 LoC.
- [x] **K5** — Indexer phase split: `phase_history`,
  `phase_test_tagging`, `phase_watch_triggers`, `save_sidecar`. The
  orchestrator's tail dropped from ~145 LoC inline to ~10 LoC.
- [ ] **K6** — Per-tool handler split in `mcp/src/main.rs`. Still
  3522 LoC; ~3000 LoC of handlers could move into `tools/<name>.rs`
  files. Mechanical, deferred until next concrete itch.
- [ ] **K7** — `IndexCtx` / `tools::Ctx` structs. Skipped: current
  signatures aren't painful; pull forward when we add a cross-cutting
  concern (per-call timing, logging) that justifies the indirection.

More ambitious follow-ups (reverse-Markdown round-trip, cross-repo
federation, MCP Resources) live in [`future-ideas.md`](future-ideas.md).

## What's left

- **C2** — needs the actual GitHub repository URL.
- **C5** — needs an installed Rust 1.75 toolchain to verify locally;
  CI does verify on every push.
- **F2** / **F3** — pure nice-to-haves, not blockers.
- **K6** / **K7** — see refactor section.
- Smaller follow-ups noted in `journal.md`: workDoneProgress wait
  instead of fixed sleeps in LSP, chunking for huge `IN [...]` lists,
  persistent LSP across MCP restarts, save-time `:GitCommit` overlay.

Everything else is done. `cargo build --workspace`,
`cargo test --workspace` (**69 tests**), `cargo fmt --all -- --check`
and `cargo clippy --workspace --all-targets -- -D warnings` all pass.
