# Roadmap

_Generated from the graph by `codegraph-mcp report`. Do not edit by hand._

## indexer

### done

- [x] **K9 fix — [:TESTS] edge duplication via velr MERGE bug** `(done)` — _since 2026-05-16T11:07:14Z_  
  `wl-k9-tests-dedup`
- [x] **K10 fix — :DocSection / :Doc accumulation across reindexes** `(done)` — _since 2026-05-16T11:07:14Z_  
  `wl-k10-doc-wipe`
- [x] **Phase 8 — :GitCommit:WorkingTree overlay for uncommitted edits** `(done)` — _since 2026-05-16T10:49:33Z_  
  `wl-phase8-overlay`
- [x] **Pipeline outgoingCalls in chunks of 32 (parallel LSP)** `(done)` — _since 2026-05-16T10:41:04Z_  
  `wl-pipeline-outgoing-calls`
- [x] **Skip outgoingCalls LSP requests for unchanged-body fns** `(done)` — _since 2026-05-16T10:27:40Z_  
  `wl-body-hash-skip`
- [x] **Skip unchanged files via FNV-1a content hash** `(done)` — _since 2026-05-16T10:15:56Z_  
  `wl-content-hash-skip`
- [x] **K8 — preserve [:NOTES] across reindex via snapshot/restore** `(done)` — _since 2026-05-16T10:03:30Z_  
  `wl-k8-notes-survive`
- [x] **G3 — real revision history (:GitCommit + :Author + :PARENT_OF)** `(done)` — _since 2026-05-13T18:00:00Z_  
  `wl-g3-git-history`

## mcp

### in_progress

- [ ] **Graph-backed worklog (items / statuses / comments) + report subcommand** `(in_progress)` — _since 2026-05-16T12:00:00Z_  
  `wl-graph-worklog`

### done

- [x] **Watcher visibility — surface pending events during a pass** `(done)` — _since 2026-05-16T10:06:57Z_  
  `wl-watcher-visibility`
- [x] **I2 — codegraph-mcp --watch mode (live reindex on save)** `(done)` — _since 2026-05-15T18:00:00Z_  
  `wl-i2-watch-mode`
- [x] **H4 — diff_since(commit) MCP tool** `(done)` — _since 2026-05-14T20:00:00Z_  
  `wl-h4-diff-since`
- [x] **H1 — impact MCP tool (transitive blast radius)** `(done)` — _since 2026-05-14T18:00:00Z_  
  `wl-h1-impact`

## refactor

### pending

- [ ] **IndexCtx / tools::Ctx structs** `(pending)` — _since 2026-05-15T20:00:00Z_  
  `wl-k7-ctx-structs`

### done

- [x] **K6 — per-tool MCP handler split into sibling modules** `(done)` — _since 2026-05-15T23:08:27Z_  
  `wl-k6-handler-split`

## release

### pending

- [ ] **Replace placeholder repository URL in Cargo.toml** `(pending)` — _since 2026-05-10T00:00:00Z_  
  `wl-c2-repo-url`
- [ ] **Verify rust-version = 1.75 locally** `(pending)` — _since 2026-05-10T00:00:00Z_  
  `wl-c5-msrv-verify`
- [ ] **Pre-built release binaries via cargo-dist** `(pending)` — _since 2026-05-10T00:00:00Z_  
  `wl-f2-cargo-dist`

## viz

### pending

- [ ] **bdd-viz rendering from velr (skip JSON intermediate)** `(pending)` — _since 2026-05-10T00:00:00Z_  
  `wl-f3-bdd-viz-velr`

