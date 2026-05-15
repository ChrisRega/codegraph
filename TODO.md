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
- [ ] **H8** — Auto-notes from PR comments. `gh pr view --comments`
  parsed into `:Note`s attached to referenced symbols. Long-term
  memory from existing review activity.
- [ ] **H9** — `watch_node` triggers. Mark a node as watched; the next
  indexer run writes a `:Note` describing what changed, so the agent
  is notified asynchronously across sessions.

More ambitious follow-ups (token-budgeted explore, reverse-Markdown
round-trip, coverage heatmap, cross-repo federation, MCP Resources)
live in [`future-ideas.md`](future-ideas.md).

## What's left

The remaining open items are:

- **C2** — needs the actual GitHub repository URL.
- **C5** — needs an installed Rust 1.75 toolchain to verify locally;
  CI does verify on every push.
- **F2** / **F3** — pure nice-to-haves, not blockers.
- **H1**–**H9** — see section above.

Everything else is done. `cargo build --workspace`,
`cargo test --workspace` (37 tests), `cargo fmt --all -- --check` and
`cargo clippy --workspace --all-targets -- -D warnings` all pass.
