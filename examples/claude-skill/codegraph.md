---
name: codegraph
description: Use the `codegraph` MCP server as the primary way to explore this codebase. Prefer it over `grep`/`find` for navigation, definitions, callers, BDD coverage, doc-to-code mentions, and API surface. Always render results as Markdown via `cypher_md` / `node_md`, and persist non-trivial findings as `:Note` nodes via `write_note` so future sessions can pick them up.
---

# codegraph skill

This project ships a graph index of the codebase exposed over MCP under the
server name **`codegraph`**. The graph contains:

- `:Workspace`, `:Package`, `:File`
- `:Function`, `:Symbol`, `:Field`, `:Parameter`, `:Import`
- `:Doc`, `:DocSection` with `MENTIONS` edges into code
- `:Feature`, `:Scenario`, `:Step` with `IMPLEMENTED_BY` edges
- `:APIEndpoint`, `:APIType`
- `:GitCommit`, `:Author` (revision history)
- `:Note` (free-form Markdown notes you write back into the graph)

See `docs/schema.md` for the full schema.

## When to use codegraph instead of grep / find / read

Default to codegraph for **navigation and reasoning** tasks. Reach for raw
file reads only when you actually need to see source.

| Task | Preferred tool |
| --- | --- |
| "Where is `foo` defined?" | `mcp__codegraph__cypher_md` on `:Function`/`:Symbol` |
| "Who calls `foo`?" | `cypher_md` over `(:Function)-[:CALLS]->()` |
| "What does this file expose?" | `mcp__codegraph__node_md` with `label='File'` |
| "Show me everything connected to function X" | `node_md` with `label='Function'` |
| "Which BDD scenarios cover this function?" | `cypher_md` over `(:Step)-[:IMPLEMENTED_BY]->()` |
| "What docs reference this symbol?" | `cypher_md` over `(:DocSection)-[:MENTIONS]->()` |
| "What changed historically?" | `mcp__codegraph__history` |
| "Read the actual implementation" | filesystem `Read` (only after locating it via codegraph) |

## Operating rules

1. **Always start with `schema`.** Call `mcp__codegraph__schema` once per
   conversation to see which labels and edge types are actually populated for
   this repo. Don't assume — alpha schemas drift.

2. **Prefer Markdown-shaped tools.** Use `cypher_md` over `cypher`, and
   `node_md` over hand-rolled queries when you want a node summary. The
   Markdown output is meant to be dropped straight into your reply or into a
   note. Only use raw `cypher` (TSV) when you need to post-process the rows
   programmatically.

3. **Persist findings as `:Note` nodes.** When you discover something
   non-obvious (a hidden coupling, a subtle invariant, a TODO buried in a
   call chain, a design decision the user just confirmed), write it back
   with `mcp__codegraph__write_note`. Attach to the relevant node so future
   sessions surface it via `node_md`. Example:

   ```
   write_note(
     match    = "MATCH (t:Function {qualified_name: 'crate::foo::bar'})",
     title    = "bar() must be called under the lock from baz()",
     markdown = "Discovered 2026-05-05 while debugging #423 — calling without the lock corrupts X.",
     author   = "claude",
     tags     = "concurrency,gotcha"
   )
   ```

4. **Recall before re-deriving.** Before doing a deep dive, check whether
   notes already exist for the area you're investigating: `list_notes` with
   a `match` clause that selects the relevant subgraph.

5. **Writes are real.** `write`, `commit`, `write_note` mutate the graph on
   disk. Don't experiment with destructive Cypher (`DETACH DELETE`,
   `REMOVE`) unless the user explicitly asked. Reads (`cypher`, `cypher_md`,
   `node_md`, `schema`, `explain`, `history`, `list_notes`) are safe.

6. **Re-index when stale.** If the graph contradicts what you see in the
   files, the index is behind the working tree. Tell the user and suggest
   running `codegraph-indexer --workspace . --db ./codegraph.db`.

## Quick recipes

**Find a definition without `grep`:**
```
cypher_md("MATCH (f:Function) WHERE f.name = 'parse_markdown' \
           RETURN f.qualified_name, f.path, f.line ORDER BY f.path")
```

**Inspect a node:**
```
node_md(label='Function', key='qualified_name', value='codegraph_indexer::main::run')
```

**Find dead code (functions with no callers and no BDD step):**
```
cypher_md("MATCH (f:Function) \
           WHERE NOT EXISTS { MATCH ()-[:CALLS]->(f) } \
            AND NOT EXISTS { MATCH (:Step)-[:IMPLEMENTED_BY]->(f) } \
           RETURN f.qualified_name, f.path LIMIT 50")
```

**See revision history captured in the graph:**
```
history(limit=20)
```
