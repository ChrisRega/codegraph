# Future ideas

Speculative / ambitious follow-ups for `codegraph`. These are *not*
scheduled — they live here so the design space is preserved when a
concrete itch shows up.

## 10. Token-budgeted exploration

`explore(starting_node, budget=2000_tokens)` does a heuristic BFS from
a starting node and returns the most informative Markdown that fits in
the budget.

- ranking: edge-count, doc-coverage, recent commit churn, presence of
  `:Note`s
- output: a single Markdown document the LLM can drop verbatim, with a
  "more available" footer pointing at the next-best subgraph

**Why interesting.** Replaces the "10× node_md" pattern with one call
that gives the agent the best slice of the graph for a fixed cost.

## 11. Reverse-Markdown round-trip

`update_node_md(label, key, markdown)` accepts the same Markdown shape
that `node_md` emits, diffs it against the current node + its `:Note`s,
and MERGEs the changes back.

- Effectively turns the graph into a wiki the LLM (or a human in an
  editor) can edit by sending Markdown.
- Hard part: defining the canonical schema for "this Markdown means
  these properties / these notes / these edges" without ambiguity.

## 12. Graph-coverage heatmap

`coverage_md` returns a Markdown table of the dimmest spots in the
graph: files with no notes, modules with no doc-mentions, public
functions with no inbound `CALLS`, test-less code paths.

- Onboarding goldmine: "where should I start reading?" → the *opposite*
  of the dim spots.
- Refactor-risk goldmine: "what's load-bearing but undocumented?" →
  high fan-in × low doc-coverage.

## 13. Cross-repo federation

`--include-deps` makes the indexer follow `Cargo.lock` /
`package-lock.json` and index referenced source crates as separate
`:Workspace`s. Edges from your code into a dep become
`:CALLS_EXTERNAL`.

- Cypher queries can then span your stack: "do any of my callers of
  `tokio::spawn` violate the runtime requirement of crate X?"
- Storage gets large fast; want a "depth = 1" gate and probably a
  shared on-disk cache so multiple projects reuse one snapshot per dep
  version.

## 14. MCP Resources for live dossiers

Instead of forcing a tool round-trip, expose every node as an MCP
resource — `codegraph://node/Function/foo::bar` — that the client can
include in its context window directly.

- Saves the JSON-RPC overhead and lets the user `@`-mention nodes in
  Claude Desktop.
- Needs the MCP server to advertise the resource list (potentially
  huge); a paginated `resources/list` plus a "frequently accessed"
  prefilter would handle that.
