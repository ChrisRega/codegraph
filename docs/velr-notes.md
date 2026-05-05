# velr-specific notes

This document captures behaviour we observed against `velr 0.2.9` while
porting codegraph from cypherlite, plus open questions to revisit when
velr matures. Anything verified here lives under `crates/codegraph-core/tests/`.

## What we depend on

| Feature | Verified in | Behaviour we rely on |
| --- | --- | --- |
| `Velr::open(path)` | `velr_roundtrip.rs` | creates the file if missing |
| `Velr::open(None)` | `velr_roundtrip.rs` | in-memory database for tests |
| `Velr::run(cypher)` | `velr_roundtrip.rs` | single-statement writes |
| `Velr::exec_one(cypher)` | `velr_roundtrip.rs` | single-table queries |
| `Velr::exec(cypher)` | `explain_probe.rs` | multi-table queries (`EXPLAIN`, `;`-separated) |
| `Velr::begin_tx()` | (used by MCP `commit`) | transactional buffered replay; rollback on error |
| `labels(n)` | `cypher_intro.rs` | label list per node, used by MCP `schema` |
| `type(r)` | `cypher_intro.rs` | edge type, used by MCP `schema` |
| `DISTINCT` + `count()` | `cypher_intro.rs`, `merge_semantics.rs` | aggregations |
| `MERGE` | `merge_semantics.rs` | idempotent match-or-create |
| `EXPLAIN <query>` keyword | `explain_probe.rs` | accepted; returns multiple tables (read with `exec()`, not `exec_one()`) |

## Known gaps in 0.2.x

- **No `$param` binding** — every value has to be inlined into the query
  string. We escape via `codegraph_core::escape_str` /
  `codegraph_core::escape`.
- **`ExplainTrace` has no `Display`/`Debug`** — `Velr::explain` returns a
  trace that we can't pretty-print. `codegraph-mcp::handle_explain`
  works around this by prefixing the user's query with the `EXPLAIN`
  keyword and reading the resulting tables instead.
- **`exec_one` errors on multi-table queries** — `EXPLAIN ...` produces
  multiple tables. `Db::query_many()` exists for this; prefer it for
  any DDL or planner-introspection queries.

## Concurrent access

velr connections are connection-affine (`Send + !Sync`). Two processes
can hold separate `Velr` handles to the same file, but coordination
between them is not part of the velr 0.2.x contract.

`codegraph-mcp` mitigates this defensively: before every `tools/call`
dispatch it `stat`s the database (and the SQLite `-wal` / `-shm`
sidecars), and if the latest mtime is newer than the moment we opened
with — and no transaction is buffered — it transparently reopens the
handle. That covers the common case where `codegraph-indexer` runs in
the background while a Claude session keeps the MCP server alive.

What is **not** covered:

- An indexer write that completes in the middle of an MCP transaction
  (we deliberately skip the reopen so we don't lose buffered queries).
- Two writers competing simultaneously — velr will return whatever
  error SQLite raises (`SQLITE_BUSY` style); our code surfaces it
  unmodified.

## Things we have not verified

- Behaviour when the velr DB file is replaced underneath us (atomic
  rename) — Sled used a directory, velr is a single file plus WAL
  sidecars, so the failure mode is different.
- Performance under bulk inserts. The indexer issues thousands of small
  `Velr::run` statements — batching via `begin_tx()` may help and is
  worth a future benchmark.
- Long-running MCP sessions across velr alpha → beta upgrades. Pin a
  velr version in `Cargo.lock` and retest before adopting any new
  release.
