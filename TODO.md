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

## What's left

The remaining open items are:

- **C2** — needs the actual GitHub repository URL.
- **C5** — needs an installed Rust 1.75 toolchain to verify locally;
  CI does verify on every push.
- **F2** / **F3** — pure nice-to-haves, not blockers.

Everything else is done. `cargo build --workspace`,
`cargo test --workspace`, `cargo fmt --all -- --check` and
`cargo clippy --workspace --all-targets -- -D warnings` all pass.
