# `codegraph-mcp` tools

`codegraph-mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io/)
over stdio. After `initialize`, the following tools are advertised on
`tools/list` and dispatched on `tools/call`.

## `schema`

Lists all vertex labels and edge types observed in the database, plus a
short Cypher cheat-sheet. No arguments. Use this first when wiring up
an LLM session — the result describes the graph the model is allowed to
query.

## `explore`

Token-budgeted graph exploration. Identifies a seed node, BFS-walks
outward up to `max_depth`, scores each discovered neighbour, then
greedily fills a Markdown report from highest-scoring downward until
`char_budget` is exhausted. The output footer reports how many
candidates were dropped so the agent knows whether to raise the budget
or pivot.

The intended use: replace the multi-call pattern of
`node_md(seed)` → `node_md(neighbour_1)` → `node_md(neighbour_2)` …
with one bounded call.

| arg | type | notes |
| --- | --- | --- |
| `label` | string, required | seed label (`Function`, `File`, …) |
| `key`   | string, required | identifying property (`qualified_name`, `path`) |
| `value` | string, required | property value of the seed |
| `char_budget` | integer, optional | rough output ceiling, default `8000` |
| `max_depth`   | integer, optional | BFS depth cap (default `2`, max `4`) |

Scoring: `degree + 4·has_notes + 2·has_doc_mentions − 5·depth`. Higher
fan-in nodes win, with a bonus for annotation. Each enrichment
(degree, has_notes, has_mentions) is a single batched query keyed off
all discovered qualified names, so the call is `1 + 2·max_depth + 3`
DB round-trips regardless of subgraph size.

## `coverage_md`

Single Markdown report surfacing the dim spots of the graph — the
`grep -L` of code intelligence. Sections:

1. **Orphan functions** — `:Function`s with no inbound `[:CALLS]`.
   Excludes `:Test`. Either entry points (CLI `main`, public API) or
   genuinely dead code.
2. **Untested functions, ranked by `[:CALLS]` fan-in** — non-test
   functions with no inbound `[:TESTS]`. Sorted by how many callers
   depend on them, so the top of the list = best ROI for adding a
   test.
3. **Files with no `:Note`** — `:File`s nobody has annotated via
   `write_note`.
4. **Packages with zero doc-mentions** — `:Package`s whose files
   contain no function mentioned by any `:DocSection`.

| arg | type | notes |
| --- | --- | --- |
| `limit` | integer, optional | max rows per category, default `15` |

Implementation note: velr 0.2.16's planner rejects
`WHERE NOT n:Label AND NOT (pattern)` in a single clause and does not
support `EXISTS { MATCH ... }` subqueries. The handler issues a small
fan-out of plain queries and combines them with client-side
set-difference / filtering — robust against planner shape limits at the
cost of two extra round-trips.

## `index_status`

Reports the live indexer's state when the MCP server was started with
`--watch <workspace>`. No arguments. Returns a Markdown summary with:

- `state` — `idle` or `running`
- `runs_total`, `last_run_at`, `last_run_mode` (`live` / `incremental` /
  `full` / `noop` / `error`), `last_run_duration_ms`
- `head_hash` short prefix at the time of the last run
- `last_paths` — workspace-relative paths from the most recent batch
  (capped at 20 in the rendered output)
- `last_error` if the previous run failed

Use this to wait until pending edits are reflected before issuing fresh
queries: when `state` is `idle`, the most recent debounced batch is
fully applied. Without `--watch`, the response makes the no-op explicit
("Live indexer is **not running**") so an LLM doesn't poll forever.

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

## `node_md` (ranked output)

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

Within each edge group, neighbours are sorted **by total degree
(in + out) descending**, then alphabetically. Hubs surface first so
the per-group `neighbours_limit` cap doesn't silently drop the most
load-bearing neighbour. Each row gets a trailing `_(deg N)_` tag when
the degree is non-zero. Degree lookup is best-effort: if the
aggregating query fails, ordering degrades to alphabetical without
erroring.

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

## `watch`, `unwatch`, `list_watches`

Cross-session change notifications. `watch(label, key, value)` adds the
`:Watch` label to a node and snapshots the current `body` as
`watch_baseline_body`, plus the current HEAD hash as
`watch_set_at_commit`. The next indexer run compares each
`:Watch`'s current `body` against the baseline; on mismatch, it
attaches a `:Note` (tagged `watch-trigger`, authored
`codegraph-indexer`) to the node and re-baselines, so a single change
produces exactly one trigger note.

`unwatch` removes the `:Watch` label and the three watch_* properties.
`list_watches` returns a Markdown table of all watched nodes.

| tool | arg | type | notes |
| --- | --- | --- | --- |
| `watch` | `label`, `key`, `value` | string, required | identifies the node |
| `unwatch` | `label`, `key`, `value` | string, required | |
| `list_watches` | (none) | | |

The trigger fires on `body` change only. Anything that doesn't show up
in the LSP body slice (e.g. an attribute moved outside the symbol
range, a doc comment edit beyond the slice) won't fire. This is a
known limit; covering it would require diffing more state at watch
time.

## `import_pr_notes`

Bulk-imports a list of PR / code-review comments as `:Note` nodes
attached to any `:Function` they reference.

For each comment, every backtick-delimited token in the body that looks
like an identifier (`[A-Za-z0-9_:.]+`, ≤ 120 chars, optional trailing
`()` stripped) is looked up against `Function.name` *and*
`Function.qualified_name`. Tokens inside fenced ``` ``` ``` blocks are
skipped. If at least one `:Function` matches, one `:Note` is created
with `tags='pr-comment'` and attached to *all* matched functions via
`[:NOTES]`.

| arg | type | notes |
| --- | --- | --- |
| `comments` | array, required | each item: `{author, body, url}` (extra fields ignored) |
| `pr` | string, optional | used in note title and id; defaults to `unknown` |

Suggested workflow:

```bash
gh pr view 42 --json comments,number \
  | jq '{pr: (.number|tostring), comments: .comments}' \
  > /tmp/comments.json
# then call import_pr_notes with that JSON
```

## `define_concept`, `concept`, `list_concepts`

User-curated subsystem labels. A `:Concept` is a node with a name and
description; `[:DESCRIBES]` edges link it to whatever nodes the user
declared as part of the subsystem (typically a mix of `:DocSection`,
`:Function`, `:Package`).

`define_concept(name, match, description?)` MERGEs the `:Concept` and
attaches `[:DESCRIBES]->t` to every node bound by the supplied MATCH
clause. Same `t`-binding contract as `write_note`.

`concept(name)` renders a Markdown dossier:
- description + creation timestamp
- direct members (whatever the DESCRIBES edges point at)
- functions in scope (members that are `:Function`, plus functions
  mentioned by member `:DocSection`s)
- tests covering those functions (via `[:TESTS]`)
- notes attached to those functions

`list_concepts` enumerates everything as a Markdown table with member
counts.

`:Concept` nodes survive `--full` reindex (excluded from the wipe set).

| tool | arg | type | notes |
| --- | --- | --- | --- |
| `define_concept` | `name` | string, required | identifier-like |
| `define_concept` | `match` | string, required | binds variable `t` |
| `define_concept` | `description` | string, optional | |
| `concept` | `name` | string, required | |
| `list_concepts` | (none) | | |

## `diff_since`

Reports what landed between a baseline `:GitCommit` and HEAD. HEAD is
identified by the `[:SNAPSHOT_OF]->(:Workspace)` edge; the baseline is
resolved against `c.hash` first, then `c.short_hash`. Lists commits in
the open-closed interval `(baseline, HEAD]`, then `:File` and
`:Function` whose `first_seen_commit` is one of those commits.

| arg | type | notes |
| --- | --- | --- |
| `commit` | string, required | full hash or short_hash of the baseline |
| `limit`  | integer, optional | per-category cap, default `50` |

**Removals are not reported.** The indexer detaches deleted nodes on
each pass and does not keep tombstones; reconstructing what existed at
an older snapshot would require either tombstones or an external
`git log -S<symbol>` cross-reference. The output includes a footer
making this explicit so an LLM doesn't infer "no removals" from the
absence of a Removed section.

**Implementation note.** The baseline lookup is two `WHERE x = ?`
queries instead of one `WHERE x = ? OR y = ?`, because velr 0.2.16's
planner expands `OR` into a `UNION` that conflicts with the trailing
`LIMIT`.

## `save_view`, `view`, `list_views`

Persist reusable Cypher queries as `:View` nodes that survive `--full`
reindex (the wipe set excludes `:View`).

`save_view` MERGEs a `:View {name}` and stores `cypher`,
`description`, `created_at`, `updated_at`. Names must match
`[A-Za-z0-9_-]{1,80}`.

`view` looks up the saved cypher, substitutes `$key` tokens against the
supplied `params` object (each value is escaped via `escape_str`), runs
the result, and renders it as a Markdown table. Unknown tokens fall
through unchanged so they show up in the rendered cypher block. Updates
`v.last_run_at`.

`list_views` returns every saved view as a Markdown table.

| tool | arg | type | notes |
| --- | --- | --- | --- |
| `save_view` | `name` | string, required | identifier-like |
| `save_view` | `cypher` | string, required | may contain `$tokens` |
| `save_view` | `description` | string, optional | one-line summary |
| `view` | `name` | string, required | |
| `view` | `params` | object, optional | substitution map |
| `list_views` | (none) | | |

## `find_symbol`

Fuzzy substring search over `:Function` and `:Symbol` nodes (case-insensitive,
matched against both `qualified_name` and `name`). Results are joined to
their defining `:File` via `[:DEFINED_IN]` and ranked client-side: exact
match, then `name` startsWith, then `qualified_name` startsWith, then
contains. Ties break on shorter `name` first, then lexicographic `qn`.

Returns a Markdown table with `kind` (label + the Rust/LSP `kind` slug),
`qualified_name`, `file:line`, and the first non-empty line of `body` as
a signature.

| arg | type | notes |
| --- | --- | --- |
| `query` | string, required | substring; trimmed; case-insensitive |
| `limit` | integer, optional | default `25` |
| `kind`  | string, optional | exact match against `s.kind`, e.g. `'fn'`, `'struct'` |

Implementation note: candidate fetch is two `:Function` / `:Symbol`
queries with `LIMIT 5000`, then merged + filtered + ranked in Rust.
Avoids depending on velr's substring-match primitives directly.

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
