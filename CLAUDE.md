# Working with this repo as Claude Code

This repo *builds* the `codegraph` MCP server, but you can also *use* it
on this same codebase — `codegraph-mcp` is wired into Claude Code and
should be the first thing you reach for whenever you need to navigate,
reason about, or document the codebase.

## Skill file

The full operating playbook lives in
[`examples/claude-skill/codegraph.md`](examples/claude-skill/codegraph.md).
It is the source of truth for *how* to use the MCP tools, including the
complete tool surface, the velr planner gotchas, and quick recipes.
Copy it to `~/.claude/skills/codegraph.md` (user-wide) or
`.claude/skills/codegraph.md` (per project) so it applies by default.

When operating in this repo, treat the skill file as authoritative —
the points below are repo-specific *additions*, not a replacement.

## Repo-specific conventions

### Cypher

- **No `$param` placeholders.** velr 0.2.x has none. All Cypher is
  built by string composition with `codegraph_core::escape_str(value)`
  / `escape_ident(name)`. Never `format!`-splice raw input directly.
- **No `EXISTS { MATCH ... }` subqueries** — velr planner doesn't
  support them. Use `WHERE NOT (pattern)` or client-side set-diff.
- **No `WHERE a OR b` combined with `LIMIT` or `SET`** — `OR`
  rewrites to `UNION` and clashes. Split into separate statements.
- **No mixed `WHERE label_pred AND existential` clauses.** Drop the
  label predicate to client-side filtering. (See `coverage_md` and
  `impact` for the pattern.)
- **Chunk `IN [...]` lists past ~100 elements when combined with a
  write clause.** Past commit `f929f42` documents the OOM that
  motivates this; `lsp_index::index_files_via_lsp` is the example.
- All of the above are documented in detail in the skill file; this
  list is the cheat-sheet.

### Indexer + watcher

- The full reindex (`--full`) wipes only source-derived labels:
  `:File`, `:Workspace`, `:Package`, `:APIEndpoint`, `:APIType`, plus
  per-pass code-node cleanup of `:Symbol` / `:Function` / `:Field` /
  `:Parameter` / `:Import`. **Revision history (`:GitCommit`,
  `:Author`) and user-derived nodes (`:Note`, `:View`, `:Concept`,
  `:Watch`) accumulate across runs.**
- When you add a new node label, decide which category it lives in
  and update `run_indexer_inner` accordingly. Don't add user-state
  labels to the wipe set.
- Live mode (`IndexOptions::with_paths(paths)`, set by the MCP
  watcher) reparses only the given paths, skips `phase_history`, and
  does **not** advance the sidecar metadata. Uncommitted edits show
  up as a draft overlay; persistent revision history advances only on
  actual `git commit`.
- The persistent `LspPool` is owned by the MCP watcher thread; each
  language server pays its cold-start cost once per server lifetime
  and is reused across batches. `wait_until_idle` (commit `3c9434d`)
  replaces the previous fixed 15 s sleep with detection of LSP
  message silence.

### Code conventions

- Tests favour `Db::open_in_memory()`; mirror that in new tests.
- Public-facing changes get a `CHANGELOG.md` entry under
  `## [Unreleased]`.
- The mcp crate's per-tool handlers live in `crates/codegraph-mcp/src/`
  as sibling modules (`concepts.rs`, `coverage.rs`, `diff.rs`,
  `explore.rs`, `find.rs`, `history.rs`, `impact.rs`, `notes.rs`,
  `pr_notes.rs`, `report.rs`, `views.rs`, `watch_tools.rs`,
  `worklog.rs`). When adding a new tool, create a new sibling module
  rather than growing `main.rs`.
- `main.rs` should stay <2000 LoC; if it grows past that, extract
  another module.
- Tests for handlers currently live in `main.rs::mod tests` for
  shared `seed_db` / `text_of` helpers. Acceptable to leave them
  there until those helpers themselves earn an extraction.

### Watcher etiquette

- The watcher debounces at 500 ms by default and processes batches
  sequentially. When you save many files in quick succession, all
  events coalesce into one pass.
- If `index_status` shows `state: running` for unusually long (>30 s
  on a small workspace), check `/proc/<pid>/status` for `VmRSS`. A
  multi-GB RSS means the indexer is in a planner explosion — see
  velr gotcha #5 in the skill file.
- The DB file (`codegraph.db`) and its sidecars are gitignored. Don't
  commit them.
