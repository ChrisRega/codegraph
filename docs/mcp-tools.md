# `codegraph-mcp` tools

`codegraph-mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io/)
over stdio. After `initialize`, the following tools are advertised on
`tools/list` and dispatched on `tools/call`.

## `schema`

Lists all vertex labels and edge types observed in the database, plus a
short Cypher cheat-sheet. No arguments. Use this first when wiring up
an LLM session — the result describes the graph the model is allowed to
query.

## `cypher`

Executes a single openCypher query (read or write) and returns the row
table as TSV.

| arg | type | notes |
| --- | --- | --- |
| `query` | string, required | full openCypher statement |

Errors come back with `isError: true` and the velr error message in
`text` content.

## `begin`

Opens a buffered transaction. Subsequent `write` calls accumulate; only
`commit` applies them. `begin` is idempotent — calling it on an already
open transaction is a no-op that reports the buffer size.

| arg | type | notes |
| --- | --- | --- |
| `message` | string, optional | free-form label kept in memory only |

## `write`

Inside a transaction, validates and buffers a write. Outside, applies
it immediately as a one-shot velr `run`.

| arg | type | notes |
| --- | --- | --- |
| `query` | string, required | Cypher write statement |

## `commit`

Replays every buffered query in order inside one velr `begin_tx()` and
commits. If any single query fails, the transaction rolls back and no
queries are persisted.

## `rollback`

Discards buffered queries and closes the transaction. Reports how many
queries it dropped.

## `explain`

Returns velr's planner trace for a query. velr 0.2.x's `ExplainTrace`
type doesn't yet implement `Display`, so this currently returns a stub
acknowledging the trace was produced. Will be wired up properly when
velr exposes a printable plan.

| arg | type | notes |
| --- | --- | --- |
| `query` | string, required | |

## Auto-reopen behaviour

Before every dispatch, `codegraph-mcp` `stat`s the database file. If its
mtime is newer than what was opened with **and** no transaction is
buffered, the velr handle is closed and reopened transparently. This
lets a long-running MCP server pick up an external indexer run.
