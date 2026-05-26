---
name: codegraph
description: Use the `codegraph` MCP server as the primary way to explore and reason about this codebase. Prefer it over `grep`/`find` for navigation, definitions, callers, BDD coverage, doc-to-code mentions, test coverage, blast-radius analysis, and any subgraph dossier. Always render results as Markdown via `cypher_md` / `node_md` / `explore` / `coverage_md`, and persist non-trivial findings as `:Note` nodes via `write_note` so future sessions can pick them up. When `--watch` is enabled, call `index_status` after edits to wait until the graph is up to date.
---

# codegraph skill

Project ships a graph index of the codebase exposed over MCP under the
server name **`codegraph`**. The graph is built by
`crates/codegraph-indexer` (LSP-driven for Rust / TypeScript / Python,
plus Markdown / Gherkin / OpenAPI / GraphQL SDL / Protobuf passes) and
queried via openCypher.

## Tool surface

Sorted by frequency you should reach for them in a typical session.

### Navigation (read-only)

| Tool | Use when |
| --- | --- |
| `index_status` | **Always first** if `--watch` mode is on — wait until `state: idle` after a save before issuing fresh queries. Also surfaces which LSP processes the pool is keeping warm and the last batch of changed paths. |
| `schema` | **First call of any session.** Shows currently-populated vertex labels + edge types. Some labels are conditional (`:Note`, `:View`, `:Concept`, `:Watch`, `:Test`) and only appear when data exists. |
| `find_symbol(query)` | ⌘-T equivalent. Fuzzy, ranked (exact > startsWith on name > startsWith on qn > contains). Returns `file:line` and the first body line as signature. Start here when you only know a partial name. |
| `node_md(label, key, value)` | Full dossier for one node: properties + neighbours grouped by edge type + attached notes. Neighbours ranked by total degree so hubs surface first. |
| `cypher_md(query)` | Arbitrary openCypher read, rendered as a GFM table. Use this for one-off questions the named tools don't cover. |
| `cypher(query)` | Same but TSV — only use when you need to post-process. |
| `explore(label, key, value, char_budget, max_depth)` | Token-budgeted BFS. Replaces the multi-`node_md`-call pattern when you want a bounded subgraph dossier. Footer reports drops so you know whether to raise the budget. |
| `impact(value, depth, top)` | Transitive blast radius for a `:Function`: callers + callees (BFS over `[:CALLS]`) plus doc mentions and BDD scenarios. Use before any refactor. |
| `coverage_md(limit)` | The "dim spots" report — orphan functions, untested functions ranked by fan-in, files with no notes, packages with zero doc mentions. Onboarding hot list. |
| `dead_code(exclude_tests?, ignore_test_callers?, kind?, name_skip?, limit?)` | `:Function`s with no incoming `:CALLS` — graph-derived suspicious functions. Hint generator, not a verdict: `main`, public API, FFI, string-matched dispatch and trait impls look dead because the caller side isn't in the AST. Defaults exclude `:Test` candidates and count test-only callers as life; flip `ignore_test_callers=true` for a "covered-only-by-tests" sweep. `name_skip` filters obvious entry-point prefixes (`main`, `handle_`, `phase_`). |
| `graph_export(label, key, value, depth?, format?, max_nodes?)` | Node-centered subgraph as Mermaid `flowchart LR` (default) or Graphviz DOT. BFS from seed, clamped depth 1–3, capped at 200 nodes. Output is fenced so it round-trips into GitHub / chats / `:Note` bodies. Neighbour identity uses coalesce(qualified_name, id, path, name, hash). At depth > 1, only nodes with the seed's label/key continue the BFS. |
| `arch_overlay(workspace_name?)` | Subprocesses `claude -p` to derive an `:ArchModule` overlay on the live DB: gathers `:Package` + hot `:Function`s + cross-package `:CALLS` density, asks the agent for a 3–7-module coarse architecture, writes back `:ArchModule` + `[:CONTAINS]`→`:Package` + `[:GROUPS]`→`:Function` + `[:USES]` edges with `semantic_kind`/`description`/`layer_hint`. In-session counterpart to `codegraph-indexer --full --with-arch-agent`. Real cost: one API call + a few seconds. Failures degrade silently — check stderr. Use this when the user wants a fresh "what does this repo decompose into" view, then visualise with `graph_export(label="ArchModule", …)`. |
| `diff_since(commit, limit)` | What landed between a baseline `:GitCommit` and HEAD. Lists commits in range + `:File`/`:Function` whose `first_seen_commit` lands inside. **Removals are not tracked** (no tombstones in the indexer). |
| `history(limit)` | List `:GitCommit` snapshots recorded in the graph, newest first. |

### Reasoning / curation (writes through MCP)

| Tool | Use when |
| --- | --- |
| `write_note(match, markdown, title?, author?, tags?)` | Persist a finding. Attach to one or more target nodes via a Cypher MATCH binding `t`. Notes survive `--full` reindex and surface in `node_md` automatically. |
| `list_notes(match?, limit?)` | List notes, optionally filtered to a subgraph (same `t`-binding contract). **Recall first** — check what previous sessions noted before re-deriving. |
| `define_concept(name, match, description?)` | Create a `:Concept` collecting an ad-hoc subgraph via `[:DESCRIBES]`. |
| `concept(name)` | Rolled-up dossier for a concept: members + functions in scope + `:Test`s covering them + attached `:Note`s. |
| `list_concepts` | All concepts as a table. |
| `save_view(name, cypher, description?)` / `view(name, params?)` / `list_views` | Persist + replay parameterised Cypher queries as `:View` nodes. Use for queries you find yourself running repeatedly. `$key` tokens in the saved cypher get substituted via `escape_str` at run time. |
| `watch(label, key, value)` / `unwatch` / `list_watches` | Mark a node so the next indexer pass attaches a `:Note` tagged `watch-trigger` when its body changes. Cross-session async notifications. |
| `import_pr_notes(comments, pr?)` | Bulk-import `gh pr view --json comments` output as `:Note`s on referenced `:Function`s. |
| `worklog_create(title, area?, kind?, status?, comment?, author?, id?, match?)` | Create a `:WorklogItem` with an initial `:Status` (default `pending`). `kind` classifies the work: `bug` \| `feature` \| `task` \| `refactor` \| `perf` \| `docs` (default `task`) — mirrors Conventional-Commits prefixes. Optional first `:Comment` and `[:RELATES_TO]` edges (Cypher MATCH binding `t`). Use this when starting any non-trivial work the user wants tracked across sessions. |
| `worklog_set_status(id, status, comment?, author?)` | Append a new `:Status` to an existing item (status is append-only — never destructive). Allowed: `pending`, `in_progress`, `done`, `blocked`, `abandoned`. Attach a comment that summarises why the transition happened. |
| `worklog_comment(id, body, author?)` | Attach a `:Comment` to the **latest** `:Status` of an item. Use this for thoughts that arrive AFTER the transition — retros, follow-up findings, lessons learned. |
| `worklog_list(area?, status?, kind?, limit?)` | Markdown table of items, optionally filtered by area / current_status / kind. Sorted by latest status timestamp. **Recall first** — check what's already in flight before starting parallel work. Common patterns: `worklog_list(kind="bug", status="done")` for recent fix retros (PR-prep gold), `worklog_list(status="in_progress")` for pickup. |
| `worklog_md(id)` | Full dossier for one item: metadata, related nodes, the chronological `:Status` timeline, and all `:Comment` threads nested under each status. |

### Transactional writes (escape hatch)

| Tool | Use when |
| --- | --- |
| `begin(message?)` / `write(query)` / `commit` / `rollback` | Buffered multi-statement transaction. `begin` opens, `write` accumulates, `commit` replays all inside one velr `begin_tx`. Use when the targeted MCP tools don't cover what you need to express. |
| `explain(query)` | velr planner trace — useful when a query is unexpectedly slow. |

## Schema (full design surface)

Labels that appear depending on what's been written:

- **Always:** `:Workspace`, `:Package`, `:File`, `:Function`, `:Symbol`,
  `:Doc`, `:DocSection`, `:Feature`, `:Scenario`, `:Step`,
  `:APIEndpoint`, `:APIType`, `:Field`
- **Revision history (accumulate across `--full`):** `:GitCommit`,
  `:Author`
- **Conditional on user/tool activity:** `:Note` (from `write_note` /
  `import_pr_notes`), `:View` (from `save_view`), `:Concept` (from
  `define_concept`), `:Watch` (from `watch`), `:WorklogItem` /
  `:Status` / `:Comment` (from the `worklog_*` tools)
- **Derived during Phase 6:** `:Test` (added to `:Function`s whose body
  contains `#[test]` / `#[tokio::test]`)
- **Optional agent overlay (Phase 5b):** `:ArchModule` from
  `arch_overlay` or `--with-arch-agent`. Wiped + re-derived per agent
  run; visualise with `graph_export`.

Key edges:

- Structural: `CONTAINS`, `DEFINED_IN`, `DEPENDS_ON`, `EXPOSES`,
  `USES_SCHEMA`
- Code: `CALLS` (rebuilt every pass), `TESTS` (derived from `[:CALLS]`
  where the source carries `:Test`)
- Docs: `HAS_SECTION`, `MENTIONS` (doc → fn), `LINKS_TO` (doc → file)
- BDD: `HAS_SCENARIO`, `HAS_STEP`, `IMPLEMENTED_BY`
- Revision: `AUTHORED`, `PARENT_OF`, `SNAPSHOT_OF`
- Annotation: `NOTES` (`:Note` → any), `DESCRIBES` (`:Concept` → any)
- Worklog: `HAS_STATUS` (`:WorklogItem` → `:Status`, append-only),
  `HAS_COMMENT` (`:Status` → `:Comment`, 1:n),
  `RELATES_TO` (`:WorklogItem` → any code/doc node)
- Architecture (agent overlay): `CONTAINS` (`:ArchModule` → `:Package`),
  `GROUPS` (`:ArchModule` → `:Function` for sub-package splits),
  `USES {semantic_kind}` (`:ArchModule` → `:ArchModule`),
  `REFERENCES` (`:GitCommit` → `:WorklogItem`, from `Refs:`-trailer
  parsing in commit messages)

Full reference: `docs/schema.md`.

## velr 0.2.x planner gotchas (learned the hard way)

These bit me while building the tools — bake them into your queries:

1. **No `EXISTS { MATCH ... }` subqueries.** velr 0.2.16 errors with
   "tried to match MultiPartQuery". Use `WHERE NOT (pattern)` or
   client-side set-difference instead.
   ```cypher
   // BAD: MATCH (p:Package) WHERE NOT EXISTS { MATCH (p)-[:CONTAINS]->(:File) } ...
   // GOOD: two queries (all packages, packages with files), set-diff in Rust.
   ```

2. **`WHERE a OR b` rewrites to `UNION`** which clashes with `LIMIT`
   placement AND with subsequent `SET` clauses. Split into separate
   single-condition statements.
   ```cypher
   // BAD: MATCH (c:GitCommit) WHERE c.hash = $g OR c.short_hash = $g LIMIT 1
   // BAD: MATCH (f) WHERE f.body CONTAINS 'a' OR f.body CONTAINS 'b' SET f:Tag
   // GOOD: try one key, fall back; or split into two SET statements.
   ```

3. **Label predicate combined with existential predicate** in one WHERE
   errors with "Stage3 bind-table existential filtering". Drop the
   label filter from Cypher, apply client-side.
   ```cypher
   // BAD: WHERE NOT f:Test AND NOT (f)<-[:TESTS]-(:Test)
   // GOOD: WHERE NOT (f)<-[:TESTS]-(:Test) ;; filter test_qns out in Rust.
   ```

4. **No `$param` placeholders.** All Cypher is built by string
   composition with `codegraph_core::escape_str(value)` /
   `escape_ident(name)`. Never `format!`-splice raw user input —
   always go through those helpers.

5. **`IN [...]` with many items + write clause OOMs** the planner.
   Anything past a few hundred elements combined with `SET`/`DELETE`
   can blow up to multi-GB heap with no forward progress. Chunk at
   ~100 elements per query.

6. **No variable-length paths** (`-[:CALLS*1..3]->`) reliably. Do
   bounded BFS client-side instead — see `impact` and `explore` for
   the pattern.

## Operating rules

1. **Always start with `schema`.** Once per conversation. Some labels
   are conditional; don't assume.

2. **If `--watch` is on, call `index_status` after the user (or you)
   saves a file, and WAIT until `state: idle` before issuing queries.**
   Live mode `DETACH DELETE`s the changed file's functions before
   re-creating them — querying mid-pass returns "Not found" for nodes
   that exist in the source. `last_paths` confirms the watcher picked
   up the change. `head_hash` confirms which commit the graph reflects.

3. **Prefer named tools over hand-rolled Cypher** — `find_symbol`,
   `node_md`, `impact`, `explore`, `coverage_md`, `diff_since` cover
   the common shapes and emit well-formatted Markdown ready to drop
   into a reply.

4. **Markdown by default.** Use `cypher_md` over `cypher`. Use the
   structured tools above when they fit.

5. **`cypher_md` is also a smoke test.** Before writing handler code
   that depends on a query shape, run the query through `cypher_md`.
   You'll catch velr planner edge cases at design time instead of at
   build time. Three smoke calls before coding can save a build-fix
   cycle.

6. **Recall before re-deriving.** Run `list_notes` filtered to the
   relevant subgraph, or `concept(name)` if the area you're touching
   has been collected before. Surface what previous sessions found
   instead of re-running the investigation.

7. **Persist findings as `:Note` nodes.** When you discover something
   non-obvious — a hidden coupling, a subtle invariant, a TODO buried
   in a call chain, a design decision the user just confirmed — write
   it back with `write_note`. Future `node_md` calls on the same node
   surface it automatically. Notes survive both `--full` reindex and
   live-mode file reparses (snapshotted by `(note_id, target_kind,
   target_identity)` and restored after the wipe).

8. **Writes are real.** `write_note`, `define_concept`, `save_view`,
   `watch`, and any `write`+`commit` mutate the graph on disk. Don't
   experiment with destructive Cypher (`DETACH DELETE`, `REMOVE`)
   unless the user asked. All `*_md` / `node_md` / `schema` / `explain`
   / `history` / `list_*` are safe reads.

9. **Re-index when stale.** If `index_status.head_hash` lags behind
   the working tree, or if the graph contradicts what you see in the
   files, the index is behind. With `--watch` this fixes itself on
   the next save. Without, suggest
   `cargo run --release -p codegraph-indexer -- --workspace . --db ./codegraph.db`.

10. **Track non-trivial work in the graph-backed worklog, not in
    free-form Markdown.** The `worklog_*` tools are the canonical
    record of "what's open, what shipped, how did it go" across
    sessions and projects. The pattern:

    - **At task start:** `worklog_create(title, area, kind,
      status="in_progress", comment="why this matters / what's the
      plan", match=<optional Cypher binding `t` to the code nodes
      this touches>)`. `kind` ∈ {`bug`, `feature`, `task`,
      `refactor`, `perf`, `docs`} — same vocab as Conventional
      Commits, so it mirrors the eventual commit prefix. The
      `match` clause attaches `[:RELATES_TO]` edges so later
      `node_md(those_fns)` calls surface the worklog item.
    - **At meaningful transitions:** `worklog_set_status(id, "done"
      | "blocked" | "abandoned", comment="what changed and why")`.
      `:Status` is append-only — every transition is a new node, so
      the full timeline survives. Don't try to edit prior statuses.
    - **For thoughts that arrive later** (retro lessons, follow-up
      findings, a related bug discovered while shipping): use
      `worklog_comment(id, body)`. Comments attach to the *current*
      status, so observations land in the right slice of the
      timeline.
    - **Before starting parallel work** or to pick up a previous
      session: `worklog_list(status="in_progress")` /
      `worklog_list(area="foo")` then `worklog_md(id)` for the full
      timeline. Treat it the same way you'd treat `list_notes` —
      recall before re-deriving.
    - **For human-readable export** (PR descriptions, status
      reports): run `codegraph-mcp report --db <db> --out <dir>` to
      regenerate `ROADMAP.md` + `WORKLOG.md`. These are *generated
      artefacts* — never hand-edit them; mutate the graph via the
      `worklog_*` tools and re-render.

    Worklog nodes survive `--full` reindex (same protection as
    `:Note` / `:Concept` / `:View`), so the timeline accumulates
    across the project's whole life — including across CG upgrades.
    Per-project: each repo has its own `codegraph.db` and therefore
    its own worklog; cross-project tracking is out of scope today.

11. **For "what does this repo decompose into" questions, run
    `arch_overlay` once and then `graph_export`.** The agent overlay
    is opt-in (costs an API call) and re-derivable. Use it when the
    user asks for an architecture diagram, a "give me the lay of the
    land" sweep, or wants to validate that a refactor preserves
    module boundaries. The overlay sits in a separate label
    (`:ArchModule`) so it composes with everything else — link a
    `:WorklogItem` to the module it touches via
    `worklog_create(match="MATCH (t:ArchModule {name: 'mcp-server'})")`.

    Two ways to trigger:
    - **In-session (this is the default):** `arch_overlay()` — uses
      the live DB, takes seconds, and surfaces the new module count
      in the response. Pair it with `cypher("MATCH (a:ArchModule)
      RETURN a.name, a.semantic_kind, a.layer_hint ORDER BY
      a.layer_hint, a.name")` for the textual view, or
      `graph_export(label="ArchModule", key="name", value="<name>")`
      for a Mermaid diagram.
    - **Out-of-session (during a CLI reindex):** add
      `--with-arch-agent` to a `codegraph-indexer --full` invocation.
      Same code path, no MCP server needed; useful for CI / cron
      that wants the overlay refreshed alongside a full reindex.

    Both require the `claude` CLI in `PATH`. Failures (CLI missing,
    parse error, exit-nonzero) degrade silently — the previous
    overlay is wiped first, so the graph ends with no
    `:ArchModule` rather than a partial one. The error message tells
    you exactly which mode you hit.

## Quick recipes

**Locate a definition without `grep`:**
```
find_symbol(query="parse_markdown")
```
Returns `qualified_name`, `file:line`, signature. One call, no
filesystem read needed.

**Full dossier for a node (use after `find_symbol` to get the qn):**
```
node_md(label="Function", key="qualified_name",
        value="codegraph-indexer::main::run")
```

**Blast radius before a refactor:**
```
impact(value="codegraph-indexer::lsp_index::index_files_via_lsp", depth=3, top=15)
```

**Bounded subgraph exploration when you don't know what to ask:**
```
explore(label="File", key="path", value="crates/codegraph-mcp/src/main.rs",
        char_budget=8000, max_depth=2)
```

**Find dead code + untested code in one shot:**
```
coverage_md(limit=20)
```
Replaces the old `WHERE NOT EXISTS { ... }` recipe — that syntax
isn't supported by velr 0.2.x anyway (see gotcha #1).

**What landed since some commit:**
```
diff_since(commit="abc1234", limit=50)
```

**Persist a finding so the next session sees it:**
```
write_note(
  match    = "MATCH (t:Function {qualified_name: 'crate::foo::bar'})",
  title    = "bar() must be called under the lock from baz()",
  markdown = "Discovered 2026-05-05 while debugging #423 — calling without the lock corrupts X.",
  author   = "claude",
  tags     = "concurrency,gotcha"
)
```

**See revision history:**
```
history(limit=20)
```

**Start tracking a non-trivial task (and link it to the code it touches):**
```
worklog_create(
  title   = "Migrate auth from session cookies to JWT",
  area    = "auth",
  kind    = "feature",
  status  = "in_progress",
  comment = "Why: legal requires shorter token lifetime. Plan: rewrite middleware first, then migrate clients in batches.",
  match   = "MATCH (t:Function) WHERE t.qualified_name STARTS WITH 'crate::auth::middleware'"
)
```
Returns an `id` like `wl-migrate-auth-...`. Hold onto it for the
status transitions.

**Append a status transition with a one-line retro:**
```
worklog_set_status(
  id      = "wl-migrate-auth-20260516T1200",
  status  = "done",
  comment = "Shipped 2026-05-20. Discovered the existing refresh-token path was unused — deleted ~200 LoC. Migration was harder than the JWT swap itself."
)
```

**Add a lesson learned after the fact:**
```
worklog_comment(
  id   = "wl-migrate-auth-20260516T1200",
  body = "Next time: audit clients FIRST. Half the planning effort assumed real client diversity that didn't exist."
)
```

**Pick up a previous session — what's open in this project right now?**
```
worklog_list(status="in_progress")
worklog_md(id="<the-relevant-id>")
```

**Regenerate human-readable docs from the graph (run from repo root):**
```
codegraph-mcp report --db ./codegraph.db --out docs/
```
Produces `docs/ROADMAP.md` (current state grouped by area + status,
done items kept not deleted) and `docs/WORKLOG.md` (chronological log
with full timeline + nested comments).

**Reusable named query:**
```
save_view(
  name        = "orphan-fns",
  cypher      = "MATCH (f:Function) WHERE NOT (f)<-[:CALLS]-(:Function) AND f.kind = $kind RETURN f.qualified_name LIMIT 100",
  description = "Functions of given kind with no callers"
)
view(name="orphan-fns", params={"kind": "Method"})
```
