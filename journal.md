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
