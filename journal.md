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

## H2 — `find_symbol`

- **Reached for:** `grep -rn 'CREATE (.*:Function' crates/codegraph-indexer/src/`
  — one shot, found the property shape (`qualified_name`, `name`,
  `kind`, `line_start`, `line_end`, `body`). The graph would have given
  me the same answer faster: `node_md(label='Function', ...)` would
  have surfaced *its own* schema. Bootstrapping is a recurring theme.
- **Defensive choice:** ranking happens in Rust, not Cypher. velr 0.2.16
  *might* support `toLower()` + `CONTAINS` + `STARTS WITH` correctly,
  but I'd rather pull a generous candidate set (`LIMIT 5000`) and rank
  client-side than discover a planner edge case at runtime. The
  trade-off is an extra `:Function` + `:Symbol` table scan per query.
- **First-pass test fail:** my assertion `md.contains("`format`")` was
  looking for a literally backtick-wrapped `format`, but the rendered
  qualified_name is `` `a::format` ``, so the inner string never had a
  backtick directly preceding `format`. Tightened to `a::format`.
  Lesson: when asserting on Markdown output, escape-aware matching
  beats clever substring tricks.
- **Wish #3:** the indexer should attach a stable `signature` property
  on `:Function` (the first line of `body`, normalized). I'm computing
  it on every `find_symbol` call; pre-computed at index time it'd halve
  the row size in the candidate scan.
- **Tests:** `find_symbol_ranks_exact_above_substring` (4 fns,
  asserts ordering exact > startsWith > contains) and
  `find_symbol_returns_no_match_message`. mcp suite 16/16.

## H3 — Saved views

- **Reached for:** zero greps. Self-contained: I knew the patterns from
  H1/H2. The `escape_str` discipline + `safe_*` validators are now
  muscle memory. This is what reuse looks like.
- **Friction:** `cargo fmt` ran in the previous step's combined command
  *after* my staged edits, which invalidated three `Edit` calls (the
  file mtime had advanced). Had to re-`Read` and re-edit. Lesson: when
  fmt is bundled at the end of a check, don't queue dependent edits in
  the same batch as a build that runs fmt.
- **Design call:** the `$token` substitution is intentionally dumb — a
  byte-walk that recognises identifier-shaped tokens after `$` and
  swaps in `escape_str(value)`. Unknown tokens fall through unchanged
  (no error) so the rendered cypher block shows the user exactly what
  ran. velr has no real prepared statements, so this is the best we
  can do without writing a Cypher lexer.
- **Persistence story:** `:View` survives `--full` because the wipe set
  is restrictive (only source-derived labels). No indexer change
  needed — the existing G3 design already accounts for this class of
  user-state node.
- **Wish #4:** a `delete_view(name)` tool. Trivial to add but I'm
  staying disciplined about scope creep within H3.
- **Tests:** `substitute_view_params_replaces_tokens` (escaping +
  unknown-token passthrough), `save_view_then_view_runs_with_params`
  (round-trip + appears in `list_views`), `view_unknown_name_…`,
  `save_view_rejects_invalid_name`. mcp suite 20/20.
