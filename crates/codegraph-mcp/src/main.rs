// `tool_list()` builds the entire tools array as one big `serde_json::json!`
// macro tree; each tool adds a few macro-recursion levels. We're past the
// default 128 — bump it so the build stays green when adding new tools.
#![recursion_limit = "256"]

//! codegraph MCP server — exposes a velr-backed graph database as Claude tools.
//!
//! Speaks the Model Context Protocol (JSON-RPC 2.0 over stdio).
//!
//! Tools:
//!   • `schema()`           — list vertex labels and edge types (sampled)
//!   • `cypher(query)`      — run an openCypher read query (TSV output)
//!   • `cypher_md(query)`   — run a Cypher query and render rows as a
//!                            GitHub-flavoured Markdown table
//!   • `node_md(...)`       — render a compact Markdown dossier for a node
//!                            (properties + incoming/outgoing neighbours)
//!   • `write_note(...)`    — attach a Markdown `:Note` to a node selected
//!                            by a Cypher MATCH
//!   • `list_notes(...)`    — list notes (optionally filtered by a target
//!                            MATCH); rendered as Markdown
//!   • `history(limit?)`    — list `:GitCommit` snapshots in the graph
//!   • `begin(message?)`    — open a buffered transaction
//!   • `write(query)`       — inside tx: buffer; outside: apply immediately
//!   • `commit()`           — replay all buffered queries inside one velr tx
//!   • `rollback()`         — discard buffered queries
//!   • `explain(query)`     — `EXPLAIN` plan for a query
//!
//! Usage:
//!   codegraph-mcp --db /path/to/codegraph.db

use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};

use codegraph_core::{escape_str, Cell, Db};
use serde::Deserialize;
use serde_json::{json, Value};

mod arch_overlay;
mod concepts;
mod coverage;
mod dead_code;
mod diff;
mod explore;
mod find;
mod graph_export;
mod history;
mod impact;
mod notes;
mod pr_notes;
mod render;
mod report;
mod tx;
mod util;
mod views;
mod watch;
mod watch_tools;
mod worklog;
use arch_overlay::handle_arch_overlay;
use concepts::{handle_concept, handle_define_concept, handle_list_concepts};
use coverage::handle_coverage_md;
use dead_code::handle_dead_code;
use diff::handle_diff_since;
use explore::handle_explore;
use find::handle_find_symbol;
use graph_export::handle_graph_export;
use history::handle_history;
use impact::handle_impact;
use notes::{handle_list_notes, handle_write_note};
use pr_notes::handle_import_pr_notes;
use render::{format_table, format_table_md, render_neighbours, render_notes_rows};
use tx::{handle_begin, handle_commit, handle_rollback, handle_write, TxState};
use util::{err_text, ok_text, parse_node_address};
use views::{handle_list_views, handle_save_view, handle_view};
use watch::{handle_index_status, new_shared_status, spawn_indexer_watcher};
use watch_tools::{handle_list_watches, handle_unwatch, handle_watch};
use worklog::{
    handle_worklog_comment, handle_worklog_create, handle_worklog_list, handle_worklog_md,
    handle_worklog_set_status,
};

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: &Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

// Moved to `util` module — see refactoring 1a.

// ── Transaction state ─────────────────────────────────────────────────────────

// `TxState` moved to `tx` module — see refactoring 1a.

// ── Tool definitions ──────────────────────────────────────────────────────────

fn tool_list() -> Value {
    json!({
        "tools": [
            {
                "name": "schema",
                "description": "Return all vertex labels and edge types observed in the database (sampled). Call this first to understand what data is available before writing Cypher queries.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cypher",
                "description": "Run an openCypher query and return its rows as a text table. Both reads and writes are accepted; for buffered transactional writes, use begin/write/commit instead.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "openCypher query" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "begin",
                "description": "Open a buffered transaction. Subsequent `write` calls are accumulated. `commit` applies them all inside one velr transaction; `rollback` discards them.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message": { "type": "string", "description": "Free-form label for the transaction (kept in memory only)" }
                    }
                }
            },
            {
                "name": "write",
                "description": "Execute or buffer a Cypher write statement (CREATE, MERGE, SET, DELETE, REMOVE, DETACH DELETE). Inside a transaction the query is buffered; outside it is applied immediately.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Cypher write statement" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "commit",
                "description": "Replay every buffered write inside a single velr transaction and commit. Fails if no transaction is open.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "rollback",
                "description": "Discard all buffered writes and close the current transaction without applying anything.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "explain",
                "description": "Return velr's planner explanation for a query (no execution).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "openCypher query to explain" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "cypher_md",
                "description": "Run an openCypher read query and render the rows as a GitHub-flavoured Markdown table. Prefer this over `cypher` whenever you want to drop the result straight into a doc, note, or chat reply.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "openCypher query" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "node_md",
                "description": "Return a compact Markdown dossier for a single node: its properties plus incoming and outgoing neighbours grouped by edge type. Identify the node with `label` + `key` + `value` (e.g. label='Function', key='qualified_name', value='codegraph_indexer::main::run').",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string", "description": "Node label, e.g. 'Function', 'File', 'Package'." },
                        "key":   { "type": "string", "description": "Property name used to identify the node, e.g. 'qualified_name' or 'path'." },
                        "value": { "type": "string", "description": "Property value to match." },
                        "neighbours_limit": { "type": "integer", "description": "Max neighbours per edge type (default 25)." }
                    },
                    "required": ["label", "key", "value"]
                }
            },
            {
                "name": "write_note",
                "description": "Attach a Markdown `:Note` node to one or more existing nodes selected by a Cypher MATCH. Use this to persist research findings, design decisions, TODOs and other long-lived context in the graph itself. The MATCH must bind a variable named `t` (target).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "match":    { "type": "string", "description": "Cypher MATCH clause that binds a variable `t`. Example: \"MATCH (t:Function {qualified_name: 'foo::bar'})\"." },
                        "markdown": { "type": "string", "description": "Markdown body of the note." },
                        "title":    { "type": "string", "description": "Optional short title (1 line)." },
                        "author":   { "type": "string", "description": "Optional author tag, e.g. 'claude' or a username." },
                        "tags":     { "type": "string", "description": "Optional comma-separated tags." }
                    },
                    "required": ["match", "markdown"]
                }
            },
            {
                "name": "list_notes",
                "description": "List `:Note` nodes as a Markdown document. Optionally filter by a Cypher MATCH that binds `t`; only notes attached to a matched target are returned.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "match": { "type": "string", "description": "Optional Cypher MATCH binding `t`. Omit to list every note." },
                        "limit": { "type": "integer", "description": "Max notes to return (default 50)." }
                    }
                }
            },
            {
                "name": "history",
                "description": "List `:GitCommit` snapshots recorded in the graph, newest first. Useful to see how far back the graph's revision history goes and which commits left a footprint.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "description": "Max commits (default 50)." }
                    }
                }
            },
            {
                "name": "watch",
                "description": "Mark a node as watched. The next indexer run compares the current `body` against the baseline captured here; if anything changed, a `:Note` tagged `watch-trigger` is created and attached to the node. Use this to be notified across sessions when a function you care about gets modified by someone else.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string", "description": "Node label." },
                        "key":   { "type": "string", "description": "Identifying property name." },
                        "value": { "type": "string", "description": "Property value." }
                    },
                    "required": ["label", "key", "value"]
                }
            },
            {
                "name": "unwatch",
                "description": "Remove the `:Watch` label and baseline tracking properties from a node.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "key":   { "type": "string" },
                        "value": { "type": "string" }
                    },
                    "required": ["label", "key", "value"]
                }
            },
            {
                "name": "list_watches",
                "description": "List every node currently carrying the `:Watch` label, with the commit at which the baseline was captured.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "import_pr_notes",
                "description": "Import a list of PR / code-review comments as `:Note` nodes attached to any `:Function` they reference. Backticked tokens in each `body` are looked up against `Function.name` and `Function.qualified_name`; matching targets all get the same note attached. Suggested workflow: feed the output of `gh pr view <n> --json comments` into the `comments` argument.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pr":       { "type": "string", "description": "PR identifier, used in the note title and id." },
                        "comments": {
                            "type": "array",
                            "description": "Array of `{author, body, url}` objects (extra fields ignored).",
                            "items": { "type": "object" }
                        }
                    },
                    "required": ["comments"]
                }
            },
            {
                "name": "define_concept",
                "description": "Create or update a `:Concept` node and attach `[:DESCRIBES]` edges to every node bound by the supplied MATCH clause (which must bind a variable `t`). Concepts are user-curated subsystem labels — once defined, `concept(name)` returns a full dossier (members + mentioned functions + tests + notes).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name":        { "type": "string", "description": "Concept name (identifier-like, '_' / '-' allowed)." },
                        "match":       { "type": "string", "description": "Cypher MATCH clause binding a variable `t`. Example: \"MATCH (t:DocSection) WHERE t.qualified_name STARTS WITH 'docs/auth'\"." },
                        "description": { "type": "string", "description": "Optional one-line description." }
                    },
                    "required": ["name", "match"]
                }
            },
            {
                "name": "concept",
                "description": "Return a Markdown dossier for a `:Concept`: its description, direct members (whatever the DESCRIBES edges point at), the `:Function`s those members mention or are, plus any `:Test`s and `:Note`s that touch those functions. The subsystem-level companion to `node_md`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Concept name." }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "list_concepts",
                "description": "List every `:Concept` as a Markdown table (name, description, member count, created_at).",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "coverage_md",
                "description": "Surface the dim spots of the graph as a single Markdown report — useful for onboarding (\"where to start reading\") and refactor risk (\"what's load-bearing but undocumented\"). Sections: functions with no inbound `[:CALLS]` (orphans), non-test functions with no inbound `[:TESTS]`, files with no `:Note`s, and packages whose files have zero doc-mentions. Each row in the untested-functions section is ranked by total `[:CALLS]` fan-in so the highest-impact gaps surface first.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "description": "Max rows per category (default 15)." }
                    }
                }
            },
            {
                "name": "explore",
                "description": "Token-budgeted graph exploration. Starts at the identified node and walks outward (BFS up to `max_depth`), then greedily fills a Markdown report with the most informative neighbours until `char_budget` is exhausted. \"Informative\" = high degree + has notes + has doc mentions. Replaces the agent pattern of issuing 5–10 `node_md` calls to map a subgraph: one bounded call returns the best slice. Output ends with a footer telling you how many candidates were dropped so you know whether to raise the budget or pivot.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label":       { "type": "string", "description": "Seed node label, e.g. 'Function'." },
                        "key":         { "type": "string", "description": "Seed node identifying property, e.g. 'qualified_name'." },
                        "value":       { "type": "string", "description": "Seed node property value." },
                        "char_budget": { "type": "integer", "description": "Approximate output budget in characters (~ tokens × 4). Default 8000." },
                        "max_depth":   { "type": "integer", "description": "BFS depth cap (default 2, max 4)." }
                    },
                    "required": ["label", "key", "value"]
                }
            },
            {
                "name": "arch_overlay",
                "description": "Synchronously invoke the agent-driven architecture overlay (claude CLI subprocess) on the live DB. In-session counterpart to `codegraph-indexer --full --with-arch-agent`. Wipes the previous `:ArchModule` overlay, gathers context from `:Package` + `:Function` + `:CALLS`, asks the agent for a 3–7-module coarse architecture, and writes back `:ArchModule` + `[:CONTAINS]`→`:Package` + `[:GROUPS]`→`:Function` + `[:USES]` edges. Workspace name derives from `--watch` path; pass `workspace_name` explicitly if the server runs without `--watch`. Real cost: one `claude -p` call + a few seconds. Failures degrade silently (no overlay rather than partial) — check stderr.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "workspace_name": { "type": "string", "description": "Override the derived workspace name (defaults to basename of `--watch` path)." }
                    }
                }
            },
            {
                "name": "dead_code",
                "description": "List `:Function` nodes with no incoming `:CALLS` edges — the 'graph-derived suspicious functions' report. Hint generator, **not** a verdict: `main`, public API, FFI, dynamic dispatch (string-matched handlers!) and trait impls look dead to the graph because the caller side isn't in the AST. Defaults exclude `:Test`-labeled candidates and count test-only callers as evidence of life (flip `ignore_test_callers=true` for a 'covered-only-by-tests' sweep). Output is grouped by file:line so it's jumpable.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "exclude_tests":       { "type": "boolean", "description": "Skip candidates labeled `:Test` (default true)." },
                        "ignore_test_callers": { "type": "boolean", "description": "When true, callers labeled `:Test` don't count as life (default false)." },
                        "kind":                { "type": "string",  "description": "Restrict to a `f.kind` value, e.g. `Free` or `Method`." },
                        "name_skip":           { "type": "string",  "description": "Substring (case-insensitive) — candidate names containing it are hidden (entry-point heuristic, e.g. `main`, `handle_`)." },
                        "limit":               { "type": "integer", "description": "Max rows in the output (default 100, clamped 1–1000)." }
                    }
                }
            },
            {
                "name": "graph_export",
                "description": "Render a node-centered subgraph as a Mermaid `flowchart LR` (default) or Graphviz DOT diagram. BFS from the seed up to `depth` (clamped 1–3), capped at `max_nodes` (5–200) so a hub doesn't blow the context window. Output is fenced (```mermaid / ```dot) so it round-trips into GitHub / chats / notes. Identity for neighbour nodes uses coalesce(qualified_name, id, path, name, hash). Heterogeneous deeper BFS is a follow-up — at depth > 1, only nodes with the seed's label/key continue.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label":     { "type": "string", "description": "Seed node label, e.g. 'Function' or 'WorklogItem'." },
                        "key":       { "type": "string", "description": "Seed node identifying property, e.g. 'qualified_name' or 'id'." },
                        "value":     { "type": "string", "description": "Seed node property value." },
                        "depth":     { "type": "integer", "description": "BFS depth (default 1, clamped 1-3)." },
                        "max_nodes": { "type": "integer", "description": "Cap on total nodes rendered (default 60, clamped 5-200)." },
                        "format":    { "type": "string", "description": "`mermaid` (default) or `dot`." }
                    },
                    "required": ["label", "key", "value"]
                }
            },
            {
                "name": "index_status",
                "description": "Report the live indexer's current state when the MCP server was started with `--watch`. Use this to wait until pending edits are reflected in the graph before issuing fresh queries: when `state` is `idle`, the most recent debounced batch is fully applied. Without `--watch`, returns a stub showing the live indexer is not running.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "diff_since",
                "description": "Walk the `:GitCommit` `[:PARENT_OF]` DAG and report what changed between the given commit and HEAD. Lists commits in the range and the `:File` / `:Function` nodes whose `first_seen_commit` lands inside it (i.e. added during the range). Removals are not tracked because the indexer does not keep tombstones — see the note in `docs/mcp-tools.md`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "commit": { "type": "string", "description": "Full hash or short_hash of the baseline commit." },
                        "limit":  { "type": "integer", "description": "Max items per category (default 50)." }
                    },
                    "required": ["commit"]
                }
            },
            {
                "name": "save_view",
                "description": "Persist a reusable Cypher query as a `:View` node. Future calls to `view(name, params)` re-run it. Use this for queries you find yourself running repeatedly (\"orphan steps\", \"public functions with no callers\"). The cypher may contain `$placeholder` tokens; `view` will substitute them at run time using `escape_str` on the supplied values.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name":        { "type": "string", "description": "View name (identifier-like: letters, digits, '_', '-')." },
                        "cypher":      { "type": "string", "description": "Cypher query body. Use $foo tokens for run-time parameters." },
                        "description": { "type": "string", "description": "Optional one-line summary." }
                    },
                    "required": ["name", "cypher"]
                }
            },
            {
                "name": "view",
                "description": "Run a previously-saved `:View` and return its rows as a Markdown table. `params` is an object whose entries replace `$key` tokens in the saved cypher (escaped via `escape_str`). Unknown / missing tokens fall through unchanged.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name":   { "type": "string", "description": "View name." },
                        "params": { "type": "object", "description": "Optional substitution map for $tokens in the cypher." }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "list_views",
                "description": "List every saved `:View` as a Markdown table (name, description, created_at, last_run_at).",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "find_symbol",
                "description": "Fuzzy substring search over `:Function` and `:Symbol` `qualified_name` / `name` (case-insensitive). Returns a Markdown table of `kind`, `qualified_name`, `file:line`, and the first line of the body as a signature. The graph equivalent of an editor's symbol picker.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Substring to look for in qualified_name or name." },
                        "limit": { "type": "integer", "description": "Max results (default 25)." },
                        "kind":  { "type": "string", "description": "Optional exact match against the `kind` property (e.g. 'fn', 'struct', 'method')." }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "worklog_create",
                "description": "Create a new :WorklogItem with an initial :Status (default `pending`) and optional first :Comment. Optionally attach [:RELATES_TO] edges to existing nodes via a Cypher MATCH that binds variable `t`. Status must be one of: pending, in_progress, done, blocked, abandoned. Kind classifies the work and must be one of: bug, feature, task, refactor, perf, docs (default 'task'). Worklog items survive --full reindex.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title":   { "type": "string", "description": "Human title (1 line)." },
                        "area":    { "type": "string", "description": "Optional grouping tag, e.g. 'indexer', 'mcp', 'docs'." },
                        "kind":    { "type": "string", "description": "Type of work: bug | feature | task | refactor | perf | docs (default 'task')." },
                        "status":  { "type": "string", "description": "Initial status (default 'pending')." },
                        "comment": { "type": "string", "description": "Optional first comment attached to the initial status." },
                        "author":  { "type": "string", "description": "Comment author (default 'claude')." },
                        "id":      { "type": "string", "description": "Optional explicit id (letters/digits/_/-). Auto-generated from title+timestamp if omitted." },
                        "match":   { "type": "string", "description": "Optional Cypher MATCH binding `t` to link [:RELATES_TO] targets." }
                    },
                    "required": ["title"]
                }
            },
            {
                "name": "worklog_set_status",
                "description": "Append a new :Status node to an existing :WorklogItem (status history is append-only). Optionally include a comment to be attached to the new status. Status timeline is queryable via the :HAS_STATUS chain.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id":      { "type": "string", "description": "Worklog item id." },
                        "status":  { "type": "string", "description": "New status (pending|in_progress|done|blocked|abandoned)." },
                        "comment": { "type": "string", "description": "Optional comment on the new status." },
                        "author":  { "type": "string", "description": "Comment author (default 'claude')." }
                    },
                    "required": ["id", "status"]
                }
            },
            {
                "name": "worklog_comment",
                "description": "Attach a :Comment to the latest :Status of a worklog item (each status can carry many comments). Use this to record thoughts that arrive AFTER the status transition was made.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id":     { "type": "string", "description": "Worklog item id." },
                        "body":   { "type": "string", "description": "Comment body (Markdown allowed)." },
                        "author": { "type": "string", "description": "Author (default 'claude')." }
                    },
                    "required": ["id", "body"]
                }
            },
            {
                "name": "worklog_list",
                "description": "List :WorklogItem nodes as a Markdown table. Optional filters: `area`, `status`, `kind`. Sorted by latest status timestamp.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "area":   { "type": "string", "description": "Filter by area." },
                        "status": { "type": "string", "description": "Filter by current_status." },
                        "kind":   { "type": "string", "description": "Filter by kind (bug | feature | task | refactor | perf | docs)." },
                        "limit":  { "type": "integer", "description": "Max items (default 100)." }
                    }
                }
            },
            {
                "name": "worklog_md",
                "description": "Render a full dossier for one :WorklogItem: metadata, related nodes, and the chronological :Status timeline with all :Comment nodes nested under each status.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Worklog item id." }
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "impact",
                "description": "Compute the transitive blast radius of a node. Walks `CALLS` outwards (callees) and inwards (callers) up to `depth`, and one-hop for `MENTIONS` (docs) and `IMPLEMENTED_BY` (scenarios). Returns a Markdown report with counts per category and the top-N affected nodes. Use this before refactoring or deleting a function to see who is affected.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string", "description": "Node label, defaults to 'Function'." },
                        "key":   { "type": "string", "description": "Identifying property, defaults to 'qualified_name'." },
                        "value": { "type": "string", "description": "Value of the identifying property." },
                        "depth": { "type": "integer", "description": "Max BFS depth for CALLS (default 3, capped at 6)." },
                        "top":   { "type": "integer", "description": "Max nodes shown per category (default 15)." }
                    },
                    "required": ["value"]
                }
            }
        ]
    })
}

// `format_cell`, `format_table`, `md_cell`, `format_table_md` moved to `render` — see refactoring 1a.

// ── Tool handlers ─────────────────────────────────────────────────────────────

fn handle_schema(db: &Db) -> Value {
    let mut text = String::new();

    let mut labels: BTreeSet<String> = BTreeSet::new();
    match db.query("MATCH (n) RETURN DISTINCT labels(n) AS lbls LIMIT 5000") {
        Ok(t) => {
            if let Some(idx) = t.col("lbls") {
                for row in &t.rows {
                    if let Some(Cell::Text(s) | Cell::Json(s)) = row.get(idx) {
                        // velr renders lists as JSON; fall back to the literal cell text.
                        if let Ok(arr) = serde_json::from_str::<Vec<String>>(s) {
                            for l in arr {
                                labels.insert(l);
                            }
                        } else {
                            labels.insert(s.clone());
                        }
                    }
                }
            }
        }
        Err(e) => {
            text.push_str(&format!("(could not enumerate labels: {e})\n"));
        }
    }

    let mut edge_types: BTreeSet<String> = BTreeSet::new();
    match db.query("MATCH ()-[r]->() RETURN DISTINCT type(r) AS t LIMIT 5000") {
        Ok(t) => {
            if let Some(idx) = t.col("t") {
                for row in &t.rows {
                    if let Some(Cell::Text(s) | Cell::Json(s)) = row.get(idx) {
                        edge_types.insert(s.clone());
                    }
                }
            }
        }
        Err(e) => {
            text.push_str(&format!("(could not enumerate edge types: {e})\n"));
        }
    }

    text.push_str("=== Vertex Labels ===\n");
    if labels.is_empty() {
        text.push_str("  (none)\n");
    } else {
        for lbl in &labels {
            text.push_str(&format!("  :{}\n", lbl));
        }
    }

    text.push_str("\n=== Edge Types ===\n");
    if edge_types.is_empty() {
        text.push_str("  (none)\n");
    } else {
        for et in &edge_types {
            text.push_str(&format!("  -[:{}]->\n", et));
        }
    }

    text.push_str("\n=== Supported Cypher (quick ref) ===\n");
    text.push_str("  Read:  MATCH (n:Label {prop: val})-[:TYPE]->(m) WHERE ... RETURN ... ORDER BY ... SKIP N LIMIT N\n");
    text.push_str("  Write: CREATE / MERGE / SET / REMOVE / DELETE / DETACH DELETE\n");
    text.push_str("  Agg:   count() sum() avg() min() max() collect()\n");

    ok_text(text.trim_end().to_string())
}

fn handle_cypher(db: &Db, params: &Value) -> Value {
    let Some(query) = params.get("query").and_then(|v| v.as_str()) else {
        return err_text("missing required argument: query".to_string());
    };
    match db.query(query) {
        Ok(t) => ok_text(format_table(&t)),
        Err(e) => err_text(format!("query error: {e}")),
    }
}

// `handle_begin` / `handle_write` / `handle_commit` / `handle_rollback`
// moved to `tx` module — see refactoring 1a.

fn handle_explain(db: &Db, params: &Value) -> Value {
    let Some(query) = params.get("query").and_then(|v| v.as_str()) else {
        return err_text("missing required argument: query".to_string());
    };
    // velr's `Velr::explain` returns an `ExplainTrace` without `Display`/`Debug`,
    // but `EXPLAIN <query>` runs as a multi-table query that we can read directly.
    let explain_query = format!("EXPLAIN {query}");
    match db.query_many(&explain_query) {
        Ok(tables) => {
            let mut buf = String::new();
            for (i, t) in tables.iter().enumerate() {
                if i > 0 {
                    buf.push_str("\n---\n");
                }
                buf.push_str(&format_table(t));
            }
            if buf.is_empty() {
                buf.push_str("(no plan returned)");
            }
            ok_text(buf)
        }
        Err(e) => err_text(format!("explain error: {e}")),
    }
}

fn handle_cypher_md(db: &Db, params: &Value) -> Value {
    let Some(query) = params.get("query").and_then(|v| v.as_str()) else {
        return err_text("missing required argument: query".to_string());
    };
    match db.query(query) {
        Ok(t) => ok_text(format_table_md(&t)),
        Err(e) => err_text(format!("query error: {e}")),
    }
}

fn handle_node_md(db: &Db, params: &Value) -> Value {
    let (label, key, value) = match parse_node_address(params) {
        Ok(t) => t,
        Err(e) => return err_text(e),
    };
    let neighbours_limit = params
        .get("neighbours_limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(25)
        .max(1);
    let val_lit = escape_str(&value);
    let mut out = String::new();
    out.push_str(&format!("# `:{label} {{{key}: {value:?}}}`\n\n"));

    // Properties
    let props_q =
        format!("MATCH (n:{label} {{{key}: {val_lit}}}) RETURN properties(n) AS props LIMIT 1");
    match db.query(&props_q) {
        Ok(t) if !t.rows.is_empty() => {
            out.push_str("## Properties\n\n");
            if let Some(Cell::Json(s) | Cell::Text(s)) = t.rows[0].first() {
                out.push_str("```json\n");
                out.push_str(s);
                if !s.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n\n");
            } else {
                out.push_str("_(no properties returned)_\n\n");
            }
        }
        Ok(_) => {
            return ok_text(format!(
                "# Not found\n\nNo `:{label}` with `{key} = {value:?}`.\n"
            ));
        }
        Err(e) => {
            out.push_str(&format!("_(could not fetch properties: {e})_\n\n"));
        }
    }

    // Outgoing neighbours grouped by edge type
    let out_q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}})-[r]->(m) \
         RETURN type(r) AS rel, labels(m) AS lbls, m.qualified_name AS qn, \
                m.name AS nm, m.path AS path \
         ORDER BY rel LIMIT {}",
        neighbours_limit * 50
    );
    out.push_str("## Outgoing edges\n\n");
    out.push_str(&render_neighbours(db, &out_q, neighbours_limit));
    out.push('\n');

    // Incoming neighbours grouped by edge type
    let in_q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}})<-[r]-(m) \
         RETURN type(r) AS rel, labels(m) AS lbls, m.qualified_name AS qn, \
                m.name AS nm, m.path AS path \
         ORDER BY rel LIMIT {}",
        neighbours_limit * 50
    );
    out.push_str("## Incoming edges\n\n");
    out.push_str(&render_neighbours(db, &in_q, neighbours_limit));
    out.push('\n');

    // Notes attached to this node
    let notes_q = format!(
        "MATCH (note:Note)-[:NOTES]->(n:{label} {{{key}: {val_lit}}}) \
         RETURN note.title AS title, note.author AS author, note.created_at AS created_at, \
                note.tags AS tags, note.markdown AS markdown \
         ORDER BY note.created_at DESC LIMIT 50"
    );
    if let Ok(t) = db.query(&notes_q) {
        if !t.rows.is_empty() {
            out.push_str(&format!("## Notes ({})\n\n", t.rows.len()));
            out.push_str(&render_notes_rows(&t));
        }
    }

    ok_text(out.trim_end().to_string())
}

// `neighbour_degrees` and `render_neighbours` moved to `render` — see refactoring 1a.

// `render_notes_rows` moved to `render` — see refactoring 1a.

// ── DB freshness check ────────────────────────────────────────────────────────

/// Best-effort mtime for the velr database. velr is SQLite-backed, so when
/// it runs in WAL mode the main file's mtime can lag the actual last-write
/// (writes go to `<path>-wal` first). We pick the latest of the three
/// candidates so we don't miss an external indexer run that hasn't yet
/// flushed its WAL.
fn db_mtime(path: &str) -> Option<std::time::SystemTime> {
    let candidates = [
        path.to_string(),
        format!("{path}-wal"),
        format!("{path}-shm"),
    ];
    candidates
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()))
        .max()
}

fn maybe_reopen(
    db: &mut Db,
    db_path: &str,
    last_opened: &mut Option<std::time::SystemTime>,
    tx: &TxState,
) {
    if !tx.pending.is_empty() {
        return;
    }
    let Some(disk) = db_mtime(db_path) else {
        return;
    };
    let stale = match last_opened {
        Some(opened) => disk > *opened,
        None => true,
    };
    if !stale {
        return;
    }
    match Db::open(db_path) {
        Ok(fresh) => {
            *db = fresh;
            *last_opened = Some(disk);
            eprintln!("[mcp] reopened DB at {db_path} (on-disk mtime advanced)");
        }
        Err(e) => {
            eprintln!("[mcp] reopen failed for {db_path}: {e} — keeping existing handle");
        }
    }
}

// ── Main loop ─────────────────────────────────────────────────────────────────
// `is_indexable_event_path`, `IndexStatus`, `SharedStatus`,
// `new_shared_status`, `handle_index_status`, `rel_paths_from`, and
// `spawn_indexer_watcher` all moved to the `watch` module — see
// refactoring 1a.

const HELP: &str = "\
codegraph-mcp — MCP server exposing a velr-backed graph database to LLM agents

USAGE:
    codegraph-mcp --db <path> [--watch <workspace>]
    codegraph-mcp <path>
    codegraph-mcp report --db <path> --out <dir>

The MCP server reads JSON-RPC requests on stdin and writes responses on
stdout. With --watch, a background thread re-runs the indexer whenever
files in <workspace> change (debounced 500ms). The indexer's standard
incremental path is used; uncommitted edits are not picked up until they
are committed. See docs/mcp-tools.md.

The `report` subcommand renders the graph-stored worklog
(:WorklogItem / :Status / :Comment) into <dir>/ROADMAP.md and
<dir>/WORKLOG.md.

OPTIONS:
    --db <path>          velr database file
    --watch <workspace>  Re-run the indexer on file changes in <workspace>
    --debounce-ms <ms>   Watcher debounce window (default 500)
    --out <dir>          (report) destination directory for the generated docs
    -h, --help           Show this help and exit
    -V, --version        Print version and exit
";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("codegraph-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let arg_value = |name: &str| -> Option<String> {
        args.iter()
            .zip(args.iter().skip(1))
            .find(|(f, _)| f.as_str() == name)
            .map(|(_, v)| v.clone())
    };
    if args.iter().skip(1).any(|a| a == "report") {
        let db_path = arg_value("--db").unwrap_or_else(|| {
            eprintln!("report: missing --db <path>");
            std::process::exit(2);
        });
        let out_dir = arg_value("--out").unwrap_or_else(|| "docs".to_string());
        match report::run(&db_path, &out_dir) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("report failed: {e}");
                std::process::exit(1);
            }
        }
    }
    let db_path = arg_value("--db")
        .or_else(|| args.iter().skip(1).find(|a| !a.starts_with("--")).cloned())
        .unwrap_or_else(|| {
            eprintln!("Usage: codegraph-mcp --db <path>\n\nRun with --help for details.");
            std::process::exit(1);
        });
    let watch_path = arg_value("--watch");
    let debounce_ms: u64 = arg_value("--debounce-ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);

    let status = new_shared_status();
    if let Some(ws) = &watch_path {
        spawn_indexer_watcher(ws.clone(), db_path.clone(), debounce_ms, status.clone());
    }

    let mut db = Db::open(&db_path).unwrap_or_else(|e| {
        eprintln!("Failed to open database at {db_path}: {e}");
        std::process::exit(1);
    });
    let mut last_opened_mtime = db_mtime(&db_path);

    let mut tx = TxState::new();

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => l,
            Err(_) => break,
        };

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let msg = error_response(&Value::Null, -32700, &format!("Parse error: {e}"));
                writeln!(out, "{}", serde_json::to_string(&msg).unwrap()).ok();
                out.flush().ok();
                continue;
            }
        };

        let reply = match req.method.as_str() {
            "initialize" => response(
                &req.id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "codegraph-mcp", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
            "notifications/initialized" | "notifications/cancelled" => continue,
            "tools/list" => response(&req.id, tool_list()),
            "tools/call" => {
                let name = req
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let params = req.params.get("arguments").unwrap_or(&Value::Null);

                maybe_reopen(&mut db, &db_path, &mut last_opened_mtime, &tx);

                let content = match name {
                    "schema" => handle_schema(&db),
                    "cypher" => handle_cypher(&db, params),
                    "begin" => handle_begin(&mut tx, params),
                    "write" => handle_write(&db, &mut tx, params),
                    "commit" => handle_commit(&db, &mut tx),
                    "rollback" => handle_rollback(&mut tx),
                    "explain" => handle_explain(&db, params),
                    "cypher_md" => handle_cypher_md(&db, params),
                    "node_md" => handle_node_md(&db, params),
                    "write_note" => handle_write_note(&db, params),
                    "list_notes" => handle_list_notes(&db, params),
                    "history" => handle_history(&db, params),
                    "impact" => handle_impact(&db, params),
                    "find_symbol" => handle_find_symbol(&db, params),
                    "save_view" => handle_save_view(&db, params),
                    "view" => handle_view(&db, params),
                    "list_views" => handle_list_views(&db),
                    "diff_since" => handle_diff_since(&db, params),
                    "define_concept" => handle_define_concept(&db, params),
                    "concept" => handle_concept(&db, params),
                    "coverage_md" => handle_coverage_md(&db, params),
                    "explore" => handle_explore(&db, params),
                    "graph_export" => handle_graph_export(&db, params),
                    "dead_code" => handle_dead_code(&db, params),
                    "arch_overlay" => handle_arch_overlay(&db, watch_path.as_deref(), params),
                    "list_concepts" => handle_list_concepts(&db),
                    "index_status" => {
                        handle_index_status(&status, watch_path.as_deref(), &tx, &db_path)
                    }
                    "import_pr_notes" => handle_import_pr_notes(&db, params),
                    "watch" => handle_watch(&db, params),
                    "unwatch" => handle_unwatch(&db, params),
                    "list_watches" => handle_list_watches(&db),
                    "worklog_create" => handle_worklog_create(&db, params),
                    "worklog_set_status" => handle_worklog_set_status(&db, params),
                    "worklog_comment" => handle_worklog_comment(&db, params),
                    "worklog_list" => handle_worklog_list(&db, params),
                    "worklog_md" => handle_worklog_md(&db, params),
                    other => err_text(format!("unknown tool: {other}")),
                };
                response(&req.id, content)
            }
            "ping" => response(&req.id, json!({})),
            other => error_response(&req.id, -32601, &format!("Method not found: {other}")),
        };

        writeln!(out, "{}", serde_json::to_string(&reply).unwrap()).ok();
        out.flush().ok();
    }
}

#[cfg(test)]
mod tests;
