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

## H4 — `diff_since`

- **Reached for:** `grep -n 'first_seen\|last_seen\|DETACH DELETE...'`
  on the indexer to confirm the data shape before designing the diff.
  Critical because my first instinct ("just list removed nodes") was
  wrong: the indexer doesn't keep tombstones — removed `:Function`s
  are gone from the graph entirely. The grep saved me from shipping a
  query that would always return zero removals while pretending to
  enumerate them.
- **velr surprise #1:** `WHERE c.hash = $g OR c.short_hash = $g …
  LIMIT 1` errors with `LIMIT clause should come after UNION not
  before`. velr's planner rewrites `OR` into a `UNION`, and the
  combined parse rejects a leading `LIMIT`. Workaround: two sequential
  `WHERE x = ?` queries with `or_else`. Worth noting in
  `docs/velr-notes.md` (deferred — not in this commit's scope).
- **Honesty in the output:** the report includes a footer stating
  removals aren't tracked. An LLM reading the dossier would otherwise
  infer "no Removed section ⇒ nothing was removed", which is false.
  Negative-evidence framing is part of the API surface here.
- **Bug bounce:** the `format!("…literal…")` in `added_section`
  triggered `clippy::useless_format`. Fixed by switching to a bare
  `&str`. Reminder to *always* run clippy in the test loop, not just
  fmt+test.
- **Wish #5:** the indexer should write a per-commit `(:GitCommit)
  -[:ADDED]->(:Function)` and `[:REMOVED]->` edge alongside the
  current first/last_seen properties. That would make `diff_since`
  precise and would unlock a "function lifespan" view. Big change —
  punted to `future-ideas.md` material.
- **Tests:** `diff_since_lists_commits_and_added_nodes` (3-commit
  setup, asserts only mid + new functions show up in the range while
  the pre-baseline `old::a` is excluded), and
  `diff_since_unknown_commit_returns_message`. mcp suite 22/22.

## H5 — Ranked neighbours in `node_md`

- **Reached for:** `Read` of the `render_neighbours` body — already had
  the call-site context from H1/H2/H3. No greps. The local change is
  small but nuanced.
- **Design call:** ranking is one extra aggregating query per
  direction (`OPTIONAL MATCH (m)-[r]-() RETURN m.qualified_name,
  count(r)`). One query per direction beats N queries per neighbour,
  and degrades gracefully — if velr trips on the implicit grouping,
  `neighbour_degrees` returns an empty map and ordering falls back to
  alphabetical (no error path bubbles up).
- **Trade-off accepted:** degree is total fan, not weighted by edge
  type. A `:File` with many `[:CONTAINS]` edges outranks a heavily-
  called `:Function`, which is arguably wrong. Acceptable for now —
  the LLM-facing improvement (hubs surface before truncation) lands
  cleanly even with the crude metric.
- **Wish #6:** velr should expose a stable degree property cached on
  each node, refreshed during indexing. The aggregation query is
  O(edges) per call, which won't scale on big graphs.
- **Tests:** `node_md_ranks_neighbours_by_degree` (5-node setup with
  one hub, asserts the hub appears before the leaf in the rendered
  Markdown and the `_(deg N)_` tag is present). mcp suite 23/23.

## H6 — `:Test` label and `[:TESTS]` edges

- **Reached for:** `grep -n 'body\|line_start' lsp_index.rs` to confirm
  the LSP body slice includes attribute lines (it does — rust-analyzer
  emits `documentSymbol.range` covering the attributes). Without that
  shape, the body-CONTAINS heuristic would silently miss every test.
- **velr surprise #2 (compounding the H4 surprise):** `MATCH (f) WHERE
  body CONTAINS 'A' OR body CONTAINS 'B' SET f:Test` applies the SET
  to *every* row in the unioned result, not just the WHERE-matching
  ones. velr 0.2.16's planner rewrites `WHERE a OR b` to a UNION and
  then SET fans out across the lot. Worked around by splitting into
  two single-CONTAINS statements. This OR→UNION quirk has now bitten
  me twice (H4, H6); writing it down in `docs/velr-notes.md` is
  overdue but out of scope for this commit.
- **Test caught the bug:** I shipped the OR-form first, the test
  immediately reported `m::foo` (a non-test) tagged. Without the test
  this would have been a silent correctness regression in production
  data. Lesson reinforced: every Cypher post-processing step needs a
  unit test, no matter how short.
- **Honest scope:** `[:TESTS]` is derived from `[:CALLS]` only, so a
  test that asserts on a static value without calling anything won't
  produce edges. Doc says so. A future pass could attribute test
  effects via macro expansion, but that's a different project.
- **Tests:** `phase6_tags_tests_and_links_them` (3 functions: a sync
  test, a tokio test, and a regular fn; asserts the right two carry
  `:Test` and exactly two `[:TESTS]` edges land on the right target).
  Workspace 36/36.
