# NEXT — open features & refactorings

Brainstormed 2026-05-25. Tracked in the graph as `:WorklogItem`s
with `area`s `indexer`/`mcp`/`docs`/`schema` and matching `kind`.
Query the live state with `worklog_list`. This file is the human
overview; the DB is the source of truth.

Suggested order: **1 → 2 → 3**, then pick from the rest.

## Bug-hunting (DB eskaliert sporadisch auf 20 GB)

- **nx-01 — Transaction-Leak-Detector** *(task, area: mcp)*
  Instrument `begin`/`commit`/`rollback` mit IDs + Timer. Offene
  Transactions >30 s loggen. Hauptverdächtiger für den WAL-Bloat.
- **nx-02 — WAL/DB-Size-Telemetry in `index_status`** *(feature, area: mcp)*
  `db_size`, `wal_size`, `oldest_open_tx`-Sekunden im Status-Output.
  Sofort-Trigger für den 20-GB-Bug.

## Refactoring

- **nx-03 — `crates/codegraph-indexer/src/lib.rs` splitten** *(refactor, area: indexer)*
  2843 LoC. `run_indexer_inner`, Wipe-Logik, Per-Sprache-Collectors
  in sibling modules — analog zur mcp-Crate-Struktur.
- **nx-04 — `main.rs` Tests rausziehen** *(refactor, area: mcp)*
  1738 LoC, nah am 2000-Cap aus CLAUDE.md. `seed_db` / `text_of`
  in `tests/common.rs`, Handler-Tests in eigene Module.

## Fehlende Tools

- **nx-05 — `graph_export`** *(feature, area: mcp)*
  DOT/Mermaid für Subgraph(node, depth). Visualisierung statt
  serielles `node_md`.
- **nx-06 — `dead_code`** *(feature, area: mcp)*
  Funktionen ohne eingehende `:CALLS`. Filter: `pub`/tests/trait
  impls excludable.
- **nx-07 — `coverage_md --by-package`** *(feature, area: mcp)*
  Rollup pro `:Package`. Findet Test-Wüsten auf einen Blick.
- **nx-08 — `worklog_md` Anreicherung** *(feature, area: mcp)*
  Comment-Count + `last_activity` pro Item in der Liste.
- **nx-09 — Auto-Link `:GitCommit` → `:WorklogItem`** *(feature, area: indexer)*
  Conventional-Commit-Trailer (`Refs: nx-09`) parsen, beim
  History-Phase Kante setzen.
- **nx-10 — MCP Tool-Error-Normalisierung** *(refactor, area: mcp)*
  Handler werfen mal Strings, mal JSON-RPC-Errors. Einmal
  durchziehen.

## Schema-Lücken

- **nx-11 — `:PR` Nodes** *(feature, area: schema)*
  Analog `:Release`, Edges zu `:GitCommit` und `:WorklogItem`.
  Macht PR-Historie graph-abfragbar.
- **nx-12 — `:Concept` ↔ `:Function` direkte Kante** *(feature, area: schema)*
  Statt nur über `:Note`. Bessere Cypher-Abfragen für
  konzept-getriebene Navigation.

## Performance

- **nx-13 — Markdown + LSP parallel indexieren** *(perf, area: indexer)*
  Beide Phasen sind unabhängig, laufen aber sequentiell.
- **nx-14 — Watcher-Batching-Telemetrie** *(feature, area: mcp)*
  Letzte-Stunde-Frequenz, mittlere Batch-Größe + Dauer in
  `index_status`. Auch Diagnostik-Asset für nx-01/nx-02.
