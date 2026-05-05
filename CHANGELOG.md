# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
