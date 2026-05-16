# codegraph

**A queryable graph of your codebase, wired into your LLM agent.**

`codegraph` indexes your repository into an embedded property graph
([velr](https://crates.io/crates/velr), openCypher) and serves it to
Claude Code / Claude Desktop over [MCP](https://modelcontextprotocol.io/).
The agent stops `grep`ping and starts asking real structural questions:
*who calls this?*, *what does this doc mention?*, *what changed since
the last release?*, *what's still in flight?*

It also gives the agent a **persistent memory** — notes, concepts,
and a structured worklog are stored as graph nodes that survive
re-indexing, so findings from one session compound across the next.

> **Status:** alpha. velr 0.2.x is itself alpha; the on-disk format
> and graph schema are not yet stable.

---

## Why a graph

`grep` finds strings; the agent then has to reconstruct call chains,
test coverage, doc coverage, and revision history by re-reading files.
That's slow and burns context. A graph turns "who tests this?" or
"what mentions this function?" into one query that returns a Markdown
table the agent drops straight into its reply.

Compared to embedding-based code RAG:

- **Deterministic.** Same query, same answer; nothing to re-rank.
- **Structural.** Edges encode real call/test/mention/commit
  relationships, not "lexically similar."
- **Compositional.** Cypher lets the agent express joins the embedding
  index cannot — *"untested functions touched in the last 5 commits
  that a doc section mentions."*
- **Writable.** The agent annotates the graph (`:Note`, `:Concept`,
  `:WorklogItem`) so investigations persist.

---

## What's in the graph

```
:Workspace -CONTAINS-> :Package -CONTAINS-> :File
                       :Package -DEPENDS_ON-> :Package
:File   <-DEFINED_IN- :Function | :Symbol
:Function -CALLS-> :Function           (via LSP outgoingCalls)
:Test (label on :Function) -TESTS-> :Function
:Doc -HAS_SECTION-> :DocSection -MENTIONS-> :Function | :Symbol
:Feature -HAS_SCENARIO-> :Scenario -HAS_STEP-> :Step -IMPLEMENTED_BY-> :Function
:Package -EXPOSES-> :APIEndpoint | :APIType
:Author -AUTHORED-> :GitCommit -PARENT_OF-> :GitCommit -SNAPSHOT_OF-> :Workspace
:Note -NOTES-> (anything)                            -- agent memory
:Concept -DESCRIBES-> (anything)                     -- subsystem groupings
:WorklogItem -HAS_STATUS-> :Status -HAS_COMMENT-> :Comment   -- project log
            -RELATES_TO-> (anything)
```

Full reference: [`docs/schema.md`](docs/schema.md).

---

## 60-second start

```bash
# build
cargo build --workspace --release

# index your repo
./target/release/codegraph-indexer --workspace . --db ./codegraph.db

# serve it to Claude (with live reindex on save)
./target/release/codegraph-mcp --db ./codegraph.db --watch .
```

Subsequent indexer runs are incremental: a sidecar
`./codegraph.db.codegraph-meta.json` tracks the last-indexed git commit
and `git diff` selects which files to re-parse. Pass `--full` to force
a clean rebuild.

### Wire it into Claude

`claude_desktop_config.json` (or per-project `.claude.json`):

```json
{
  "mcpServers": {
    "codegraph": {
      "command": "/abs/path/to/codegraph-mcp",
      "args": [
        "--db",    "/abs/path/to/codegraph.db",
        "--watch", "/abs/path/to/your/repo"
      ]
    }
  }
}
```

With `--watch`, the MCP server runs a debounced filesystem watcher
(default 500 ms) and reindexes only the changed files. The persistent
revision history (`:GitCommit` / `:Author`) advances only on actual
`git commit`; uncommitted edits show up as a draft overlay so
`diff_since(HEAD)` reflects unstaged work.

Drop the Claude skill at
[`examples/claude-skill/codegraph.md`](examples/claude-skill/codegraph.md)
into `~/.claude/skills/codegraph.md` (user-wide) or
`.claude/skills/codegraph.md` (per project). It teaches Claude Code to
prefer graph queries over `grep`/`find` and to persist findings as
notes and worklog items.

---

## The agent loop

This is what makes `codegraph` more than "a search tool with extra
steps." The agent has read tools, write tools (memory), and a workflow
that compounds across sessions:

```
       ┌─ recall ──────────────────────────────────────┐
       │  list_notes, worklog_list, concept(name)      │
       │  → "what did past sessions already find?"     │
       │                                               │
       v                                               │
  ┌─────────┐   ┌─────────┐   ┌──────────┐             │
  │ explore │ → │ node_md │ → │  impact  │ → decision  │
  └─────────┘   └─────────┘   └──────────┘             │
       │                                               │
       v                                               │
       ├─ persist findings ────────────────────────────┤
       │  write_note  → annotate a node                │
       │  worklog_*   → track the task end-to-end      │
       │  define_concept → group a subsystem           │
       └───────────────────────────────────────────────┘
```

Read tools (`explore`, `node_md`, `impact`, `find_symbol`, `coverage_md`,
`diff_since`) answer structural questions. Write tools (`write_note`,
`worklog_*`, `define_concept`, `save_view`, `watch`) carry forward
what the agent learns. Next session's `node_md(some_fn)` automatically
surfaces the notes and worklog items attached.

---

## Tool surface

Sorted by frequency.

### Navigation (read)

| Tool | Use when |
| --- | --- |
| `schema` | First call of any session. Lists currently-populated labels + edge types. |
| `find_symbol(q)` | Fuzzy substring lookup over `:Function` / `:Symbol`. Ranked. The graph equivalent of ⌘-T. |
| `node_md(label, key, value)` | Full dossier for one node: properties + neighbours grouped by edge type + attached notes + linked worklog items. |
| `cypher_md(query)` | Arbitrary openCypher, rendered as a GFM table. |
| `explore(label, key, value, char_budget)` | Token-budgeted BFS dossier — bounded subgraph in one call. |
| `impact(value, depth, top)` | Transitive blast radius of a `:Function`: callers + callees + doc mentions + BDD scenarios. |
| `coverage_md(limit)` | Dim-spots report — orphan functions, untested functions ranked by fan-in, files with no notes. |
| `diff_since(commit)` | What landed between a baseline `:GitCommit` and HEAD. Picks up uncommitted edits via the `:WorkingTree` overlay. |
| `history(limit)` | `:GitCommit` snapshots, newest first. |
| `index_status` | Live indexer state — wait for `idle` after a save before querying. |

### Memory (writes)

| Tool | Use when |
| --- | --- |
| `write_note(match, markdown, ...)` | Persist a finding. Attach to any node via a Cypher MATCH binding `t`. |
| `list_notes(match?, limit?)` | Recall before re-deriving. |
| `define_concept(name, match)` / `concept(name)` / `list_concepts` | User-curated subsystem groupings, queryable as a rolled-up dossier. |
| `save_view(name, cypher)` / `view(name, params)` / `list_views` | Parameterised reusable Cypher, stored as `:View` nodes. |
| `watch(label, key, value)` / `unwatch` / `list_watches` | Cross-session "tell me if this function changes." Next indexer pass attaches a `watch-trigger` note. |
| `import_pr_notes(comments, pr)` | Turn `gh pr view --json comments` output into `:Note`s on referenced functions. |

### Worklog (project log in the graph)

| Tool | Use when |
| --- | --- |
| `worklog_create(title, area?, kind?, status?, comment?, match?)` | Open a tracked task. `kind` ∈ {`bug`, `feature`, `task`, `refactor`, `perf`, `docs`}. Optional `match` attaches `[:RELATES_TO]` edges to code nodes. |
| `worklog_set_status(id, status, comment?)` | Append a status transition. `:Status` is append-only so the full history survives. |
| `worklog_comment(id, body)` | Attach a `:Comment` to the latest status — for thoughts that arrive *after* the transition. |
| `worklog_list(area?, status?, kind?)` | Filtered table. Common: `worklog_list(kind="bug", status="done")` for fix retros (PR-prep gold). |
| `worklog_md(id)` | Full dossier: metadata + related code nodes + chronological timeline with nested comments. |

### Transactional + escape hatch

| Tool | Use when |
| --- | --- |
| `begin` / `write` / `commit` / `rollback` | Buffered multi-statement transaction. Replays inside one velr `begin_tx`. |
| `cypher(query)` | Same as `cypher_md` but TSV (use only when post-processing). |
| `explain(query)` | velr planner trace for slow queries. |

Full schemas in [`docs/mcp-tools.md`](docs/mcp-tools.md).

---

## Example queries

```cypher
// every BDD scenario whose steps don't all resolve to a function
MATCH (sc:Scenario)-[:HAS_STEP]->(st:Step)
WHERE NOT (st)-[:IMPLEMENTED_BY]->(:Function)
RETURN sc.qualified_name, count(st) AS missing
ORDER BY missing DESC

// who calls `format_table`?
MATCH (caller:Function)-[:CALLS]->(:Function {name: 'format_table'})
RETURN caller.qualified_name

// docs that mention a function in src/main.rs
MATCH (s:DocSection)-[:MENTIONS]->(fn:Function)-[:DEFINED_IN]->(f:File {path: 'src/main.rs'})
RETURN s.qualified_name, fn.qualified_name

// untested public functions ranked by callers
MATCH (f:Function {kind: 'fn'})
WHERE NOT (f)<-[:TESTS]-(:Function)
WITH f, count{ (f)<-[:CALLS]-(:Function) } AS fanin
RETURN f.qualified_name, fanin ORDER BY fanin DESC LIMIT 20

// recent bug retros for the changelog
MATCH (w:WorklogItem {kind: 'bug', current_status: 'done'})
      -[:HAS_STATUS]->(:Status {text: 'done'})-[:HAS_COMMENT]->(c:Comment)
RETURN w.title, c.body ORDER BY w.current_status_at DESC LIMIT 10
```

---

## Generated docs from the worklog

```bash
codegraph-mcp report --db ./codegraph.db --out docs/
```

Produces `docs/ROADMAP.md` (current state grouped by area + status, done
items kept not deleted with timestamps) and `docs/WORKLOG.md`
(chronological log with full status timeline and nested comment
threads). Re-generate whenever you want a fresh snapshot — the graph
is the source of truth, the Markdown is the export.

This repo's own `docs/ROADMAP.md` and `docs/WORKLOG.md` are produced
this way.

---

## Revision history in the graph

The first run on a repository (or any `--full` rebuild) backfills up to
200 commits reachable from `HEAD`; incremental runs walk only the
range between the previously indexed `HEAD` and the new one.

```
(:Author)-[:AUTHORED]->(:GitCommit)-[:PARENT_OF]->(:GitCommit)
                       (:GitCommit)-[:SNAPSHOT_OF]->(:Workspace)
```

`:File` and `:Function` carry `first_seen_commit` /
`last_seen_commit`. `diff_since` walks the `[:PARENT_OF]` DAG.

A pseudo `:GitCommit:WorkingTree` overlay reflects uncommitted edits
as the SNAPSHOT_OF tip, so `diff_since(HEAD)` sees unstaged work
without polluting the persistent history.

`:GitCommit`, `:Author`, and all agent-written nodes (`:Note`,
`:Concept`, `:View`, `:Watch`, `:WorklogItem` / `:Status` /
`:Comment`) survive `--full` reindex.

---

## Crates

| Crate | Purpose |
| --- | --- |
| [`codegraph-core`](crates/codegraph-core) | Shared velr adapter, owned `Cell` / `Table` types, Cypher value escaper. |
| [`codegraph-indexer`](crates/codegraph-indexer) | Walks a workspace and writes graph data: Rust (LSP), TypeScript / Node (LSP), Python (LSP), Markdown, Gherkin / BDD, OpenAPI, GraphQL SDL, Protobuf. Plus `bdd-viz` HTML renderer. |
| [`codegraph-mcp`](crates/codegraph-mcp) | MCP server + `report` subcommand. Per-tool handlers live in sibling modules (`worklog.rs`, `coverage.rs`, `impact.rs`, …). |

---

## Language-server requirements

The indexer needs the LSP for the chosen language on `$PATH`:

- Rust: [`rust-analyzer`](https://rust-analyzer.github.io/)
- TypeScript / JavaScript: [`typescript-language-server`](https://github.com/typescript-language-server/typescript-language-server)
- Python: [`pyright-langserver`](https://github.com/microsoft/pyright)

Override the binary with `--lsp <path>`.

---

## Development

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — code conventions, test layout.
- [`CLAUDE.md`](CLAUDE.md) — repo-specific guidance for Claude Code.
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — generated from the worklog.
- [`journal.md`](journal.md) — narrative session notes (free-form,
  predates the graph-backed worklog).
- [`docs/velr-notes.md`](docs/velr-notes.md) — velr 0.2.x quirks and
  workarounds discovered the hard way.

---

## License

Dual-licensed under

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the
Apache-2.0 license, shall be dual-licensed as above, without any
additional terms or conditions.
