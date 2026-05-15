# codegraph

A code-indexing toolchain that builds a queryable property graph of your
codebase and exposes it to LLM agents via [MCP](https://modelcontextprotocol.io/).
Backed by [velr](https://crates.io/crates/velr), an embedded graph
database with openCypher.

> **Status:** alpha. velr 0.2.x is itself alpha; backend behaviour may
> change. The on-disk format and graph schema are not yet stable.

## What's in the graph

The indexer walks your repo and projects it into one connected graph:

```
:Workspace ─CONTAINS─→ :Package ─CONTAINS─→ :File
                       :Package ─DEPENDS_ON─→ :Package
:File ←DEFINED_IN─ :Function | :Symbol
:Function ─CALLS─→ :Function          (LSP outgoingCalls)
:Doc ─HAS_SECTION─→ :DocSection ─MENTIONS─→ :Function | :Symbol
:Feature ─HAS_SCENARIO─→ :Scenario ─HAS_STEP─→ :Step ─IMPLEMENTED_BY─→ :Function
:Package ─EXPOSES─→ :APIEndpoint | :APIType
```

Full reference: [`docs/schema.md`](docs/schema.md).

## Crates

| Crate | Purpose |
| --- | --- |
| [`codegraph-core`](crates/codegraph-core) | Shared velr adapter, owned `Cell` / `Table` types, Cypher value escaper. |
| [`codegraph-indexer`](crates/codegraph-indexer) | Walks a workspace and writes graph data: Rust (LSP), TypeScript / Node (LSP), Python (LSP), Markdown, Gherkin / BDD, OpenAPI, GraphQL SDL, Protobuf. Plus `bdd-viz` HTML renderer. |
| [`codegraph-mcp`](crates/codegraph-mcp) | MCP server exposing the graph as Claude Code / Claude Desktop tools (`cypher`, `schema`, `write`, `begin`, `commit`, `rollback`, `explain`). |

## Quick start

```bash
# 1. build everything
cargo build --workspace --release

# 2. index your repository
./target/release/codegraph-indexer --workspace . --db ./codegraph.db

# 3. serve it to Claude
./target/release/codegraph-mcp --db ./codegraph.db
```

Subsequent indexer runs are incremental: a sidecar
`./codegraph.db.codegraph-meta.json` records the last-indexed git
commit and `git diff` selects which files to re-parse. Pass `--full` to
force a clean rebuild.

### MCP wiring

For Claude Desktop / Claude Code, add to `claude_desktop_config.json`
(or the per-project `.claude.json`):

```json
{
  "mcpServers": {
    "codegraph": {
      "command": "/abs/path/to/codegraph-mcp",
      "args": ["--db", "/abs/path/to/codegraph.db"]
    }
  }
}
```

The full list of tools and their JSON Schemas is in
[`docs/mcp-tools.md`](docs/mcp-tools.md).

## Example queries

```cypher
// every BDD scenario whose steps don't all resolve to a function
MATCH (sc:Scenario)-[:HAS_STEP]->(st:Step)
WHERE NOT EXISTS { MATCH (st)-[:IMPLEMENTED_BY]->(:Function) }
RETURN sc.qualified_name, count(st) AS missing
ORDER BY missing DESC

// who calls `format_table`?
MATCH (caller:Function)-[:CALLS]->(:Function {name: 'format_table'})
RETURN caller.qualified_name

// docs that talk about a function in src/main.rs
MATCH (s:DocSection)-[:MENTIONS]->(fn:Function)-[:DEFINED_IN]->(f:File {path: 'src/main.rs'})
RETURN s.qualified_name, fn.qualified_name
```

## Language-server requirements

The indexer requires an LSP for the chosen language to be on `$PATH`:

- Rust: [`rust-analyzer`](https://rust-analyzer.github.io/)
- TypeScript / JavaScript: [`typescript-language-server`](https://github.com/typescript-language-server/typescript-language-server)
- Python: [`pyright-langserver`](https://github.com/microsoft/pyright)

Override the binary with `--lsp <path>`.

## Using codegraph from Claude Code

The MCP server exposes Markdown-shaped tools designed to drop straight
into an LLM reply, plus a `:Note` mechanism for persisting findings:

| tool | purpose |
| --- | --- |
| `schema` | enumerate labels and edge types — call first |
| `cypher` / `cypher_md` | run a Cypher query (TSV / Markdown table) |
| `node_md` | full dossier of a node (props + neighbours + notes) |
| `write_note` | attach a Markdown `:Note` to any node |
| `list_notes` | list notes (optionally filtered to a subgraph) |
| `history` | walk `:GitCommit` snapshots stored in the graph |
| `impact` | transitive blast radius of a node (callers, callees, mentions, scenarios) |
| `begin` / `write` / `commit` / `rollback` | buffered transactions |
| `explain` | velr planner trace |

A Claude skill that wires these tools into Claude Code's default
behaviour ("prefer the graph over `grep`/`find`, persist findings as
notes") ships at
[`examples/claude-skill/codegraph.md`](examples/claude-skill/codegraph.md).
Copy it to `~/.claude/skills/codegraph.md` (user-wide) or
`.claude/skills/codegraph.md` (per project).

## Revision history in the graph

`codegraph-indexer` records git history as it indexes. The first run on
a repository (or any `--full` rebuild) backfills up to
`HISTORY_BACKFILL_LIMIT` (default 200) commits reachable from `HEAD`;
incremental runs walk only the commits between the previously indexed
`HEAD` and the new one. The full DAG is materialised:

```
(:Author)-[:AUTHORED]->(:GitCommit)-[:PARENT_OF]->(:GitCommit)
                       (:GitCommit)-[:SNAPSHOT_OF]->(:Workspace)   -- HEAD only
```

`:File` and `:Function` carry `first_seen_commit` / `last_seen_commit`
properties, updated on every indexer pass. `:GitCommit`, `:Author` and
user-written `:Note` nodes survive `--full` reindex — the wipe set is
limited to source-derived labels.

## Development

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Outstanding work is tracked
in [`TODO.md`](TODO.md).

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the
Apache-2.0 license, shall be dual-licensed as above, without any
additional terms or conditions.
