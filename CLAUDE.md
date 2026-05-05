# Working with this repo as Claude Code

This repo *builds* the `codegraph` MCP server, but you can also *use* it on
this same codebase. If the user has `codegraph-mcp` wired into Claude Code
(see `docs/mcp-tools.md`) and an indexed `./codegraph.db`, prefer it over
ad-hoc filesystem search.

A reusable skill file lives at `examples/claude-skill/codegraph.md` — copy
it to `~/.claude/skills/codegraph.md` (or to `.claude/skills/codegraph.md`
in any project that uses codegraph) to apply the rules below by default.

## Rules of engagement (when codegraph MCP is available)

- **Navigate via the graph.** Use `mcp__codegraph__cypher_md`,
  `mcp__codegraph__node_md`, and `mcp__codegraph__schema` for "where is X
  defined?", "who calls Y?", "what does Z file expose?". Reach for `grep` /
  `find` / `Read` only when the graph cannot answer or when you need actual
  source.
- **Markdown by default.** Prefer `cypher_md` over `cypher`. Drop the result
  straight into your reply.
- **Persist findings.** Use `mcp__codegraph__write_note` to attach a
  `:Note` to the node your finding is about. This is the long-term memory
  for downstream sessions; future `node_md` calls surface it automatically.
- **Recall first.** Before a deep investigation, run `list_notes` filtered
  to the relevant subgraph.
- **Re-index when stale.** If the graph disagrees with the working tree,
  the index is old. Tell the user; suggest
  `cargo run --release -p codegraph-indexer -- --workspace . --db ./codegraph.db`.

## Project conventions for this codebase

- velr 0.2.x has no `$param` placeholders. All Cypher is built by string
  composition with `codegraph_core::escape_str` / `escape_ident`. Keep that
  pattern when adding queries.
- The full reindex must wipe owned labels (see
  `crates/codegraph-indexer/src/main.rs` Phase 0). When you add a new label,
  add it to that wipe list **unless** it's revision history (`:GitCommit`,
  `:Author`, `:Note` — those accumulate across runs).
- Tests favour `Db::open_in_memory()`; mirror that in new tests.
- Public-facing changes get a `CHANGELOG.md` entry under `## [Unreleased]`.
