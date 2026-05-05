# codegraph

A code-indexing toolchain that builds a queryable graph of your codebase and exposes it to LLM agents via MCP. Backed by [velr](https://crates.io/crates/velr), an embedded property-graph database with openCypher.

> **Status:** alpha. Public-API surfaces and on-disk format may change.

## Crates

| Crate | Purpose |
| --- | --- |
| [`codegraph-core`](crates/codegraph-core) | Shared velr adapter, query helpers, Cypher value escaping. |
| [`codegraph-indexer`](crates/codegraph-indexer) | Walks a workspace and writes graph data: Rust (`syn`/LSP), Markdown, Gherkin/BDD, OpenAPI specs. |
| [`codegraph-mcp`](crates/codegraph-mcp) | MCP server that exposes the resulting graph as Claude Code / Claude Desktop tools (`cypher`, `schema`, `write`, ...). |

## Quick start

```bash
# build everything
cargo build --workspace --release

# index the current repository into ./codegraph.db
./target/release/codegraph-indexer index --root . --db ./codegraph.db

# expose it to Claude as an MCP tool
./target/release/codegraph-mcp ./codegraph.db
```

Add to your `claude_desktop_config.json` / `.claude.json`:

```json
{
  "mcpServers": {
    "codegraph": {
      "command": "/abs/path/to/codegraph-mcp",
      "args": ["/abs/path/to/codegraph.db"]
    }
  }
}
```

## Incremental indexing

`codegraph-indexer` uses git diff (when the workspace is a git repo) plus file mtimes to skip unchanged files. There is no in-database time-travel — the graph reflects the most recent index pass.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
