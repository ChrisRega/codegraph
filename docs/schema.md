# codegraph graph schema

The indexer projects every codebase into the same labelled-property graph
schema. This document is the canonical reference for what `codegraph-mcp`
and your own openCypher queries can expect to find in a populated database.

> Properties marked **(opt)** may be absent depending on language /
> source — query defensively (e.g. `coalesce(n.body, '')`).

## Top-level structure

```
:Workspace ─[:CONTAINS]─→ :Package ─[:CONTAINS]─→ :File
                          :Package ─[:DEPENDS_ON]─→ :Package
                          :Package ─[:EXPOSES]─→ :APIEndpoint | :APIType
:File      ←[:DEFINED_IN]─ :Function | :Symbol | :Field | :Parameter | :Import
:Function  ─[:CALLS]─→ :Function
:GitCommit ─[:SNAPSHOT_OF]─→ :Workspace      (only the current HEAD)
:GitCommit ─[:PARENT_OF]─→ :GitCommit         (full revision DAG)
:Author    ─[:AUTHORED]─→ :GitCommit
:Note      ─[:NOTES]─→ <any node>             (free-form Markdown notes)
:Doc       ─[:HAS_SECTION]─→ :DocSection
:DocSection ─[:MENTIONS]─→ :Function | :Symbol
:DocSection ─[:LINKS_TO]─→ :File | :Doc
:Feature   ─[:HAS_SCENARIO]─→ :Scenario ─[:HAS_STEP]─→ :Step
:Step      ─[:IMPLEMENTED_BY]─→ :Function
```

## Vertex labels

### `:Workspace`

The repository being indexed.

| property | type | notes |
| --- | --- | --- |
| `name` | string | derived from the workspace directory name |
| `root_path` | string | absolute filesystem path |

### `:Package`

A Cargo crate, npm package, or Python project.

| property | type | notes |
| --- | --- | --- |
| `name` | string | unique within `is_external = false`; external dependencies share names with the registry |
| `version` | string | (opt) — `"0.0.0"` if missing in the manifest |
| `path` | string | (opt) — workspace-relative path; absent for external deps |
| `language` | string | one of `Rust`, `TypeScript`, `Python` |
| `edition` | string | (opt) — Rust only |
| `is_external` | bool | `true` for transitive dependencies |

### `:File`

| property | type | notes |
| --- | --- | --- |
| `path` | string | workspace-relative POSIX path |
| `name` | string | basename |
| `extension` | string | (opt) |
| `lines` | int | line count when LSP indexed |
| `first_seen_commit` | string | (opt) — full SHA of the first indexed commit that contained this file |
| `last_seen_commit` | string | (opt) — full SHA of the most recent indexed commit that touched this file |

### `:Function`

| property | type | notes |
| --- | --- | --- |
| `qualified_name` | string | `pkg::module::name` |
| `name` | string | bare identifier |
| `kind` | string | `Free`, `Method`, `Test`, `Step` |
| `line_start` / `line_end` | int | 1-based |
| `body` | string | source slice, may be empty |
| `step_kind` | string | (opt) — when `kind = 'Step'`: `Given` \| `When` \| `Then` |
| `step_regex` | string | (opt) — when `kind = 'Step'` |
| `first_seen_commit` | string | (opt) — first indexed commit that defined this function |
| `last_seen_commit` | string | (opt) — most recent indexed commit |

### `:Symbol`

Catch-all for non-function declarations LSP returns: `Struct`, `Enum`,
`Interface`, `TypeParameter`, `Constant`, `Variable`. Property set
matches `:Function` minus the call-graph-specific fields.

### `:Test` (a label *added* to `:Function`)

After the LSP pass, the indexer tags every `:Function` whose body
contains `#[test]` or `#[tokio::test]` with an additional `:Test`
label, and materialises `(:Test)-[:TESTS]->(:Function)` for every
existing `[:CALLS]` edge from a test into a non-test. This is a
heuristic — it relies on rust-analyzer's `documentSymbol` ranges
including the attribute lines (which they currently do). Languages
without attribute-style test markers will need their own pass.

### `:Field`, `:Parameter`, `:Import`

Reserved labels — currently emitted only by API-spec scanners (`:Field`)
and not at all by `:Parameter` / `:Import`. Future LSP passes will fill
them in.

### `:GitCommit`, `:Author`

A real revision history. The first time a repo is indexed (or whenever
`--full` is passed) the indexer backfills up to `HISTORY_BACKFILL_LIMIT`
(currently 200) commits reachable from `HEAD`. Incremental runs walk
only the commits between the previously indexed `HEAD` and the new
`HEAD`. Both `:GitCommit` and `:Author` survive `--full` reindexes —
they are not part of the wipe set.

| property | type | notes |
| --- | --- | --- |
| `:GitCommit.hash` | string | full SHA — primary key |
| `:GitCommit.short_hash` | string | first 7 chars |
| `:GitCommit.message` | string | first line of the commit |
| `:GitCommit.timestamp` | string | author-date ISO-8601 |
| `:Author.email`, `:Author.name` | string | as recorded by git |

Edges:

- `(:Author)-[:AUTHORED]->(:GitCommit)`
- `(:GitCommit)-[:PARENT_OF]->(:GitCommit)` — the full DAG, including
  merges (a merge commit appears as the target of multiple `PARENT_OF`
  edges).
- `(:GitCommit)-[:SNAPSHOT_OF]->(:Workspace)` — only the current `HEAD`.

### `:Note`

Free-form Markdown notes written by humans or LLM agents via the
`write_note` MCP tool. Survives `--full` reindex.

| property | type | notes |
| --- | --- | --- |
| `id` | string | timestamp-derived primary key |
| `title` | string | (opt) |
| `author` | string | defaults to `claude` from the MCP tool |
| `tags` | string | comma-separated, (opt) |
| `created_at` | string | ISO-8601 |
| `markdown` | string | the body |

Edges: `(:Note)-[:NOTES]->(<any target>)`.

### `:Doc`, `:DocSection`

Each Markdown file becomes one `:Doc`; each heading becomes one
`:DocSection`.

| property | type | notes |
| --- | --- | --- |
| `:Doc.qualified_name` | string | identical to `:Doc.path` |
| `:Doc.title` | string | first H1 or filename |
| `:Doc.path` | string | workspace-relative |
| `:Doc.line_count` | int | |
| `:DocSection.qualified_name` | string | `<doc-path>#<heading-slug>` |
| `:DocSection.heading` | string | raw heading text |
| `:DocSection.level` | int | 1–6 |
| `:DocSection.line` | int | 1-based |

### `:Feature`, `:Scenario`, `:Step`

Gherkin (cucumber) feature files.

| property | type | notes |
| --- | --- | --- |
| `:Feature.qualified_name` | string | `"<file>::<feature name>"` |
| `:Feature.name` / `.file_path` / `.line` / `.tags` | mixed | tags is a comma-joined string |
| `:Scenario.qualified_name` | string | `"<feature-qn>::<name>@<line>"` |
| `:Scenario.name` / `.line` / `.tags` | mixed | |
| `:Step.qualified_name` | string | `"<scenario-qn>#<order>"` |
| `:Step.kind` | string | `Given` \| `When` \| `Then` (etc.) |
| `:Step.text` | string | the step body |
| `:Step.step_order` / `.line` | int | |

### `:APIEndpoint`, `:APIType`

OpenAPI operations / GraphQL SDL types / Protobuf RPCs and messages.

`:APIEndpoint` carries `method`, `path`, `operationId`, `summary`,
`tags`, `spec_file`. `:APIType` carries `name`, `kind`, `spec_file`.
`:Field` (children of `:APIType`) carry `name`, `type_name`, `kind`,
`index`.

## Edge types

| edge | from → to | notes |
| --- | --- | --- |
| `CONTAINS` | Workspace → Package, Package → File | hierarchy |
| `DEPENDS_ON` | Package → Package | property `kind`: `Normal`, `Dev`, `Build` |
| `DEFINED_IN` | Function/Symbol/etc. → File | reverse direction matters: `(:File)<-[:DEFINED_IN]-(n)` |
| `CALLS` | Function → Function | rebuilt every run from LSP `outgoingCalls` |
| `SNAPSHOT_OF` | GitCommit → Workspace | only on the current HEAD |
| `PARENT_OF` | GitCommit → GitCommit | full revision DAG, including merges |
| `AUTHORED` | Author → GitCommit | |
| `NOTES` | Note → any | written via the `write_note` MCP tool |
| `HAS_SECTION` | Doc → DocSection | |
| `MENTIONS` | DocSection → Function/Symbol | resolved code-spans |
| `LINKS_TO` | DocSection → File | resolved `[text](path)` links |
| `HAS_SCENARIO`, `HAS_STEP` | Feature → Scenario, Scenario → Step | |
| `IMPLEMENTED_BY` | Step → Function | regex match of `Step.text` against `Function.step_regex` |
| `TESTS` | Test → Function | derived from `[:CALLS]` where the source carries `:Test` and the target does not |
| `EXPOSES` | Package → APIEndpoint or APIType | API specs |
| `USES_SCHEMA` | APIEndpoint → APIType | OpenAPI `$ref`s |
| `HAS_FIELD` | APIType → Field | |
| `HAS_IMPORT` | File → Import | (reserved; not yet emitted) |

## Example queries

```cypher
// find all functions that call a function named `format_table`
MATCH (caller:Function)-[:CALLS]->(callee:Function {name: 'format_table'})
RETURN caller.qualified_name

// every BDD scenario that has at least one un-implemented step
MATCH (sc:Scenario)-[:HAS_STEP]->(st:Step)
WHERE NOT EXISTS { MATCH (st)-[:IMPLEMENTED_BY]->(:Function) }
RETURN sc.qualified_name, count(st) AS missing
ORDER BY missing DESC

// Markdown sections that mention any function defined in `src/main.rs`
MATCH (s:DocSection)-[:MENTIONS]->(fn:Function)-[:DEFINED_IN]->(f:File {path: 'src/main.rs'})
RETURN s.qualified_name, fn.qualified_name
```
