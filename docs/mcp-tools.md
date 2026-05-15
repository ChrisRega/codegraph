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

Returns velr's planner trace for a query, fetched as the result tables of
`EXPLAIN <query>` via `Db::query_many`.

| arg | type | notes |
| --- | --- | --- |
| `query` | string, required | |

## `cypher_md`

Same as `cypher`, but renders the result as a GitHub-flavoured Markdown
table instead of TSV. Pipes inside cells are escaped, embedded
newlines/tabs collapsed to spaces. Prefer this whenever you want the
rows to drop directly into a doc, note, or chat reply.

| arg | type | notes |
| --- | --- | --- |
| `query` | string, required | |

## `node_md`

Returns a compact Markdown dossier for a single node identified by a
property lookup: properties (as JSON), outgoing edges grouped by edge
type, incoming edges grouped by edge type, and any attached `:Note`s.

| arg | type | notes |
| --- | --- | --- |
| `label` | string, required | bare identifier, e.g. `Function`, `File` |
| `key`   | string, required | bare identifier of the property to match on |
| `value` | string, required | property value (currently always passed as text) |
| `neighbours_limit` | integer, optional | per-edge cap, default `25` |

Both `label` and `key` are validated against `^[A-Za-z_][A-Za-z0-9_]*$`
because they're inlined into the query — invalid input is rejected.

## `write_note`

Attaches a Markdown `:Note` node to one or more existing nodes selected
by a Cypher `MATCH`. Use this to persist findings, design notes,
gotchas — anything you'd otherwise lose at end of session. Future
`node_md` calls on the target surface the notes automatically.

| arg | type | notes |
| --- | --- | --- |
| `match` | string, required | Cypher `MATCH` clause that binds variable `t` |
| `markdown` | string, required | note body |
| `title` | string, optional | one-line title |
| `author` | string, optional | defaults to `claude` |
| `tags` | string, optional | comma-separated tags |

If the `MATCH` binds zero targets, the note is **not** persisted —
`write_note` returns `isError: true` and cleans up the orphan. This
prevents accumulating ghost notes from typo'd MATCH clauses.

`:Note` nodes survive a `--full` reindex (they're part of the persistent
revision/annotation history, not the regenerated source-derived graph).

## `list_notes`

Lists `:Note` nodes as Markdown, newest first. Without arguments it
returns every note. With a `match` clause that binds `t`, only notes
attached to a matched target are returned.

| arg | type | notes |
| --- | --- | --- |
| `match` | string, optional | Cypher MATCH binding `t` |
| `limit` | integer, optional | default `50` |

## `history`

Lists `:GitCommit` snapshots recorded in the graph, newest first, joined
to their `:Author` via the `[:AUTHORED]` edge.

| arg | type | notes |
| --- | --- | --- |
| `limit` | integer, optional | default `50` |

## `impact`

Computes the transitive blast radius of a node. Walks `[:CALLS]` outwards
(callees) and inwards (callers) up to `depth` hops via app-level BFS, and
one-hop for `[:MENTIONS]` (`:DocSection`s) and `[:IMPLEMENTED_BY]`
(`:Step`s). Returns a Markdown report with counts per category and the
top-N affected nodes by discovery order (depth-ascending).

| arg | type | notes |
| --- | --- | --- |
| `value` | string, required | identifying property value, e.g. `'codegraph_indexer::main::run'` |
| `label` | string, optional | default `Function` |
| `key`   | string, optional | default `qualified_name` |
| `depth` | integer, optional | BFS depth for CALLS, default `3`, capped at `6` |
| `top`   | integer, optional | max nodes shown per category, default `15` |

`label` and `key` are validated against `^[A-Za-z_][A-Za-z0-9_]*$` since
they are inlined into the Cypher template. `value` is escaped via
`escape_str`. Returns "Not found" if the seed doesn't exist.

## Auto-reopen behaviour

Before every dispatch, `codegraph-mcp` `stat`s the database file. If its
mtime is newer than what was opened with **and** no transaction is
buffered, the velr handle is closed and reopened transparently. This
lets a long-running MCP server pick up an external indexer run.
