# codegraph build journal

A log of what it felt like to dogfood `codegraph` while building the
H-series features. The premise: I (Claude) should use the
`codegraph` MCP tools for code lookups instead of `grep` / `find` /
`Read`, and note where the experience falls short.

Each entry: feature, what I reached for, what I wished existed.

---

## Setup (velr 0.2.16 bump + planning docs)

- **Used:** `cargo search velr` (Bash), `grep` for the workspace pin,
  one `Edit` to bump it. No graph involved — the dependency version
  isn't in the graph.
- **Used:** plain `Read` / `Write` / `Edit` for `TODO.md`,
  `future-ideas.md`, this file. Project scaffolding lives outside the
  graph.
- **MCP availability:** the `mcp__codegraph__*` tools are *not* wired
  into this Claude Code session. CLAUDE.md says to prefer them; the
  install instructions are in `docs/mcp-tools.md`. So this journal will
  also be a record of *what I would have asked the graph* if it were
  available — a usability proxy.
- **Fallback I'll use:** `grep` / `Read` for code, plus running
  `cargo run --release -p codegraph-mcp -- --db ./codegraph.db` ad-hoc
  if I want to validate a query shape against a real graph.
- **Wish:** the indexer should be runnable as a library so I can spin
  up an in-memory graph from a few files in tests without shelling out
  to LSPs.

## H1 — `impact`

- **Reached for:** `grep -n 'fn handle_\|tool_list' main.rs` to find
  the dispatch and registry. Then a 300-line `Read` for the file
  preamble + handler patterns. Then a 450-line read for the existing
  `node_md` to crib its safe-ident validation and Cypher template.
- **What the graph would have given me:** a `node_md`-style dossier of
  the `handle_*` functions with their definitions, callers, and
  related tests — exactly the thing I'm building. Bootstrapping
  problem: I'm building the tool that would have made building the
  tool faster.
- **Wish #1:** a "show me the source of this `:Function`" tool. The
  graph carries `path` + line numbers; emitting a Markdown code block
  would save me the `Read` step for every handler I need to mimic.
- **Wish #2:** velr `*1..N` variable-path matches would have saved my
  app-side BFS. I went app-side defensively (untested syntax in 0.2.16,
  no time to spelunk). Worth probing in a follow-up.
- **Surprise:** `safe_ident` was duplicated as a closure inside
  `handle_node_md`. Pulling it module-scope as part of H1 was the
  obvious cleanup. The graph would not have surfaced this — it's a
  shape-of-the-source observation, not a relationship one. Lesson:
  the graph isn't an LSP and doesn't replace one.
- **Tests:** `impact_reports_callers_and_callees` (5-node CALLS
  diamond, asserts both transitive directions appear) and
  `impact_handles_unknown_seed` (returns "Not found", not isError).
  Both green; full mcp suite 14/14.
