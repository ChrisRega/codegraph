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

mod concepts;
mod coverage;
mod diff;
mod explore;
mod find;
mod history;
mod impact;
mod notes;
mod pr_notes;
mod render;
mod tx;
mod util;
mod views;
mod watch;
mod watch_tools;
use concepts::{handle_concept, handle_define_concept, handle_list_concepts};
use coverage::handle_coverage_md;
use diff::handle_diff_since;
use explore::handle_explore;
use find::handle_find_symbol;
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

The MCP server reads JSON-RPC requests on stdin and writes responses on
stdout. With --watch, a background thread re-runs the indexer whenever
files in <workspace> change (debounced 500ms). The indexer's standard
incremental path is used; uncommitted edits are not picked up until they
are committed. See docs/mcp-tools.md.

OPTIONS:
    --db <path>          velr database file
    --watch <workspace>  Re-run the indexer on file changes in <workspace>
    --debounce-ms <ms>   Watcher debounce window (default 500)
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
                    "list_concepts" => handle_list_concepts(&db),
                    "index_status" => handle_index_status(&status, watch_path.as_deref()),
                    "import_pr_notes" => handle_import_pr_notes(&db, params),
                    "watch" => handle_watch(&db, params),
                    "unwatch" => handle_unwatch(&db, params),
                    "list_watches" => handle_list_watches(&db),
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
mod tests {
    use super::*;
    use crate::pr_notes::extract_backticked_symbols;
    use crate::util::chrono_now_iso;
    use crate::views::substitute_view_params;
    use crate::watch::{is_indexable_event_path, new_shared_status, rel_paths_from};
    use codegraph_core::Table;

    #[test]
    fn tool_list_advertises_expected_tools() {
        let v = tool_list();
        let names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in [
            "schema",
            "cypher",
            "begin",
            "write",
            "commit",
            "rollback",
            "explain",
            "cypher_md",
            "node_md",
            "write_note",
            "list_notes",
            "history",
            "impact",
            "find_symbol",
            "save_view",
            "view",
            "list_views",
            "diff_since",
            "define_concept",
            "concept",
            "list_concepts",
            "coverage_md",
            "explore",
            "index_status",
            "import_pr_notes",
            "watch",
            "unwatch",
            "list_watches",
        ] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
    }

    #[test]
    fn format_table_md_renders_gfm() {
        let t = Table {
            columns: vec!["name".into(), "n".into()],
            rows: vec![
                vec![Cell::Text("alpha".into()), Cell::Integer(1)],
                vec![Cell::Text("beta|x".into()), Cell::Integer(2)],
            ],
        };
        let md = format_table_md(&t);
        assert!(md.contains("| name | n |"));
        assert!(md.contains("| --- | --- |"));
        // pipe inside a cell must be escaped
        assert!(md.contains("beta\\|x"));
        assert!(md.contains("_2 rows_"));
    }

    #[test]
    fn format_table_md_handles_empty() {
        let t = Table {
            columns: vec![],
            rows: vec![],
        };
        assert_eq!(format_table_md(&t), "_(no results)_");
    }

    fn seed_db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (:Function {qualified_name: 'a::foo', name: 'foo', path: 'src/a.rs'})")
            .unwrap();
        db.run("CREATE (:Function {qualified_name: 'a::bar', name: 'bar', path: 'src/a.rs'})")
            .unwrap();
        db.run(
            "MATCH (a:Function {qualified_name: 'a::foo'}), (b:Function {qualified_name: 'a::bar'}) \
             CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();
        db
    }

    fn text_of(v: &Value) -> String {
        v["content"][0]["text"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn cypher_md_renders_table() {
        let db = seed_db();
        let v = handle_cypher_md(
            &db,
            &json!({ "query": "MATCH (f:Function) RETURN f.name AS name ORDER BY name" }),
        );
        let md = text_of(&v);
        assert!(md.contains("| name |"), "got {md}");
        assert!(md.contains("bar"));
        assert!(md.contains("foo"));
        assert!(md.contains("_2 rows_"));
    }

    #[test]
    fn node_md_lists_neighbours() {
        let db = seed_db();
        let v = handle_node_md(
            &db,
            &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
        );
        let md = text_of(&v);
        assert!(md.contains("# `:Function"));
        assert!(md.contains("## Properties"));
        assert!(md.contains("## Outgoing edges"));
        assert!(md.contains("-[:CALLS]->"));
        assert!(md.contains("a::bar"));
    }

    #[test]
    fn node_md_ranks_neighbours_by_degree() {
        let db = Db::open_in_memory().unwrap();
        // hub has many incoming/outgoing edges; leaf has just one.
        for n in ["seed", "hub", "leaf", "x1", "x2", "x3"] {
            db.run(&format!(
                "CREATE (:Function {{qualified_name: 'r::{n}', name: '{n}'}})"
            ))
            .unwrap();
        }
        // seed -> hub, seed -> leaf
        for tgt in ["hub", "leaf"] {
            db.run(&format!(
                "MATCH (a:Function {{qualified_name: 'r::seed'}}), \
                       (b:Function {{qualified_name: 'r::{tgt}'}}) \
                 CREATE (a)-[:CALLS]->(b)"
            ))
            .unwrap();
        }
        // hub gets multiple extra edges so its degree dwarfs leaf's.
        for tgt in ["x1", "x2", "x3"] {
            db.run(&format!(
                "MATCH (a:Function {{qualified_name: 'r::hub'}}), \
                       (b:Function {{qualified_name: 'r::{tgt}'}}) \
                 CREATE (a)-[:CALLS]->(b)"
            ))
            .unwrap();
        }
        let v = handle_node_md(
            &db,
            &json!({ "label": "Function", "key": "qualified_name", "value": "r::seed" }),
        );
        let md = text_of(&v);
        // hub must appear before leaf in the outgoing CALLS group.
        let pos_hub = md.find("r::hub").expect("hub missing");
        let pos_leaf = md.find("r::leaf").expect("leaf missing");
        assert!(pos_hub < pos_leaf, "hub should outrank leaf:\n{md}");
        // degree tag rendered for non-zero degree
        assert!(md.contains("_(deg "), "degree tag missing:\n{md}");
    }

    #[test]
    fn index_status_renders_stub_without_watch() {
        let status = new_shared_status();
        let v = handle_index_status(&status, None);
        let md = text_of(&v);
        assert!(md.contains("# Indexer status"), "{md}");
        assert!(md.contains("not running"), "{md}");
    }

    #[test]
    fn index_status_reflects_watcher_state() {
        let status = new_shared_status();
        // Simulate a completed run.
        if let Ok(mut s) = status.lock() {
            s.state = "idle".to_string();
            s.last_run_at = "2026-05-15T18:55:00Z".to_string();
            s.last_run_mode = "live".to_string();
            s.last_run_duration_ms = 142;
            s.runs_total = 7;
            s.last_paths = vec!["src/lib.rs".into(), "README.md".into()];
            s.head_hash = "abcd1234ef567890".into();
        }
        let md = text_of(&handle_index_status(&status, Some("/tmp/ws")));
        assert!(md.contains("`/tmp/ws`"), "{md}");
        assert!(md.contains("`idle`"), "{md}");
        assert!(md.contains("`live`"), "{md}");
        assert!(md.contains("142ms"), "{md}");
        assert!(md.contains("Runs total:** 7"), "{md}");
        assert!(md.contains("`abcd1234`"), "{md}");
        assert!(md.contains("src/lib.rs"));
        assert!(md.contains("README.md"));
    }

    #[test]
    fn explore_returns_seed_and_neighbours_within_budget() {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (:Function {qualified_name: 'm::root', name: 'root'})")
            .unwrap();
        for n in ["a", "b", "c", "d", "e"] {
            db.run(&format!(
                "CREATE (:Function {{qualified_name: 'm::{n}', name: '{n}'}})"
            ))
            .unwrap();
            db.run(&format!(
                "MATCH (r:Function {{qualified_name: 'm::root'}}), \
                       (n:Function {{qualified_name: 'm::{n}'}}) CREATE (r)-[:CALLS]->(n)"
            ))
            .unwrap();
        }
        // Make `m::a` a hub.
        for tgt in ["b", "c", "d"] {
            db.run(&format!(
                "MATCH (a:Function {{qualified_name: 'm::a'}}), \
                       (b:Function {{qualified_name: 'm::{tgt}'}}) CREATE (a)-[:CALLS]->(b)"
            ))
            .unwrap();
        }

        let v = handle_explore(
            &db,
            &json!({
                "label": "Function", "key": "qualified_name", "value": "m::root",
                "char_budget": 8000, "max_depth": 2
            }),
        );
        let md = text_of(&v);
        assert!(md.contains("# Explore:"), "{md}");
        assert!(md.contains("## Seed"));
        assert!(md.contains("Neighbourhood"));
        let pos_a = md.find("m::a").expect("hub missing");
        let pos_e = md.find("m::e").expect("leaf missing");
        assert!(pos_a < pos_e, "hub should outrank leaf:\n{md}");
        assert!(md.contains("Showed"));
    }

    #[test]
    fn explore_respects_tight_budget_and_reports_drops() {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (:Function {qualified_name: 'r::seed', name: 'seed'})")
            .unwrap();
        for i in 0..30 {
            db.run(&format!(
                "CREATE (:Function {{qualified_name: 'r::n{i}', name: 'n{i}'}})"
            ))
            .unwrap();
            db.run(&format!(
                "MATCH (s:Function {{qualified_name: 'r::seed'}}), \
                       (n:Function {{qualified_name: 'r::n{i}'}}) CREATE (s)-[:CALLS]->(n)"
            ))
            .unwrap();
        }
        let md = text_of(&handle_explore(
            &db,
            &json!({
                "label": "Function", "key": "qualified_name", "value": "r::seed",
                "char_budget": 800, "max_depth": 1
            }),
        ));
        assert!(
            md.len() <= 1000,
            "exceeded budget: {} bytes\n{md}",
            md.len()
        );
        assert!(md.contains("dropped"));
        let visible = (0..30).filter(|i| md.contains(&format!("r::n{i}"))).count();
        assert!(
            visible > 0 && visible < 30,
            "expected partial truncation, got {visible}/30:\n{md}"
        );
    }

    #[test]
    fn explore_handles_unknown_seed() {
        let db = Db::open_in_memory().unwrap();
        let md = text_of(&handle_explore(
            &db,
            &json!({"label":"Function","key":"qualified_name","value":"nope"}),
        ));
        assert!(md.contains("# Not found"));
    }

    #[test]
    fn coverage_md_surfaces_orphans_and_untested() {
        let db = Db::open_in_memory().unwrap();
        // Two functions; `caller` calls `callee`. `caller` has no inbound
        // CALLS (orphan); `callee` has inbound CALLS but no `:TESTS`.
        for n in ["caller", "callee"] {
            db.run(&format!(
                "CREATE (:Function {{qualified_name: 'm::{n}', name: '{n}'}})"
            ))
            .unwrap();
        }
        db.run(
            "MATCH (a:Function {qualified_name: 'm::caller'}), \
                   (b:Function {qualified_name: 'm::callee'}) CREATE (a)-[:CALLS]->(b)",
        )
        .unwrap();
        // A file with no notes attached.
        db.run("CREATE (:File {path: 'src/lonely.rs'})").unwrap();
        // A package with no doc-mentioned function.
        db.run("CREATE (:Package {name: 'undocumented-pkg'})")
            .unwrap();

        let v = handle_coverage_md(&db, &json!({ "limit": 10 }));
        let md = text_of(&v);
        assert!(md.contains("# Graph coverage report"));
        // orphan section: caller is orphan, callee is not.
        assert!(md.contains("Orphan functions"));
        assert!(md.contains("m::caller"));
        // untested ranked section: callee has fan-in 1 and no TESTS.
        assert!(md.contains("Untested functions"));
        assert!(md.contains("m::callee"));
        // file with no note.
        assert!(md.contains("Files with no `:Note`"));
        assert!(md.contains("src/lonely.rs"));
        // undocumented package.
        assert!(md.contains("Packages with zero doc-mentions"));
        assert!(md.contains("undocumented-pkg"));
    }

    #[test]
    fn coverage_md_excludes_test_functions_from_orphans() {
        let db = Db::open_in_memory().unwrap();
        // A test function with no inbound CALLS should NOT show up in orphans.
        db.run("CREATE (:Function:Test {qualified_name: 'm::it_works', name: 'it_works'})")
            .unwrap();
        let md = text_of(&handle_coverage_md(&db, &json!({})));
        assert!(
            !md.contains("m::it_works"),
            "test fn should be excluded from orphans/untested:\n{md}"
        );
    }

    #[test]
    fn is_indexable_event_path_skips_indexer_outputs() {
        // Sidecar that the indexer itself writes — would feedback-loop the watcher.
        assert!(!is_indexable_event_path(std::path::Path::new(
            "/ws/codegraph.db.codegraph-meta.json"
        )));
        // velr db + its SQLite siblings.
        assert!(!is_indexable_event_path(std::path::Path::new(
            "/ws/codegraph.db"
        )));
        assert!(!is_indexable_event_path(std::path::Path::new(
            "/ws/codegraph.db-wal"
        )));
        assert!(!is_indexable_event_path(std::path::Path::new(
            "/ws/codegraph.db-shm"
        )));
        // Real source still passes.
        assert!(is_indexable_event_path(std::path::Path::new(
            "/ws/src/main.rs"
        )));
        // Other JSON files still indexable (e.g. OpenAPI spec).
        assert!(is_indexable_event_path(std::path::Path::new(
            "/ws/api/openapi.json"
        )));
    }

    #[test]
    fn rel_paths_from_strips_workspace_prefix() {
        let ws = std::path::PathBuf::from("/tmp/ws");
        let abs = vec![
            std::path::PathBuf::from("/tmp/ws/src/a.rs"),
            std::path::PathBuf::from("/tmp/ws/docs/x.md"),
            // Outside workspace ⇒ dropped.
            std::path::PathBuf::from("/etc/passwd"),
        ];
        let rels = rel_paths_from(&ws, &abs);
        assert_eq!(rels, vec!["src/a.rs".to_string(), "docs/x.md".into()]);
    }

    #[test]
    fn write_note_attaches_and_list_finds_it() {
        let db = seed_db();
        let res = handle_write_note(
            &db,
            &json!({
                "match": "MATCH (t:Function {qualified_name: 'a::foo'})",
                "title": "hot path",
                "markdown": "called from request handler",
                "author": "claude",
                "tags": "perf,hot"
            }),
        );
        let txt = text_of(&res);
        assert!(txt.contains("attached to 1 target"), "got {txt}");

        // appears in list_notes (unfiltered)
        let list = text_of(&handle_list_notes(&db, &json!({})));
        assert!(list.contains("hot path"));
        assert!(list.contains("called from request handler"));

        // appears under the function in node_md
        let node = text_of(&handle_node_md(
            &db,
            &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
        ));
        assert!(node.contains("## Notes"));
        assert!(node.contains("hot path"));
    }

    #[test]
    fn write_note_rejects_when_no_target_matches() {
        let db = seed_db();
        let res = handle_write_note(
            &db,
            &json!({
                "match": "MATCH (t:Function {qualified_name: 'does::not::exist'})",
                "markdown": "irrelevant"
            }),
        );
        assert!(res
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false));
        // and no orphan note was left behind
        let count = db
            .query("MATCH (n:Note) RETURN count(n) AS c")
            .unwrap()
            .rows[0][0]
            .as_i64()
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn node_md_rejects_unsafe_label() {
        let db = seed_db();
        let res = handle_node_md(
            &db,
            &json!({ "label": "Function`); MATCH (n) DETACH DELETE n; //",
                     "key": "qualified_name", "value": "x" }),
        );
        assert!(res
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false));
    }

    #[test]
    fn history_handler_lists_commits() {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (:GitCommit {hash: 'aaa111', short_hash: 'aaa111', message: 'first', timestamp: '2026-01-01T00:00:00Z'})").unwrap();
        db.run("CREATE (:GitCommit {hash: 'bbb222', short_hash: 'bbb222', message: 'second', timestamp: '2026-02-01T00:00:00Z'})").unwrap();
        let v = handle_history(&db, &json!({}));
        let md = text_of(&v);
        assert!(md.contains("# Indexed commits"));
        assert!(md.contains("aaa111"));
        assert!(md.contains("bbb222"));
    }

    #[test]
    fn impact_reports_callers_and_callees() {
        let db = Db::open_in_memory().unwrap();
        // Chain a -> b -> c -> d, plus e -> b (so b has two callers transitively from a's POV).
        for name in ["a", "b", "c", "d", "e"] {
            db.run(&format!(
                "CREATE (:Function {{qualified_name: 'x::{name}', name: '{name}', path: 'src/x.rs'}})"
            ))
            .unwrap();
        }
        for (caller, callee) in [("a", "b"), ("b", "c"), ("c", "d"), ("e", "b")] {
            db.run(&format!(
                "MATCH (u:Function {{qualified_name: 'x::{caller}'}}), \
                       (v:Function {{qualified_name: 'x::{callee}'}}) \
                 CREATE (u)-[:CALLS]->(v)"
            ))
            .unwrap();
        }
        let v = handle_impact(&db, &json!({ "value": "x::b", "depth": 3, "top": 10 }));
        let md = text_of(&v);
        assert!(md.contains("# Impact:"), "got {md}");
        assert!(md.contains("Blast radius:"));
        // b's callees transitively: c, d
        assert!(md.contains("x::c"));
        assert!(md.contains("x::d"));
        // b's callers: a, e
        assert!(md.contains("x::a"));
        assert!(md.contains("x::e"));
    }

    #[test]
    fn watch_unwatch_lifecycle() {
        let db = seed_db();
        let r = handle_watch(
            &db,
            &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
        );
        let txt = text_of(&r);
        assert!(txt.contains("watching"), "got {txt}");

        let lw = text_of(&handle_list_watches(&db));
        assert!(lw.contains("a::foo"), "got {lw}");

        let r = handle_unwatch(
            &db,
            &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
        );
        assert!(text_of(&r).contains("unwatched"));
        let lw2 = text_of(&handle_list_watches(&db));
        assert!(lw2.contains("nothing is watched"), "got {lw2}");
    }

    #[test]
    fn watch_rejects_unknown_node() {
        let db = seed_db();
        let r = handle_watch(
            &db,
            &json!({ "label": "Function", "key": "qualified_name", "value": "no::such" }),
        );
        assert!(r.get("isError").and_then(|v| v.as_bool()).unwrap_or(false));
    }

    #[test]
    fn extract_backticked_symbols_strips_calls_and_codeblocks() {
        let body = "Looking at `foo` and `a::bar()`. Don't include\n```\n`fenced::not_a_symbol`\n```\nbut keep `Baz`.";
        let syms = extract_backticked_symbols(body);
        assert!(syms.contains(&"foo".to_string()), "{syms:?}");
        assert!(syms.contains(&"a::bar".to_string()), "{syms:?}");
        assert!(syms.contains(&"Baz".to_string()), "{syms:?}");
        assert!(
            !syms.iter().any(|s| s.contains("fenced")),
            "fenced block leaked: {syms:?}"
        );
    }

    #[test]
    fn import_pr_notes_attaches_to_matching_function() {
        let db = seed_db();
        let v = handle_import_pr_notes(
            &db,
            &json!({
                "pr": "42",
                "comments": [
                    { "author": "alice", "body": "I think `foo` calls `bar` redundantly here.", "url": "https://example/c/1" },
                    { "author": "bob", "body": "totally unrelated chatter, nothing to see" },
                    { "author": "carol", "body": "see also `does_not_exist` for reference" }
                ]
            }),
        );
        let txt = text_of(&v);
        assert!(txt.contains("Processed 3 comments"), "got {txt}");
        assert!(txt.contains("created 1 notes"), "got {txt}");
        assert!(txt.contains("attached to 2"), "got {txt}");

        let dossier = text_of(&handle_node_md(
            &db,
            &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
        ));
        assert!(dossier.contains("PR 42 — alice"), "{dossier}");
        assert!(dossier.contains("redundantly"));
    }

    #[test]
    fn concept_lifecycle_define_then_render() {
        let db = seed_db();
        // Define a concept covering all functions in 'a::'.
        let r = handle_define_concept(
            &db,
            &json!({
                "name": "module-a",
                "description": "everything in module a",
                "match": "MATCH (t:Function) WHERE t.qualified_name STARTS WITH 'a::'"
            }),
        );
        let txt = text_of(&r);
        assert!(txt.contains("describes 2 members"), "got {txt}");

        // list_concepts shows it
        let lc = text_of(&handle_list_concepts(&db));
        assert!(lc.contains("module-a"), "got {lc}");
        assert!(lc.contains("everything in module a"));

        // dossier
        let c = text_of(&handle_concept(&db, &json!({ "name": "module-a" })));
        assert!(c.contains("# Concept `module-a`"));
        assert!(c.contains("everything in module a"));
        assert!(c.contains("a::foo"));
        assert!(c.contains("a::bar"));
        // No tests in seed_db, but section header still appears.
        assert!(c.contains("Functions in scope"));
    }

    #[test]
    fn concept_unknown_returns_not_found() {
        let db = seed_db();
        let v = handle_concept(&db, &json!({ "name": "nope" }));
        assert!(text_of(&v).contains("# Not found"));
    }

    #[test]
    fn diff_since_lists_commits_and_added_nodes() {
        let db = Db::open_in_memory().unwrap();
        // Three commits in chronological order.
        for (h, sh, ts, msg) in [
            ("aaa1aaa1", "aaa1aaa", "2026-01-01T00:00:00Z", "first"),
            ("bbb2bbb2", "bbb2bbb", "2026-01-02T00:00:00Z", "second"),
            ("ccc3ccc3", "ccc3ccc", "2026-01-03T00:00:00Z", "third"),
        ] {
            db.run(&format!(
                "CREATE (:GitCommit {{hash: '{h}', short_hash: '{sh}', \
                 timestamp: '{ts}', message: '{msg}'}})"
            ))
            .unwrap();
        }
        // Workspace + SNAPSHOT_OF on HEAD (ccc3).
        db.run("CREATE (:Workspace {name: 'ws'})").unwrap();
        db.run(
            "MATCH (c:GitCommit {hash: 'ccc3ccc3'}), (w:Workspace) \
             CREATE (c)-[:SNAPSHOT_OF]->(w)",
        )
        .unwrap();
        // Functions with first_seen at different commits.
        db.run("CREATE (:Function {qualified_name: 'old::a', first_seen_commit: 'aaa1aaa1'})")
            .unwrap();
        db.run("CREATE (:Function {qualified_name: 'mid::b', first_seen_commit: 'bbb2bbb2'})")
            .unwrap();
        db.run("CREATE (:Function {qualified_name: 'new::c', first_seen_commit: 'ccc3ccc3'})")
            .unwrap();
        // File added at bbb.
        db.run("CREATE (:File {path: 'src/new.rs', first_seen_commit: 'bbb2bbb2'})")
            .unwrap();

        let v = handle_diff_since(&db, &json!({ "commit": "aaa1aaa" }));
        let md = text_of(&v);
        // commits in range: bbb + ccc (not aaa itself; baseline excluded).
        assert!(md.contains("bbb2bbb"), "got {md}");
        assert!(md.contains("ccc3ccc"));
        // added Functions: mid::b and new::c, but NOT old::a
        assert!(md.contains("mid::b"));
        assert!(md.contains("new::c"));
        assert!(!md.contains("old::a"));
        // added File
        assert!(md.contains("src/new.rs"));
        // tombstone caveat present
        assert!(md.contains("Removals are"));
    }

    #[test]
    fn diff_since_unknown_commit_returns_message() {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (:GitCommit {hash: 'aaa', short_hash: 'aaa', timestamp: '2026-01-01T00:00:00Z'})").unwrap();
        db.run("CREATE (:Workspace {name: 'ws'})").unwrap();
        db.run("MATCH (c:GitCommit), (w:Workspace) CREATE (c)-[:SNAPSHOT_OF]->(w)")
            .unwrap();
        let v = handle_diff_since(&db, &json!({ "commit": "zzznope" }));
        let md = text_of(&v);
        assert!(md.contains("no `:GitCommit` matches"), "got {md}");
    }

    #[test]
    fn substitute_view_params_replaces_tokens() {
        let mut m = serde_json::Map::new();
        m.insert("name".into(), Value::String("a::foo".into()));
        m.insert("limit".into(), Value::Number(10.into()));
        let out = substitute_view_params(
            "MATCH (f:Function {qualified_name: $name}) RETURN f LIMIT $limit",
            &m,
        );
        // 'a::foo' must be string-escaped, 10 must come through as a string-escaped numeric.
        assert!(out.contains("'a::foo'"), "got {out}");
        assert!(out.contains("'10'"), "got {out}");
        // unknown token stays:
        let out2 = substitute_view_params("RETURN $unknown", &m);
        assert_eq!(out2, "RETURN $unknown");
    }

    #[test]
    fn save_view_then_view_runs_with_params() {
        let db = seed_db();
        let r = handle_save_view(
            &db,
            &json!({
                "name": "by-name",
                "cypher": "MATCH (f:Function {name: $name}) RETURN f.qualified_name AS qn",
                "description": "find a function by short name"
            }),
        );
        assert!(text_of(&r).contains("saved view"));

        // list_views surfaces it
        let lv = text_of(&handle_list_views(&db));
        assert!(lv.contains("by-name"), "got {lv}");
        assert!(lv.contains("find a function by short name"));

        // running with params returns the matching row
        let v = text_of(&handle_view(
            &db,
            &json!({ "name": "by-name", "params": { "name": "foo" } }),
        ));
        assert!(v.contains("a::foo"), "got {v}");
        assert!(v.contains("```cypher"));
    }

    #[test]
    fn view_unknown_name_returns_empty_message() {
        let db = seed_db();
        let v = text_of(&handle_view(&db, &json!({ "name": "does-not-exist" })));
        assert!(v.contains("no view named"));
    }

    #[test]
    fn save_view_rejects_invalid_name() {
        let db = seed_db();
        let r = handle_save_view(
            &db,
            &json!({ "name": "bad name with spaces", "cypher": "RETURN 1" }),
        );
        assert!(r.get("isError").and_then(|v| v.as_bool()).unwrap_or(false));
    }

    #[test]
    fn find_symbol_ranks_exact_above_substring() {
        let db = Db::open_in_memory().unwrap();
        // Names where "format" appears in different positions / completeness.
        for (qn, name, kind, line) in [
            ("a::format_table", "format_table", "fn", 10),
            ("a::format", "format", "fn", 20), // exact
            ("a::reformat_input", "reformat_input", "fn", 30),
            ("a::unrelated", "unrelated", "fn", 40),
        ] {
            db.run(&format!(
                "CREATE (s:Function {{qualified_name: '{qn}', name: '{name}', kind: '{kind}', \
                 line_start: {line}, body: 'fn {name}() {{}}' }})"
            ))
            .unwrap();
        }
        db.run("CREATE (:File {path: 'src/a.rs'})").unwrap();
        db.run(
            "MATCH (s:Function), (f:File {path: 'src/a.rs'}) \
             WHERE s.qualified_name STARTS WITH 'a::' \
             CREATE (s)-[:DEFINED_IN]->(f)",
        )
        .unwrap();

        let v = handle_find_symbol(&db, &json!({ "query": "format", "limit": 10 }));
        let md = text_of(&v);
        assert!(md.contains("a::format"), "got {md}");
        // Exact match must come before substring matches.
        let pos_exact = md.find("a::format").unwrap();
        let pos_table = md.find("a::format_table").unwrap();
        let pos_re = md.find("a::reformat_input").unwrap();
        assert!(pos_exact < pos_table, "exact should rank first");
        assert!(
            pos_table < pos_re,
            "name-startsWith should outrank tail-substring"
        );
        assert!(!md.contains("unrelated"));
    }

    #[test]
    fn find_symbol_returns_no_match_message() {
        let db = seed_db();
        let v = handle_find_symbol(&db, &json!({ "query": "nonexistent_zzzz" }));
        let md = text_of(&v);
        assert!(
            md.contains("no `:Function` or `:Symbol` matched"),
            "got {md}"
        );
    }

    #[test]
    fn impact_handles_unknown_seed() {
        let db = seed_db();
        let v = handle_impact(&db, &json!({ "value": "does::not::exist" }));
        let md = text_of(&v);
        assert!(md.contains("# Not found"));
    }

    #[test]
    fn chrono_now_iso_is_iso_shaped() {
        let s = chrono_now_iso();
        assert!(s.ends_with('Z'), "got {s}");
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[7], b'-');
        assert_eq!(s.as_bytes()[10], b'T');
    }

    #[test]
    fn format_table_renders_rows_as_tsv() {
        let t = Table {
            columns: vec!["name".into(), "n".into()],
            rows: vec![
                vec![Cell::Text("alpha".into()), Cell::Integer(1)],
                vec![Cell::Text("beta".into()), Cell::Integer(2)],
            ],
        };
        let out = format_table(&t);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "name\tn");
        assert!(lines[1].contains("alpha"));
        assert!(lines[1].ends_with("\t1"));
        assert!(lines[2].contains("beta"));
    }

    #[test]
    fn format_table_handles_empty() {
        let t = Table {
            columns: vec![],
            rows: vec![],
        };
        assert_eq!(format_table(&t), "(no results)");
    }
}
