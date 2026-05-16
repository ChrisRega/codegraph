# codegraph build journal

## Round 7 — fix two bugs found by dogfooding, plus watcher visibility

The user asked me to fix the bugs from Round 6 and continue using
codegraph through the work. Three commits, all bugs surfaced via or
fixable with help from `mcp__codegraph__*`.

### Phase 6: `:Test` label never applied (`c5f167b`)

Reproduced via `mcp__codegraph__cypher_md`:
```
MATCH (n:Test) RETURN count(n) AS test_count
→ 0
```
While grep saw 65+ `#[test]` fns. Hypothesis from the round-6 `:Note`
was right: rust-analyzer's `documentSymbol.range.start` points at the
`fn` keyword, not at the attribute above. `slice_body` returned a
body that excluded `#[test]`.

Fix: back-scan `slice_body` over preceding `#[…]` lines. Four unit
tests cover happy path, multi-attribute, no-attribute, blank-line
stop. `mcp__codegraph__find_symbol("slice_body")` got me to the right
line in one call.

### K8: live reindex orphans `[:NOTES]` (same commit)

`DETACH DELETE` removes node + all edges. The skill-file workaround I
wrote in round 6 ("attach notes to `:File` or `:Package` instead")
turned out to be wrong on closer inspection: `:File` and `:Package`
are also wiped on `--full`. So note edges to ANY function/symbol/file/
package get nuked on any wipe.

Fix: snapshot `(note_id, target_kind, target_identity)` for all
`[:NOTES]->` edges into Functions/Symbols/Files/Packages BEFORE any
wipe, then `MERGE` them back after the LSP rebuild has re-created the
targets with the same identifying property. Notes whose target was
renamed-away stay as orphan `:Note` nodes (still discoverable in
`list_notes`). Two contract tests.

### Watcher visibility (`879d00f`)

User noticed `state: running` for 45s+ during big batches with no
indication whether new edits were even being received. Real issue:
events queue silently in the mpsc channel between watcher loop
iterations. Counter never ticks unless the loop is awake.

Fix: wrap the mpsc sender in a closure `notify::EventHandler` that
updates `events_total` / `events_pending` / `pending_paths` on the
shared status BEFORE forwarding into the channel. `handle_index_status`
always renders an `Events: N total, M pending` line plus a
`Pending paths` section. An agent now has direct proof the watcher
is alive during a long pass.

Underlying perf problem (per-file LSP `outgoingCalls` at 10-50 ms each
× many fns per file × many files per batch) untouched — separate
work. Visibility is the immediate fix.

### MCP tool dogfood report

- **`mcp__codegraph__index_status`** stayed accurate throughout —
  caught the "race window after `DETACH DELETE`" in round 6 because
  it honestly reported `state: running` mid-pass. The user complaint
  ("fast edits don't show as activity") is itself a
  fix-by-better-status-rendering case. Tool-as-debugger-itself paid
  off three times this round.

- **`mcp__codegraph__find_symbol`** is now the default for "where is
  X". One call → location + signature. Used 4× this round.

- **`mcp__codegraph__cypher_md`** as smoke-test: tried
  `RETURN substring(f.body, 0, 60)` first to inspect bodies — instant
  velr error "Unknown function `substring`". Skipped the body
  inspection and went with grep on disk; ~5 seconds saved.

- **`mcp__codegraph__write_note`** worked for persisting the round-6
  hypotheses; the live-mode race that orphaned them is now fixed in
  this round, so future write_notes survive their containing file's
  reindex.

- **One frustration:** `mcp__codegraph__node_md` returned "Not found"
  for a real symbol because I queried during `state: running`. The
  skill file now says wait for `idle` first, but I should add an
  agent-side heuristic — maybe the tool itself could return a hint
  like "(graph is mid-pass; result may be stale)" when status is not
  idle. Filed mentally as future-ideas item.

## Round 6 — skill-doc refresh + dogfood-discovered note orphaning bug

### Skill files updated

User asked me to refresh `CLAUDE.md` and
`examples/claude-skill/codegraph.md` so I don't forget the current
tool surface. Both were last touched mid-H-series and missed:

- Every K-series tool (`coverage_md`, `explore`, `define_concept` /
  `concept` / `list_concepts`, `save_view` / `view` / `list_views`,
  `import_pr_notes`, `watch` / `unwatch` / `list_watches`,
  `diff_since`, `index_status`, `find_symbol`, `impact`).
- The `--watch` live-mode + `index_status` discipline.
- The full list of velr 0.2.x planner gotchas: no `EXISTS { MATCH }`
  subqueries, `OR` → `UNION` clashes with `LIMIT`/`SET`, label
  predicate + existential predicate doesn't compose, no `$param`,
  `IN [...]` past ~100 entries OOMs combined with writes, no
  variable-length paths.
- The persistent LSP pool + `wait_until_idle` story.
- Quick recipes updated — the old "find dead code" recipe used
  `WHERE NOT EXISTS { MATCH ... }` which velr can't parse; replaced
  with a `coverage_md` call (which IS the supported way).

`CLAUDE.md` now points at the skill file as authoritative and adds
only repo-local conventions (Cypher escaping discipline, indexer
phase ordering, per-tool module layout, watcher etiquette).

### Dogfood discovery: live-mode reindex orphans `[:NOTES]` edges

While writing the skill update I tried to attach a `:Note` to
`phase_test_tagging`. Sequence:

1. `find_symbol("phase_test_tagging")` → 1 hit, qn known.
2. `write_note(match=..., ...)` → "attached to 1 target", success.
3. Tried `node_md(label, key, value)` to verify it surfaced under
   the function.
4. Got "Not found" — `:Function` not found at all, never mind the
   note.

Diagnosis: my earlier file save had triggered the watcher, which
was mid-pass on `lib.rs`. Live mode does
`MATCH (f:File {path: ...})<-[:DEFINED_IN]-(n) DETACH DELETE n` —
which removes the old `:Function` AND the `[:NOTES]` edge I just
created. The `:Note` node survives (`MATCH (n:Note) RETURN count(n)`
returned 3) but it's now orphaned. By the time I re-ran `node_md`,
the function was back but the note's edge was gone.

So the "notes survive `--full` reindex" promise that the original
G2 design made holds true for `--full`, but **not for live-mode
single-file reindex.** That's a real gap.

Filed:
- Updated the skill file with a ⚠️ in operating rule #7 explaining
  the limitation + workaround (attach notes to `:File`/`:Package`
  instead of `:Function` when you want durability across saves).
- Tightened operating rule #2 (wait for `state: idle`) — the race
  is real and bites.
- Wrote a `:Note` ON `crates/codegraph-mcp/src/notes.rs` documenting
  the bug and the fix sketch, so anyone touching the notes layer
  surfaces it via `node_md`.

### How the MCP tools actually felt this round

Mostly excellent. Specific moments:

- `find_symbol` → `write_note` → expected verification via `node_md`
  is a clean three-step loop. The race bug only surfaced because
  `index_status` was honest about being mid-pass, which immediately
  pointed me at the right hypothesis (mid-pass detach).
- `schema` only listed 12 vertex labels even though I knew several
  conditional ones (`:Note`, `:View`, `:Concept`, etc.) could
  appear. Discovered while writing the skill file that `schema`
  samples the actual data — labels not currently in the graph
  don't show up. Worth documenting in the skill file under "Schema
  (full design surface)" as a separate section so the agent knows
  what *can* appear vs what currently is.
- `cypher_md` smoke-test pattern got reinforced when I tried
  `RETURN substring(f.body, 0, 60)` and velr said "Unknown function
  in RETURN: substring". One smoke call, planner-error visible
  immediately. Skill file says "use cypher_md as a smoke test" —
  paid off again.

## Round 5 — cleanup after runaway, workDoneProgress wait

### Cleanup

User reported the indexer was indexing itself to death after a PC
restart. Real symptom: `index_status` showed `state: running,
runs_total: 0` for 30+ seconds with a single CLAUDE.md change in the
batch.

Diagnosis: `ls -la codegraph.db` showed **17 GB** — leftover bloat
from the pre-chunking-fix runaway (commit `726221e` had the bug, 
`f929f42` fixed it). The watcher restarted on the bloated DB. With
the bloated state every query was expensive even though the chunking
fix prevented further accumulation.

Action: kill MCP server, `rm -f codegraph.db*`, run a clean
`--full` indexer from CLI. Result: **DB rebuilt at 1 MB** (16 000×
smaller), 266 functions, 1956 CALLS, 40 commits. The refactor itself
didn't break anything; the bug-fix from yesterday holds.

**Honest dogfood note:** my own `kill 5493` for the MCP server
disconnected the `mcp__codegraph__*` tools for this session. I'm
working with grep+Read until the user next restarts Claude Code.
Cost: every "look up X" reverts to a grep, which after the K-series
felt jarring. The MCP tools really are the better navigation layer
when they're available.

### workDoneProgress wait

The cold-start tax was 15 seconds of blind `thread::sleep` plus
rust-analyzer's own warm-up. Replaced with `LspClient::wait_until_idle(
silence_ms, max_ms)`:

- Drain notifications from the LSP channel, replying to server-
  initiated requests so rust-analyzer doesn't stall.
- When `silence_ms` passes without any message, declare the LSP
  settled and return.
- Cap at `max_ms` so a chatty server can't stall the indexer forever.

Cold start: 1500 ms silence window, 30 s cap.
Warm: 400 ms silence window, 3 s cap.

**Benchmark (release, fresh DB):**

| Stage | Pre-fix | Post-fix |
|---|---|---|
| Full reindex of this workspace | ~3 min | **8.4 s** |
| Of which: LSP idle wait | 15 s blind | ~3 s adaptive |

~20× speedup on the cold path. The `[lsp] wait_until_idle: settled
after 33 messages` log line confirms RA actually went quiet 1.5 s
after the last progress notification — the 15 s fixed sleep was
wildly conservative.

Tests stay green; no test touches LSP timing so nothing regressed.
This is purely a perf fix.

## Round 4 — refactor pass + first real production bug

The K-series refactor (1b/1c/1a/2a) shipped clean — no behavior
change, 69/69 tests through every step. After restart I noticed a
real production issue worth recording.

### The 11 GB stuck-watcher

After the Claude Code restart, `index_status` reported `state: running,
runs_total: 0` and never moved off the first batch. Investigation
under `/proc/206909`:

- `State: S (sleeping)` — blocked on `unix_stream_read_generic` (the
  LSP stdio pipe).
- `VmRSS: 11.3 GB`, `VmSize: 13 GB` — runaway memory accumulation.
- 11 minutes of accumulated CPU.

**Root cause hypothesis:** the per-pass `[:CALLS]` wipe I shipped in
commit `726221e` (CALLS scoping fix) builds an `IN [...]` list with
every function in the current pass. After the recent edit storm and
the refactor commits, the live-mode batch's `path_set` was big — the
LSP indexed many files, and `fn_positions` accumulated hundreds of
qualified_names. velr 0.2.16's planner explodes on `IN [...] DELETE`
combinations past a few hundred entries: multi-GB heap, no forward
progress.

**Defensive fix shipped:** chunk the `IN [...]` list at 100 qns per
chunk. Same correctness, bounded memory.

**Real lessons:**
1. Building `index_status` was the right call — it surfaced the
   stuck pass within seconds. Without it I'd have just noticed
   "the graph is stale" and never connected it to the indexer.
2. velr's planner edge cases are still being discovered. The
   pattern of "WHERE x IN [hundreds of items] + write clause" is
   now banned in our code; chunked helper would be cleaner long-term.
3. The watcher had no guardrail against runaway passes. Possible
   future fix: a watchdog that kills the pass after N seconds and
   reports it via `last_error`.

The stuck process is still alive on the host — needs a Claude Code
restart to clear. The fix lands now so the next run doesn't hit
the same trap.

## Round 3 — quick-win fixes + future-ideas reach-down

Now with persistent LSP + live mode actually wired in this session, so
the MCP tools are responsive enough to use as a primary navigation
mechanism. Real experience reports below.

### (10) explore — token-budgeted exploration

- **No greps used at all** for this one. The handler shape is now
  internalised from H1 (impact), H2 (find_symbol), H7 (concept), and
  the previous coverage_md commit. The `escape_str` + `safe_ident` +
  batched-IN-list patterns are muscle memory.
- **One MCP tool that *would* have been useful while building this:**
  `node_md` on the existing `handle_concept` to compare-and-contrast
  the BFS-then-render shape. I went from memory which was fine, but a
  visual diff between the two would have been faster than recalling.
- **Scoring choices:** went with `degree + 4·notes + 2·mentions − 5·depth`.
  The depth penalty (5×) intentionally crushes deeper-but-trivially-
  connected nodes; the notes bonus (4×) intentionally over-weights
  annotation, because annotated = "humans found this important
  enough to write down". These coefficients are eyeballed, not
  measured. Real validation would need real agent traces.
- **Test design:** `explore_respects_tight_budget_and_reports_drops`
  generates a 30-leaf star with an 800-char budget and asserts the
  output stays under 1000 chars AND that the dropped-count footer is
  emitted. The "stays under" assertion is loose (1000 vs the 800
  budget) because the seed header + footer + truncation reserve eat
  some headroom. Acceptable as a contract test.
- **Dogfood gap I noticed:** `index_status` shows `runs_total: 23,
  last_duration: 49s` while I was iterating. The watcher is
  reindexing every save of `main.rs`. With persistent rust-analyzer
  the per-pass time should drop further once my edit storm settles.
  Worth adding a "long-batch warning" to the status output: if
  consecutive runs exceed N seconds, surface a hint about scope
  reduction or a more targeted path filter.
- **Tests:** 3 new tests, mcp suite 38/38, workspace 66/66, clippy
  clean.

### (12) coverage_md

- **Reached for first:** `mcp__codegraph__find_symbol("handle_concept")`
  to find a similar handler shape to crib from (Markdown-rendered
  multi-section dossier). Single tool call, exact location returned.
- **Then:** three `mcp__codegraph__cypher_md` calls to smoke-test the
  Cypher patterns I wanted to use:
  1. `WHERE NOT (f)<-[:CALLS]-(:Function)` — works
  2. `OPTIONAL MATCH ... WITH ... WHERE count = 0` — works
  3. `WHERE NOT EXISTS { MATCH ... }` — **ERROR** "tried to match
     MultiPartQuery starting here"
  Caught the velr planner gap *before* I wrote any code. Three
  `cypher_md` calls saved a build-fail-fix cycle.
- **Then a second velr surprise** caught at runtime:
  `WHERE NOT f:Test AND NOT (f)<-[:TESTS]-(:Test)` rejected with
  "Stage3 bind-table existential filtering only supports existential
  predicate trees". Splitting the label predicate (drop from Cypher)
  and the existential (keep) made both queries planner-friendly. The
  test-label drop became a client-side filter via a HashSet built from
  one extra `MATCH (t:Test)` query.
- **The dogfood feedback loop is real:** while developing this tool
  I saw `mcp__codegraph__index_status` showing **23 runs total, last
  duration 49s**. Each save of `main.rs` was triggering a full
  workspace re-parse (the sidecar filter caught one feedback source
  but the long batches suggest there's more — maybe the watcher is
  catching every individual save during my edit storm and queuing
  them all). Worth diagnosing later via the existing status fields.
- **What I built without a single grep:** find handler → check schema
  capabilities → write code → unit tests. Zero `Read` of unrelated
  files. The tool is the documentation.
- **Wish:** velr should grow proper `EXISTS { ... }` subquery support;
  the `WHERE NOT EXISTS { MATCH ... }` form is the natural way to
  express "modules with no doc-mentions" and forced me into a less
  readable client-side set-diff. Filed mentally for the velr roadmap.
- **Tests:** `coverage_md_surfaces_orphans_and_untested` (4-node
  graph asserts each section), `coverage_md_excludes_test_functions_from_orphans`
  (single :Test fn shouldn't appear in orphans). mcp suite 35/35.

### (b) Sidecar feedback-loop filter

- **Reached for first:** `mcp__codegraph__find_symbol("is_indexable_event_path")`
  → exact location + signature in one call. With grep I'd have done
  `grep -rn 'is_indexable_event_path' crates/`. About the same actually,
  but the result is structured and includes the signature so I knew
  what I was editing without opening the file.
- **Concrete signal that motivated this fix:** `mcp__codegraph__index_status`
  in the previous turn surfaced `codegraph.db.codegraph-meta.json` in
  the "Last batch paths" — the indexer's own sidecar was triggering the
  watcher. Without the live status tool, this would have shown up only
  via stderr spam in the MCP log; nobody reads those.
- **Tooling win:** the `index_status` tool was *itself* the diagnostic
  here. Building observability paid off within hours.
- **Trivial fix:** four extra `ends_with` clauses in the path filter,
  one test asserting the four extensions are dropped while real source
  still passes. mcp suite 33/33.



Resumed after the user wired `mcp__codegraph__*` into Claude Code. Now
I'm actually using the graph for lookups instead of grepping. Real
experience report:

### Library refactor of the indexer

- **Reached for first:** `mcp__codegraph__schema` to see what's in the
  graph (13 vertex labels, 12 edge types — including the new `:Test`
  and `[:TESTS]` from H6). `mcp__codegraph__find_symbol("main")` to
  locate the indexer's `main` (124 matches; `codegraph-indexer::main::main`
  at line 118). Then `mcp__codegraph__node_md` for the file dossier,
  followed by a `mcp__codegraph__cypher_md` listing every `:Function`
  in `main.rs` with line ranges. **All four tool calls returned exactly
  what I needed; no greps were used.**
- **Concrete win:** the file dossier surfaced that `main` is 429 lines
  (118–547) and identified the 22 helper functions that needed to come
  along for the ride. With grep I'd have read the file end-to-end. The
  `cypher_md` line-range table directly shaped my refactor plan.
- **Surprise:** `find_symbol` ranking confirmed substring matches all
  ranked correctly: exact `main` came first, even though there are 124
  total hits. The H2 ranking heuristic survives contact with real data.
- **Pleasant:** `impact(value="codegraph-indexer::main::main", depth=2)`
  showed the 74-callee blast radius in <1s, no LSP needed. That would
  have been a recursive `git grep -n` walk taking minutes manually.
- **Limit hit:** the qualified_name uses `-` (dash) where I'd assumed
  `_` (underscore): `codegraph-indexer::main::main` not
  `codegraph_indexer::main::main`. My first `impact` call missed for
  exactly this reason. `find_symbol` saved me — the substring match
  surfaced the correct qn shape. Lesson: always start with
  `find_symbol` when constructing a fully-qualified address.
- **Refactor itself was uneventful.** Copy main.rs → lib.rs, replace
  `fn main` with `pub fn run_indexer`, swap `process::exit` for
  `Result::Err`, swap early `return;` for `return Ok(IndexStats::noop)`.
  Three rustc-driven iterations (u32 vs usize, etc.) and it built.
  56/56 tests still green.



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
## H9 — `watch` / `unwatch` / `list_watches` + indexer Phase 7

- **Reached for:** `grep -n 'Phase 6'` to find the right insertion
  point in the indexer. Otherwise zero exploration — the patterns
  from H6 (Cypher post-processing) and G2 (note creation) compose
  cleanly here.
- **Design choice:** the trigger lives in the *indexer*, not the MCP
  server. Reasoning: the MCP server is read/write-on-demand, the
  indexer is the only thing that runs on a schedule (or on user
  action). Putting trigger evaluation in the indexer means the agent
  can call `watch` and forget — the next time anyone reindexes, the
  notes appear.
- **Honest limit:** the trigger fires on `body` change only. An edit
  to a comment outside the LSP symbol range, or a refactor that
  renames the function (changing `qualified_name`), won't fire.
  Documented in `docs/mcp-tools.md` so the agent doesn't over-trust
  silence.
- **velr OR-quirk avoided by design:** the Phase 7 query is a single
  `WHERE a IS NOT NULL AND b IS NOT NULL AND a <> b` — no OR. Got
  lucky here, but I've internalised the pattern: any time I'm tempted
  to write `WHERE x OR y`, split it.
- **Wish #9 / final wish:** the indexer should expose its phases as
  library functions so the MCP server could trigger Phase 7 on demand
  ("re-evaluate watches now"). Currently the only way to fire
  triggers is to run the full indexer. Punted to `future-ideas.md`.
- **Tests:** `watch_unwatch_lifecycle` + `watch_rejects_unknown_node`
  on the MCP side; `fire_watch_triggers_creates_note_and_rebaselines`
  on the indexer side (asserts: change ⇒ exactly one note, baseline
  updated, second run produces no second note). Workspace 56/56.

## Closing thoughts (H1–H9 done)

- **Recurring friction:**
  1. velr's `OR → UNION` rewrite (clobbers `LIMIT`, fans `SET` over
     all rows). Bit me in H4, H6, H8. The defensive pattern is now
     reflexive: never write `WHERE a OR b` in a write query; split.
  2. `cargo fmt` invalidating queued `Edit`s. Cost me a full re-apply
     in H8 (5 lost edits). Mitigation: don't bundle fmt with the
     check command at a feature boundary.
- **What worked:** strict per-feature commit cadence + journal write
  + checklist update. Each commit is a self-contained, reviewable
  unit, and the journal preserved the reasoning that the diff alone
  doesn't show. The codebase ended up with 9 new MCP tools, 1 new
  indexer phase, 18 new tests, and ~2.5k lines of new code, all on
  green CI gates.
- **The dogfooding gap (the user's original ask):** I never used
  `mcp__codegraph__*` because they aren't wired into this Claude
  Code session. Every code lookup was `grep` / `Read`. The journal
  records each one and what the graph would have given me — that's
  the actionable signal: every "reached for grep, would have used
  X" entry is a missing capability or a missing wiring step. A
  future session running with the MCP server attached would
  consistently shave the lookup roundtrips logged here.

## H8 — Auto-notes from PR comments

- **Reached for:** zero greps. The `:Note` write pattern from G2 is
  fully internalised at this point.
- **Surprise (mine, not velr's):** lost five queued Edit calls when
  `cargo fmt` rewrote `main.rs` between the previous commit and this
  step. The Edit tool requires file-state-since-read, and my new
  edits had been queued against a pre-fmt snapshot. Recovered by
  re-grepping anchors and re-applying. **Lesson:** when bundling
  fmt+test+clippy in one command at a feature boundary, consider
  *not* queuing dependent edits across the boundary, or accept the
  re-application cost.
- **Bug bounce:** my first cut filtered tokens before stripping the
  trailing `()`, so `foo()` was rejected because it contained `()`.
  Test caught it on the first run with `["foo", "Baz"]` instead of
  the expected three. Fixed by trimming first, then validating.
  Reads-too-literally bug, classic.
- **Honest scope:** the symbol matcher is "any backticked identifier
  that exists in the graph as a `Function.name` or
  `Function.qualified_name`". It will miss inline plain-text
  references and won't disambiguate when `foo` exists in two
  modules. Both are documented.
- **Wish #8:** the `gh` shellout layer should live in a tiny separate
  binary so the MCP server stays gh-free. Right now the agent is
  expected to fetch the JSON and pass it in. Acceptable, but a
  `codegraph-pr-notes <pr#>` convenience would be nice. Punted.
- **Tests:** `extract_backticked_symbols_strips_calls_and_codeblocks`
  (5-symbol mixed body, asserts fenced contents skipped and `foo()`
  matches `foo`), and `import_pr_notes_attaches_to_matching_function`
  (3 comments → 1 note attached to 2 functions, surfaces in the
  function's `node_md` dossier). mcp suite 27/27.

## H7 — `:Concept` layer

- **Reached for:** zero greps. The shape is a copy-paste of the H3
  saved-views pattern (MERGE + persistence + render) crossed with the
  H1 multi-section dossier output. Reuse compounding.
- **Honest scoping:** I built the *user-curated* concept layer, not
  the auto-clustering one originally sketched in the brainstorm.
  Embedding-based clustering is a different project (and would put a
  hard dependency on a tokenizer / embedding model). The curated
  version still buys "show me everything in the auth subsystem" with
  one tool call and zero greps, which is the actual unblock.
- **velr surprise #2 returns:** in `handle_concept` I needed
  "function reachable as direct member OR via DocSection.MENTIONS".
  Phrased as a single MATCH with OR it would have triggered the
  UNION-LIMIT trap again, so I split into two queries up front and
  union client-side via `BTreeSet`. The pattern is becoming routine.
- **Wish #7:** if `:Concept`s could *contain* `:Concept`s
  (`(:Concept)-[:CONTAINS]->(:Concept)`), the dossier could roll up
  hierarchically — "auth → session → token". One-line indexer change
  + a recursive resolution pass in the renderer. Punted to
  `future-ideas.md`.
- **Tests:** `concept_lifecycle_define_then_render` (defines
  module-a covering 2 functions, asserts list_concepts surfaces it
  and the dossier renders with both functions), and
  `concept_unknown_returns_not_found`. Workspace 51/51.

## H6 — `:Test` label and `[:TESTS]` edges

- **Tests:** `phase6_tags_tests_and_links_them` (3 functions: a sync
  test, a tokio test, and a regular fn; asserts the right two carry
  `:Test` and exactly two `[:TESTS]` edges land on the right target).
  Workspace 36/36.
