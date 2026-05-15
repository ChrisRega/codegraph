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

use codegraph_core::{escape_str, Cell, Db, Table};
use serde::Deserialize;
use serde_json::{json, Value};

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

fn ok_text(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn err_text(msg: String) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

// ── Transaction state ─────────────────────────────────────────────────────────

struct TxState {
    active: bool,
    message: Option<String>,
    pending: Vec<String>,
}

impl TxState {
    fn new() -> Self {
        Self {
            active: false,
            message: None,
            pending: Vec::new(),
        }
    }
}

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

// ── Result formatting ─────────────────────────────────────────────────────────

fn format_cell(c: &Cell) -> String {
    match c {
        Cell::Null => "null".to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Integer(i) => i.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Text(s) => format!("{:?}", s),
        Cell::Json(s) => s.clone(),
    }
}

fn format_table(t: &Table) -> String {
    if t.columns.is_empty() && t.rows.is_empty() {
        return "(no results)".to_string();
    }
    if t.rows.is_empty() {
        return format!("(no rows; columns: {})", t.columns.join(", "));
    }
    let mut out = String::new();
    out.push_str(&t.columns.join("\t"));
    out.push('\n');
    for row in &t.rows {
        let cells: Vec<String> = row.iter().map(format_cell).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out.trim_end().to_string()
}

// ── Markdown rendering ────────────────────────────────────────────────────────

/// Escape a single cell for inclusion in a GFM table cell.
/// Pipes break columns; newlines break rows. Both must go.
fn md_cell(c: &Cell) -> String {
    let raw = match c {
        Cell::Null => "—".to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Integer(i) => i.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Json(s) => s.clone(),
    };
    raw.replace('|', "\\|").replace(['\n', '\r', '\t'], " ")
}

fn format_table_md(t: &Table) -> String {
    if t.columns.is_empty() && t.rows.is_empty() {
        return "_(no results)_".to_string();
    }
    if t.rows.is_empty() {
        return format!(
            "_(no rows; columns: {})_",
            t.columns
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut out = String::new();
    out.push_str("| ");
    out.push_str(&t.columns.join(" | "));
    out.push_str(" |\n");
    out.push('|');
    for _ in &t.columns {
        out.push_str(" --- |");
    }
    out.push('\n');
    for row in &t.rows {
        out.push_str("| ");
        let cells: Vec<String> = row.iter().map(md_cell).collect();
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
    }
    format!(
        "{out}\n_{} row{}_",
        t.rows.len(),
        if t.rows.len() == 1 { "" } else { "s" }
    )
}

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

fn handle_begin(tx: &mut TxState, params: &Value) -> Value {
    if tx.active {
        return ok_text(format!(
            "transaction already open ({} queries buffered)",
            tx.pending.len()
        ));
    }
    tx.active = true;
    tx.message = params
        .get("message")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    tx.pending.clear();
    ok_text("transaction opened".to_string())
}

fn handle_write(db: &Db, tx: &mut TxState, params: &Value) -> Value {
    let Some(query) = params.get("query").and_then(|v| v.as_str()) else {
        return err_text("missing required argument: query".to_string());
    };

    if tx.active {
        tx.pending.push(query.to_string());
        return ok_text(format!("buffered (#{} pending)", tx.pending.len()));
    }

    match db.run(query) {
        Ok(()) => ok_text("OK — write applied".to_string()),
        Err(e) => err_text(format!("execution error: {e}")),
    }
}

fn handle_commit(db: &Db, tx: &mut TxState) -> Value {
    if !tx.active {
        return err_text("no open transaction — use `begin` first".to_string());
    }
    if tx.pending.is_empty() {
        tx.active = false;
        tx.message = None;
        return ok_text("transaction committed (nothing to apply)".to_string());
    }

    let queries: Vec<String> = tx.pending.drain(..).collect();
    tx.active = false;
    let _msg = tx.message.take();

    // Replay inside a velr transaction so failures roll back the batch.
    let velr = db.velr();
    let velr_tx = match velr.begin_tx() {
        Ok(t) => t,
        Err(e) => return err_text(format!("could not begin velr transaction: {e}")),
    };

    for (i, q) in queries.iter().enumerate() {
        if let Err(e) = velr_tx.run(q) {
            return err_text(format!(
                "query #{} failed; transaction rolled back:\n  {q}\nError: {e}",
                i + 1
            ));
        }
    }

    if let Err(e) = velr_tx.commit() {
        return err_text(format!("commit failed: {e}"));
    }
    ok_text(format!("committed {} queries", queries.len()))
}

fn handle_rollback(tx: &mut TxState) -> Value {
    if !tx.active {
        return err_text("no open transaction".to_string());
    }
    let n = tx.pending.len();
    tx.active = false;
    tx.message = None;
    tx.pending.clear();
    ok_text(format!(
        "rolled back ({n} buffered queries discarded, nothing was written)"
    ))
}

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
    let label = match params.get("label").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return err_text("missing required argument: label".to_string()),
    };
    let key = match params.get("key").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return err_text("missing required argument: key".to_string()),
    };
    let value = match params.get("value").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return err_text("missing required argument: value".to_string()),
    };
    let neighbours_limit = params
        .get("neighbours_limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(25)
        .max(1);

    // Reject anything that doesn't look like a bare identifier — defensive,
    // since label/key are inlined directly into Cypher.
    if !safe_ident(&label) {
        return err_text(format!("invalid label: {label}"));
    }
    if !safe_ident(&key) {
        return err_text(format!("invalid key: {key}"));
    }

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

/// Best-effort total degree (in + out) for the given `qualified_name`s.
/// Returns an empty map if the aggregating query fails — the caller treats
/// missing entries as degree 0 and sorts them last, so a velr regression
/// just degrades to alphabetical ordering instead of erroring out.
fn neighbour_degrees(db: &Db, qns: &[String]) -> std::collections::HashMap<String, i64> {
    use std::collections::HashMap;
    let mut map: HashMap<String, i64> = HashMap::new();
    if qns.is_empty() {
        return map;
    }
    let in_list = qns
        .iter()
        .map(|s| escape_str(s))
        .collect::<Vec<_>>()
        .join(",");
    let q = format!(
        "MATCH (m) WHERE m.qualified_name IN [{in_list}] \
         OPTIONAL MATCH (m)-[r]-() \
         RETURN m.qualified_name AS qn, count(r) AS deg"
    );
    if let Ok(t) = db.query(&q) {
        for row in &t.rows {
            let qn = row
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let deg = row.get(1).and_then(|c| c.as_i64()).unwrap_or(0);
            if !qn.is_empty() {
                map.insert(qn, deg);
            }
        }
    }
    map
}

fn render_neighbours(db: &Db, query: &str, limit_per_rel: i64) -> String {
    let t = match db.query(query) {
        Ok(t) => t,
        Err(e) => return format!("_(query error: {e})_\n"),
    };
    if t.rows.is_empty() {
        return "_(none)_\n".to_string();
    }
    use std::collections::BTreeMap;
    // (lbls, identity, qn_for_degree_lookup)
    let mut groups: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    let rel_i = t.col("rel");
    let lbl_i = t.col("lbls");
    let qn_i = t.col("qn");
    let nm_i = t.col("nm");
    let pa_i = t.col("path");
    let mut degree_lookup_qns: Vec<String> = Vec::new();
    for row in &t.rows {
        let rel = rel_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("?")
            .to_string();
        let lbls = lbl_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("[]")
            .to_string();
        let qn = qn_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let identity = if !qn.is_empty() {
            qn.clone()
        } else {
            nm_i.and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .or_else(|| pa_i.and_then(|i| row.get(i)).and_then(|c| c.as_str()))
                .unwrap_or("?")
                .to_string()
        };
        if !qn.is_empty() {
            degree_lookup_qns.push(qn.clone());
        }
        groups.entry(rel).or_default().push((lbls, identity, qn));
    }

    let degrees = neighbour_degrees(db, &degree_lookup_qns);

    let mut out = String::new();
    for (rel, mut items) in groups {
        // Sort by degree desc, then by identity asc for stable output.
        items.sort_by(|a, b| {
            let da = degrees.get(&a.2).copied().unwrap_or(0);
            let db_ = degrees.get(&b.2).copied().unwrap_or(0);
            db_.cmp(&da).then_with(|| a.1.cmp(&b.1))
        });
        let total = items.len();
        let truncated = total > limit_per_rel as usize;
        items.truncate(limit_per_rel as usize);
        out.push_str(&format!(
            "- **`-[:{rel}]->`** ({total}{})\n",
            if truncated {
                format!(", showing top {limit_per_rel}")
            } else {
                String::new()
            }
        ));
        for (lbls, ident, qn) in items {
            let deg_tag = degrees
                .get(&qn)
                .filter(|d| **d > 0)
                .map(|d| format!(" _(deg {d})_"))
                .unwrap_or_default();
            out.push_str(&format!("  - `{lbls}` `{ident}`{deg_tag}\n"));
        }
    }
    out
}

fn handle_write_note(db: &Db, params: &Value) -> Value {
    let match_clause = match params.get("match").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return err_text("missing required argument: match".to_string()),
    };
    let markdown = match params.get("markdown").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return err_text("missing required argument: markdown".to_string()),
    };
    let title = params
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let author = params
        .get("author")
        .and_then(|v| v.as_str())
        .unwrap_or("claude")
        .to_string();
    let tags = params
        .get("tags")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Lightweight guard: the user-supplied MATCH must bind variable `t`.
    let lower = match_clause.to_lowercase();
    if !lower.contains("match") || !match_clause.contains('t') {
        return err_text("`match` must be a Cypher MATCH clause that binds variable `t`".into());
    }

    let now = chrono_now_iso();
    let note_id = format!("note-{}", now.replace([':', '.'], "-"));

    // Create the note node + attach via :NOTES edge to every target.
    let q = format!(
        "{match_clause} \
         MERGE (n:Note {{id: {id}}}) \
         SET n.title = {title}, n.author = {author}, n.tags = {tags}, \
             n.created_at = {created}, n.markdown = {md} \
         CREATE (n)-[:NOTES]->(t)",
        id = escape_str(&note_id),
        title = escape_str(&title),
        author = escape_str(&author),
        tags = escape_str(&tags),
        created = escape_str(&now),
        md = escape_str(&markdown),
    );
    if let Err(e) = db.run(&q) {
        return err_text(format!("note write failed: {e}"));
    }

    // Count how many targets got the note.
    let count_q = format!(
        "MATCH (n:Note {{id: {}}})-[:NOTES]->(x) RETURN count(x) AS c",
        escape_str(&note_id)
    );
    let attached = db
        .query(&count_q)
        .ok()
        .and_then(|t| t.rows.into_iter().next())
        .and_then(|r| r.into_iter().next())
        .and_then(|c| c.as_i64())
        .unwrap_or(0);

    if attached == 0 {
        // No target matched — clean up the orphan note so we don't accumulate junk.
        let _ = db.run(&format!(
            "MATCH (n:Note {{id: {}}}) DETACH DELETE n",
            escape_str(&note_id)
        ));
        return err_text(
            "MATCH bound no targets — note discarded. Verify your MATCH clause first with `cypher`.".into(),
        );
    }
    ok_text(format!(
        "wrote note `{note_id}` attached to {attached} target{}",
        if attached == 1 { "" } else { "s" }
    ))
}

fn handle_list_notes(db: &Db, params: &Value) -> Value {
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .max(1);
    let q = match params.get("match").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => format!(
            "{s} \
             MATCH (note:Note)-[:NOTES]->(t) \
             RETURN DISTINCT note.id AS id, note.title AS title, note.author AS author, \
                    note.created_at AS created_at, note.tags AS tags, note.markdown AS markdown \
             ORDER BY note.created_at DESC LIMIT {limit}"
        ),
        _ => format!(
            "MATCH (note:Note) \
             RETURN note.id AS id, note.title AS title, note.author AS author, \
                    note.created_at AS created_at, note.tags AS tags, note.markdown AS markdown \
             ORDER BY note.created_at DESC LIMIT {limit}"
        ),
    };
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("list_notes query failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text("_(no notes)_".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("# Notes ({})\n\n", t.rows.len()));
    out.push_str(&render_notes_rows(&t));
    ok_text(out.trim_end().to_string())
}

fn render_notes_rows(t: &Table) -> String {
    let mut out = String::new();
    let id_i = t.col("id");
    let title_i = t.col("title");
    let author_i = t.col("author");
    let created_i = t.col("created_at");
    let tags_i = t.col("tags");
    let md_i = t.col("markdown");
    for row in &t.rows {
        let id = id_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("?");
        let title = title_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let author = author_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let created = created_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let tags = tags_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let md = md_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let heading = if title.is_empty() { id } else { title };
        out.push_str(&format!("## {heading}\n\n"));
        out.push_str(&format!(
            "_id: `{id}` · author: `{author}` · created: `{created}`{}_\n\n",
            if tags.is_empty() {
                String::new()
            } else {
                format!(" · tags: `{tags}`")
            }
        ));
        out.push_str(md);
        if !md.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn handle_history(db: &Db, params: &Value) -> Value {
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .max(1);
    let q = format!(
        "MATCH (c:GitCommit) \
         OPTIONAL MATCH (a:Author)-[:AUTHORED]->(c) \
         RETURN c.short_hash AS short, c.timestamp AS ts, a.name AS author, c.message AS message \
         ORDER BY c.timestamp DESC LIMIT {limit}"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("history query failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text(
            "_(no `:GitCommit` nodes recorded — run the indexer inside a git repo)_".to_string(),
        );
    }
    let mut out = String::new();
    out.push_str(&format!("# Indexed commits ({})\n\n", t.rows.len()));
    out.push_str("| short | timestamp | author | message |\n| --- | --- | --- | --- |\n");
    let s_i = t.col("short");
    let ts_i = t.col("ts");
    let a_i = t.col("author");
    let m_i = t.col("message");
    for row in &t.rows {
        let s = s_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let ts = ts_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let a = a_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let m = m_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        out.push_str(&format!("| `{s}` | {ts} | {a} | {m} |\n"));
    }
    ok_text(out.trim_end().to_string())
}

// ── import_pr_notes ───────────────────────────────────────────────────────────

/// Extract backtick-delimited tokens from `body`. Tokens longer than 120
/// chars (almost certainly fenced code blocks) and tokens that don't look
/// like identifiers are dropped. Handles ```…``` blocks by skipping their
/// contents entirely.
fn extract_backticked_symbols(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        // Triple backtick ⇒ skip to the matching closer.
        if bytes.get(i + 1) == Some(&b'`') && bytes.get(i + 2) == Some(&b'`') {
            if let Some(rel_end) = body[i + 3..].find("```") {
                i = i + 3 + rel_end + 3;
            } else {
                break;
            }
            continue;
        }
        // Single backtick ⇒ find next single backtick.
        if let Some(rel_end) = body[i + 1..].find('`') {
            let raw = &body[i + 1..i + 1 + rel_end];
            // Strip a trailing `()` so `foo()` becomes `foo` before validation.
            let token = raw.trim_end_matches("()");
            if !token.is_empty()
                && token.len() <= 120
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '.')
            {
                out.push(token.to_string());
            }
            i = i + 1 + rel_end + 1;
        } else {
            break;
        }
    }
    out
}

fn lookup_function_targets(db: &Db, symbol: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut hits: BTreeSet<String> = BTreeSet::new();
    let s_lit = escape_str(symbol);
    for key in ["name", "qualified_name"] {
        let q = format!(
            "MATCH (f:Function) WHERE f.{key} = {s_lit} \
             RETURN f.qualified_name AS qn LIMIT 10"
        );
        if let Ok(t) = db.query(&q) {
            for row in &t.rows {
                if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                    hits.insert(qn.to_string());
                }
            }
        }
    }
    hits.into_iter().collect()
}

fn handle_import_pr_notes(db: &Db, params: &Value) -> Value {
    let pr = params
        .get("pr")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let comments = match params.get("comments").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return err_text("missing required argument: comments (array)".into()),
    };

    let mut comments_processed = 0usize;
    let mut notes_created = 0usize;
    let mut total_attached = 0usize;
    let mut symbols_seen = 0usize;
    let now_base = chrono_now_iso();

    for (idx, c) in comments.iter().enumerate() {
        let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let author = c.get("author").and_then(|v| v.as_str()).unwrap_or("github");
        let url = c.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if body.trim().is_empty() {
            continue;
        }
        comments_processed += 1;
        let symbols = extract_backticked_symbols(body);
        if symbols.is_empty() {
            continue;
        }
        symbols_seen += symbols.len();
        use std::collections::BTreeSet;
        let mut targets: BTreeSet<String> = BTreeSet::new();
        for s in &symbols {
            for qn in lookup_function_targets(db, s) {
                targets.insert(qn);
            }
        }
        if targets.is_empty() {
            continue;
        }

        let note_id = format!(
            "pr-{}-{}-{}",
            pr.replace(['/', '#', ' '], "_"),
            idx,
            now_base.replace([':', '.'], "-")
        );
        let title = format!("PR {pr} — {author}");
        let md = if url.is_empty() {
            body.to_string()
        } else {
            format!("{body}\n\n[source]({url})")
        };
        let upsert = format!(
            "MERGE (n:Note {{id: {id}}}) \
             SET n.title = {title}, n.author = {author}, n.tags = 'pr-comment', \
                 n.created_at = {now}, n.markdown = {md}",
            id = escape_str(&note_id),
            title = escape_str(&title),
            author = escape_str(author),
            now = escape_str(&now_base),
            md = escape_str(&md),
        );
        if db.run(&upsert).is_err() {
            continue;
        }
        notes_created += 1;
        for qn in &targets {
            let q = format!(
                "MATCH (n:Note {{id: {id}}}), (t:Function {{qualified_name: {qn}}}) \
                 MERGE (n)-[:NOTES]->(t)",
                id = escape_str(&note_id),
                qn = escape_str(qn),
            );
            if db.run(&q).is_ok() {
                total_attached += 1;
            }
        }
    }

    ok_text(format!(
        "Processed {comments_processed} comments, scanned {symbols_seen} backticked tokens, \
         created {notes_created} notes attached to {total_attached} `:Function` targets."
    ))
}

// ── concepts ──────────────────────────────────────────────────────────────────

fn handle_define_concept(db: &Db, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) if safe_view_name(s) => s.to_string(),
        Some(s) => return err_text(format!("invalid concept name: {s:?}")),
        None => return err_text("missing required argument: name".to_string()),
    };
    let match_clause = match params.get("match").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return err_text("missing required argument: match".to_string()),
    };
    let description = params
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let lower = match_clause.to_lowercase();
    if !lower.contains("match") || !match_clause.contains('t') {
        return err_text("`match` must be a Cypher MATCH clause binding `t`".into());
    }
    let now = chrono_now_iso();
    let upsert = format!(
        "MERGE (c:Concept {{name: {n}}}) \
         SET c.description = {d}, c.updated_at = {now}, \
             c.created_at = coalesce(c.created_at, {now})",
        n = escape_str(&name),
        d = escape_str(&description),
        now = escape_str(&now),
    );
    if let Err(e) = db.run(&upsert) {
        return err_text(format!("concept upsert failed: {e}"));
    }
    let attach = format!(
        "{match_clause} \
         MATCH (c:Concept {{name: {n}}}) \
         MERGE (c)-[:DESCRIBES]->(t)",
        n = escape_str(&name),
    );
    if let Err(e) = db.run(&attach) {
        return err_text(format!("concept attach failed: {e}"));
    }
    let count_q = format!(
        "MATCH (:Concept {{name: {n}}})-[:DESCRIBES]->(t) RETURN count(t) AS c",
        n = escape_str(&name),
    );
    let attached = db
        .query(&count_q)
        .ok()
        .and_then(|t| t.rows.into_iter().next())
        .and_then(|r| r.into_iter().next())
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    ok_text(format!(
        "concept `{name}` now describes {attached} member{}",
        if attached == 1 { "" } else { "s" }
    ))
}

fn handle_concept(db: &Db, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) if safe_view_name(s) => s.to_string(),
        Some(s) => return err_text(format!("invalid concept name: {s:?}")),
        None => return err_text("missing required argument: name".to_string()),
    };
    let n_lit = escape_str(&name);

    let head_q = format!(
        "MATCH (c:Concept {{name: {n_lit}}}) \
         RETURN c.description AS d, c.created_at AS created LIMIT 1"
    );
    let (description, created) = match db.query(&head_q) {
        Ok(t) if !t.rows.is_empty() => {
            let r = &t.rows[0];
            let d = r.first().and_then(|c| c.as_str()).unwrap_or("").to_string();
            let cr = r.get(1).and_then(|c| c.as_str()).unwrap_or("").to_string();
            (d, cr)
        }
        Ok(_) => return ok_text(format!("# Not found\n\nNo `:Concept` named `{name}`.\n")),
        Err(e) => return err_text(format!("concept lookup failed: {e}")),
    };

    let mut out = String::new();
    out.push_str(&format!("# Concept `{name}`\n\n"));
    if !description.is_empty() {
        out.push_str(&format!("> {description}\n\n"));
    }
    if !created.is_empty() {
        out.push_str(&format!("_created: {created}_\n\n"));
    }

    // Direct members.
    let members_q = format!(
        "MATCH (:Concept {{name: {n_lit}}})-[:DESCRIBES]->(t) \
         RETURN labels(t) AS lbls, t.qualified_name AS qn, t.path AS path, t.name AS name \
         LIMIT 200"
    );
    out.push_str("## Members\n\n");
    match db.query(&members_q) {
        Ok(t) if !t.rows.is_empty() => {
            for row in &t.rows {
                let lbls = row
                    .first()
                    .and_then(|c| c.as_str())
                    .unwrap_or("[]")
                    .to_string();
                let id = row
                    .get(1)
                    .and_then(|c| c.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        row.get(2)
                            .and_then(|c| c.as_str())
                            .filter(|s| !s.is_empty())
                    })
                    .or_else(|| row.get(3).and_then(|c| c.as_str()))
                    .unwrap_or("?")
                    .to_string();
                out.push_str(&format!("- `{lbls}` `{id}`\n"));
            }
            out.push('\n');
        }
        _ => out.push_str("_(none)_\n\n"),
    }

    // Functions reachable from members: members are :Function directly,
    // OR members are :DocSection that MENTIONS a :Function.
    // velr's OR-becomes-UNION quirk again — split into two queries.
    use std::collections::BTreeSet;
    let mut function_qns: BTreeSet<String> = BTreeSet::new();
    let q_direct = format!(
        "MATCH (:Concept {{name: {n_lit}}})-[:DESCRIBES]->(f:Function) \
         RETURN f.qualified_name AS qn"
    );
    if let Ok(t) = db.query(&q_direct) {
        for row in &t.rows {
            if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                function_qns.insert(qn.to_string());
            }
        }
    }
    let q_via_doc = format!(
        "MATCH (:Concept {{name: {n_lit}}})-[:DESCRIBES]->(:DocSection)-[:MENTIONS]->(f:Function) \
         RETURN f.qualified_name AS qn"
    );
    if let Ok(t) = db.query(&q_via_doc) {
        for row in &t.rows {
            if let Some(qn) = row.first().and_then(|c| c.as_str()) {
                function_qns.insert(qn.to_string());
            }
        }
    }

    out.push_str(&format!(
        "## Functions in scope ({})\n\n",
        function_qns.len()
    ));
    if function_qns.is_empty() {
        out.push_str("_(none)_\n\n");
    } else {
        for qn in function_qns.iter().take(50) {
            out.push_str(&format!("- `{qn}`\n"));
        }
        if function_qns.len() > 50 {
            out.push_str(&format!("- _… {} more_\n", function_qns.len() - 50));
        }
        out.push('\n');
    }

    // Tests covering those functions.
    if !function_qns.is_empty() {
        let in_list = function_qns
            .iter()
            .map(|s| escape_str(s))
            .collect::<Vec<_>>()
            .join(",");
        let tests_q = format!(
            "MATCH (t:Test)-[:TESTS]->(f:Function) WHERE f.qualified_name IN [{in_list}] \
             RETURN t.qualified_name AS test, f.qualified_name AS fn"
        );
        if let Ok(t) = db.query(&tests_q) {
            out.push_str(&format!("## Tests covering scope ({})\n\n", t.rows.len()));
            if t.rows.is_empty() {
                out.push_str("_(none)_\n\n");
            } else {
                for row in &t.rows {
                    let test = row
                        .first()
                        .and_then(|c| c.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let fn_ = row
                        .get(1)
                        .and_then(|c| c.as_str())
                        .unwrap_or("?")
                        .to_string();
                    out.push_str(&format!("- `{test}` → `{fn_}`\n"));
                }
                out.push('\n');
            }
        }

        // Notes on members or in-scope functions.
        let notes_q = format!(
            "MATCH (note:Note)-[:NOTES]->(f:Function) WHERE f.qualified_name IN [{in_list}] \
             RETURN note.title AS title, note.author AS author, note.created_at AS created_at, \
                    note.tags AS tags, note.markdown AS markdown \
             ORDER BY note.created_at DESC LIMIT 25"
        );
        if let Ok(t) = db.query(&notes_q) {
            if !t.rows.is_empty() {
                out.push_str(&format!(
                    "## Notes on functions in scope ({})\n\n",
                    t.rows.len()
                ));
                out.push_str(&render_notes_rows(&t));
            }
        }
    }

    ok_text(out.trim_end().to_string())
}

fn handle_list_concepts(db: &Db) -> Value {
    let q = "MATCH (c:Concept) \
             OPTIONAL MATCH (c)-[:DESCRIBES]->(t) \
             RETURN c.name AS name, c.description AS description, c.created_at AS created_at, \
                    count(t) AS members \
             ORDER BY c.name";
    let t = match db.query(q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("list_concepts failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text("_(no concepts defined)_".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("# Concepts ({})\n\n", t.rows.len()));
    out.push_str("| name | description | members | created_at |\n| --- | --- | --- | --- |\n");
    let n_i = t.col("name");
    let d_i = t.col("description");
    let c_i = t.col("created_at");
    let m_i = t.col("members");
    for row in &t.rows {
        let n = n_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let d = d_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let c = c_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let m = m_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        out.push_str(&format!("| `{n}` | {d} | {m} | {c} |\n"));
    }
    ok_text(out.trim_end().to_string())
}

// ── diff_since ────────────────────────────────────────────────────────────────

fn handle_diff_since(db: &Db, params: &Value) -> Value {
    let given = match params.get("commit").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return err_text("missing required argument: commit".to_string()),
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .max(1);

    let g_lit = escape_str(&given);
    // velr's planner turns `WHERE x = a OR y = a` into a UNION which clashes
    // with LIMIT placement, so try the two keys separately.
    let try_lookup = |key: &str| -> Option<(String, String, String)> {
        let q = format!(
            "MATCH (c:GitCommit) WHERE c.{key} = {g_lit} \
             RETURN c.hash AS hash, c.short_hash AS short, c.timestamp AS ts LIMIT 1"
        );
        let t = db.query(&q).ok()?;
        let r = t.rows.into_iter().next()?;
        let mut it = r.into_iter();
        let h = it.next().and_then(|c| c.as_str().map(str::to_string))?;
        let s = it
            .next()
            .and_then(|c| c.as_str().map(str::to_string))
            .unwrap_or_default();
        let ts = it
            .next()
            .and_then(|c| c.as_str().map(str::to_string))
            .unwrap_or_default();
        Some((h, s, ts))
    };
    let (given_hash, given_short, given_ts) =
        match try_lookup("hash").or_else(|| try_lookup("short_hash")) {
            Some(t) => t,
            None => return ok_text(format!("_(no `:GitCommit` matches `{given}`)_")),
        };

    let head_q = "MATCH (h:GitCommit)-[:SNAPSHOT_OF]->(:Workspace) \
                  RETURN h.hash AS hash, h.short_hash AS short, h.timestamp AS ts LIMIT 1";
    let (head_hash, head_short, head_ts) = match db.query(head_q) {
        Ok(t) if !t.rows.is_empty() => {
            let r = &t.rows[0];
            let h = r.first().and_then(|c| c.as_str()).unwrap_or("").to_string();
            let s = r.get(1).and_then(|c| c.as_str()).unwrap_or("").to_string();
            let ts = r.get(2).and_then(|c| c.as_str()).unwrap_or("").to_string();
            (h, s, ts)
        }
        _ => {
            return err_text(
                "no HEAD `:GitCommit` (no `[:SNAPSHOT_OF]->(:Workspace)` edge) — was the indexer ever run?".to_string(),
            )
        }
    };

    let gt_lit = escape_str(&given_ts);
    let ht_lit = escape_str(&head_ts);

    // Commits strictly newer than the baseline up to and including HEAD.
    let range_q = format!(
        "MATCH (c:GitCommit) WHERE c.timestamp > {gt_lit} AND c.timestamp <= {ht_lit} \
         OPTIONAL MATCH (a:Author)-[:AUTHORED]->(c) \
         RETURN c.hash AS hash, c.short_hash AS short, c.timestamp AS ts, \
                a.name AS author, c.message AS msg \
         ORDER BY c.timestamp"
    );
    let range = match db.query(&range_q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("range query failed: {e}")),
    };
    let range_hashes: Vec<String> = range
        .rows
        .iter()
        .filter_map(|r| r.first().and_then(|c| c.as_str()).map(|s| s.to_string()))
        .collect();

    let mut out = String::new();
    out.push_str(&format!(
        "# Diff since `{given_short}` → HEAD `{head_short}`\n\n"
    ));
    out.push_str(&format!(
        "_Baseline `{given_hash}` ({given_ts})_  \n_HEAD `{head_hash}` ({head_ts})_\n\n"
    ));

    if range_hashes.is_empty() {
        out.push_str("No commits between baseline and HEAD.\n");
        return ok_text(out.trim_end().to_string());
    }

    out.push_str(&format!("## Commits in range ({})\n\n", range_hashes.len()));
    out.push_str("| short | timestamp | author | message |\n| --- | --- | --- | --- |\n");
    let s_i = range.col("short");
    let ts_i = range.col("ts");
    let a_i = range.col("author");
    let m_i = range.col("msg");
    for row in range.rows.iter().take(limit as usize) {
        let s = s_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let ts = ts_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let a = a_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let m = m_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        out.push_str(&format!("| `{s}` | {ts} | {a} | {m} |\n"));
    }
    if range_hashes.len() > limit as usize {
        out.push_str(&format!(
            "_… {} more (raise `limit`)_\n",
            range_hashes.len() - limit as usize
        ));
    }
    out.push('\n');

    let in_list = range_hashes
        .iter()
        .map(|h| escape_str(h))
        .collect::<Vec<_>>()
        .join(",");

    let added_section = |label: &str, key: &str, alias: &str| -> String {
        let q = format!(
            "MATCH (n:{label}) WHERE n.first_seen_commit IN [{in_list}] \
             RETURN n.{key} AS {alias}, n.first_seen_commit AS first \
             ORDER BY n.{key} LIMIT {limit_x}",
            limit_x = limit + 1
        );
        let mut s = String::new();
        match db.query(&q) {
            Ok(t) => {
                if t.rows.is_empty() {
                    s.push_str(&format!("## Added `:{label}` (0)\n\n_(none)_\n\n"));
                } else {
                    let total = t.rows.len();
                    let truncated = total > limit as usize;
                    let shown = (limit as usize).min(total);
                    s.push_str(&format!(
                        "## Added `:{label}` ({}{})\n\n",
                        if truncated { ">=" } else { "" },
                        total
                    ));
                    s.push_str("| identifier | first_seen_commit |\n| --- | --- |\n");
                    for row in t.rows.iter().take(shown) {
                        let id = row
                            .first()
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        let f = row
                            .get(1)
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        let f_short = if f.len() > 8 { &f[..8] } else { &f };
                        s.push_str(&format!("| `{id}` | `{f_short}` |\n"));
                    }
                    if truncated {
                        s.push_str("_… more (raise `limit`)_\n");
                    }
                    s.push('\n');
                }
            }
            Err(e) => {
                s.push_str(&format!("## Added `:{label}`\n\n_(query failed: {e})_\n\n"));
            }
        }
        s
    };

    out.push_str(&added_section("File", "path", "id"));
    out.push_str(&added_section("Function", "qualified_name", "id"));

    out.push_str(
        "> Removals are **not** listed: the indexer detaches deleted nodes \
         on each pass and does not keep tombstones. To detect a removal, \
         compare two snapshots externally (e.g. `git log -S<symbol>`).\n",
    );

    ok_text(out.trim_end().to_string())
}

// ── saved views ───────────────────────────────────────────────────────────────

fn safe_view_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Substitute `$key` tokens in `cypher` with `escape_str(value)` for each
/// `(key, value)` in `params`. Tokens are matched as `$` followed by an
/// identifier-shaped run (`[A-Za-z_][A-Za-z0-9_]*`); unknown tokens stay.
fn substitute_view_params(cypher: &str, params: &serde_json::Map<String, Value>) -> String {
    let bytes = cypher.as_bytes();
    let mut out = String::with_capacity(cypher.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'$' && i + 1 < bytes.len() {
            let start = i + 1;
            let mut end = start;
            if end < bytes.len() && (bytes[end].is_ascii_alphabetic() || bytes[end] == b'_') {
                end += 1;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                let key = &cypher[start..end];
                if let Some(v) = params.get(key) {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        Value::Bool(b) => b.to_string(),
                        Value::Null => "null".to_string(),
                        other => other.to_string(),
                    };
                    out.push_str(&escape_str(&s));
                    i = end;
                    continue;
                }
            }
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn handle_save_view(db: &Db, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) if safe_view_name(s) => s.to_string(),
        Some(s) => return err_text(format!("invalid view name: {s:?}")),
        None => return err_text("missing required argument: name".to_string()),
    };
    let cypher = match params.get("cypher").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => return err_text("missing required argument: cypher".to_string()),
    };
    let description = params
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let now = chrono_now_iso();
    let q = format!(
        "MERGE (v:View {{name: {name}}}) \
         SET v.cypher = {cypher}, v.description = {desc}, v.updated_at = {now}, \
             v.created_at = coalesce(v.created_at, {now})",
        name = escape_str(&name),
        cypher = escape_str(&cypher),
        desc = escape_str(&description),
        now = escape_str(&now),
    );
    if let Err(e) = db.run(&q) {
        return err_text(format!("save_view failed: {e}"));
    }
    ok_text(format!("saved view `{name}`"))
}

fn handle_view(db: &Db, params: &Value) -> Value {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(s) if safe_view_name(s) => s.to_string(),
        Some(s) => return err_text(format!("invalid view name: {s:?}")),
        None => return err_text("missing required argument: name".to_string()),
    };
    let lookup = format!(
        "MATCH (v:View {{name: {n}}}) RETURN v.cypher AS cypher LIMIT 1",
        n = escape_str(&name),
    );
    let cypher_template = match db.query(&lookup) {
        Ok(t) if !t.rows.is_empty() => t.rows[0]
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
        Ok(_) => return ok_text(format!("_(no view named `{name}`)_")),
        Err(e) => return err_text(format!("view lookup failed: {e}")),
    };
    let empty = serde_json::Map::new();
    let map = params
        .get("params")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let cypher = substitute_view_params(&cypher_template, map);

    let now = chrono_now_iso();
    let _ = db.run(&format!(
        "MATCH (v:View {{name: {n}}}) SET v.last_run_at = {now}",
        n = escape_str(&name),
        now = escape_str(&now),
    ));

    let mut out = String::new();
    out.push_str(&format!("# View `{name}`\n\n"));
    out.push_str("```cypher\n");
    out.push_str(&cypher);
    if !cypher.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n\n");
    match db.query(&cypher) {
        Ok(t) => out.push_str(&format_table_md(&t)),
        Err(e) => out.push_str(&format!("_(query failed: {e})_")),
    }
    ok_text(out)
}

fn handle_list_views(db: &Db) -> Value {
    let q = "MATCH (v:View) RETURN v.name AS name, v.description AS description, \
             v.created_at AS created_at, v.last_run_at AS last_run_at \
             ORDER BY v.name";
    let t = match db.query(q) {
        Ok(t) => t,
        Err(e) => return err_text(format!("list_views failed: {e}")),
    };
    if t.rows.is_empty() {
        return ok_text("_(no saved views)_".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!("# Saved views ({})\n\n", t.rows.len()));
    out.push_str("| name | description | created_at | last_run_at |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    let n_i = t.col("name");
    let d_i = t.col("description");
    let c_i = t.col("created_at");
    let l_i = t.col("last_run_at");
    for row in &t.rows {
        let n = n_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let d = d_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let c = c_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        let l = l_i
            .and_then(|i| row.get(i))
            .map(md_cell)
            .unwrap_or_default();
        out.push_str(&format!("| `{n}` | {d} | {c} | {l} |\n"));
    }
    ok_text(out.trim_end().to_string())
}

// ── find_symbol ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct SymbolHit {
    label: String,
    qn: String,
    name: String,
    kind: String,
    path: String,
    line: i64,
    body: String,
}

/// Relevance score (lower is better): 0 exact, 1 startsWith on name,
/// 2 startsWith on qn, 3 contains on name, 4 contains on qn.
fn relevance(needle_lower: &str, hit: &SymbolHit) -> u8 {
    let name = hit.name.to_lowercase();
    let qn = hit.qn.to_lowercase();
    if name == needle_lower || qn == needle_lower {
        0
    } else if name.starts_with(needle_lower) {
        1
    } else if qn.starts_with(needle_lower) {
        2
    } else if name.contains(needle_lower) {
        3
    } else {
        4
    }
}

fn collect_symbols(db: &Db, label: &str, kind_filter_clause: &str) -> Vec<SymbolHit> {
    // Pull a generous candidate set; final filtering / scoring happens
    // client-side so we don't depend on velr's substring-match semantics.
    let q = format!(
        "MATCH (s:{label}) {kind_filter_clause} \
         OPTIONAL MATCH (s)-[:DEFINED_IN]->(f:File) \
         RETURN s.qualified_name AS qn, s.name AS name, s.kind AS kind, \
                f.path AS path, s.line_start AS line, s.body AS body \
         LIMIT 5000"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let qn_i = t.col("qn");
    let nm_i = t.col("name");
    let kd_i = t.col("kind");
    let pa_i = t.col("path");
    let ln_i = t.col("line");
    let bd_i = t.col("body");
    t.rows
        .iter()
        .map(|row| SymbolHit {
            label: label.to_string(),
            qn: qn_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            name: nm_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            kind: kd_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            path: pa_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            line: ln_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_i64())
                .unwrap_or(0),
            body: bd_i
                .and_then(|i| row.get(i))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect()
}

fn handle_find_symbol(db: &Db, params: &Value) -> Value {
    let needle = match params.get("query").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return err_text("missing required argument: query".to_string()),
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(25)
        .max(1) as usize;
    let kind = params
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let kind_clause = match &kind {
        Some(k) if !k.is_empty() => format!("WHERE s.kind = {}", escape_str(k)),
        _ => String::new(),
    };

    let needle_lower = needle.to_lowercase();
    let mut hits: Vec<SymbolHit> = collect_symbols(db, "Function", &kind_clause);
    hits.extend(collect_symbols(db, "Symbol", &kind_clause));

    hits.retain(|h| {
        h.qn.to_lowercase().contains(&needle_lower) || h.name.to_lowercase().contains(&needle_lower)
    });

    hits.sort_by(|a, b| {
        let ra = relevance(&needle_lower, a);
        let rb = relevance(&needle_lower, b);
        ra.cmp(&rb)
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.qn.cmp(&b.qn))
    });

    let total = hits.len();
    if total == 0 {
        return ok_text(format!(
            "_(no `:Function` or `:Symbol` matched `{needle}`)_"
        ));
    }
    hits.truncate(limit);

    let mut out = String::new();
    out.push_str(&format!(
        "# `find_symbol({needle:?})` — {} of {total} match{}\n\n",
        hits.len(),
        if total == 1 { "" } else { "es" }
    ));
    out.push_str("| kind | qualified_name | location | signature |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for h in &hits {
        let loc = if h.path.is_empty() {
            "—".to_string()
        } else if h.line > 0 {
            format!("`{}:{}`", h.path, h.line)
        } else {
            format!("`{}`", h.path)
        };
        let sig = h
            .body
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .replace('|', "\\|");
        let sig = if sig.is_empty() {
            "—".to_string()
        } else {
            format!("`{sig}`")
        };
        let label_tag = format!("{}:{}", h.label, h.kind);
        out.push_str(&format!(
            "| `{}` | `{}` | {loc} | {sig} |\n",
            md_cell(&Cell::Text(label_tag)),
            md_cell(&Cell::Text(h.qn.clone())),
        ));
    }
    if total > hits.len() {
        out.push_str(&format!(
            "\n_… {} more (raise `limit`)_",
            total - hits.len()
        ));
    }
    ok_text(out.trim_end().to_string())
}

// ── impact (blast radius) ─────────────────────────────────────────────────────

fn safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One BFS hop along `rel`, expanding from a frontier of nodes identified by
/// `(label, key, value ∈ frontier)`. Returns the next frontier as
/// `(qualified_name, path)` pairs not already in `visited`.
fn bfs_hop(
    db: &Db,
    label: &str,
    key: &str,
    rel: &str,
    outgoing: bool,
    frontier: &[String],
    visited: &mut BTreeSet<String>,
) -> Vec<(String, String)> {
    if frontier.is_empty() {
        return Vec::new();
    }
    let in_list = frontier
        .iter()
        .map(|s| escape_str(s))
        .collect::<Vec<_>>()
        .join(",");
    let pattern = if outgoing {
        format!("(n:{label})-[:{rel}]->(m:{label})")
    } else {
        format!("(n:{label})<-[:{rel}]-(m:{label})")
    };
    let q = format!(
        "MATCH {pattern} \
         WHERE n.{key} IN [{in_list}] \
         RETURN DISTINCT m.{key} AS qn, m.path AS path"
    );
    let t = match db.query(&q) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for row in &t.rows {
        let qn = row
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        if qn.is_empty() {
            continue;
        }
        if visited.insert(qn.clone()) {
            let path = row
                .get(1)
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            out.push((qn, path));
        }
    }
    out
}

/// BFS along `rel` from the seed up to `max_depth`, returning
/// `(qualified_name, path, depth)` for every newly discovered node.
fn bfs_collect(
    db: &Db,
    label: &str,
    key: &str,
    seed: &str,
    rel: &str,
    outgoing: bool,
    max_depth: i64,
) -> Vec<(String, String, i64)> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(seed.to_string());
    let mut frontier = vec![seed.to_string()];
    let mut found = Vec::new();
    for d in 1..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let next = bfs_hop(db, label, key, rel, outgoing, &frontier, &mut visited);
        let next_keys: Vec<String> = next.iter().map(|(qn, _)| qn.clone()).collect();
        for (qn, path) in next {
            found.push((qn, path, d));
        }
        frontier = next_keys;
    }
    found
}

fn render_impact_section(title: &str, items: &[(String, String, i64)], top: i64) -> String {
    let mut out = String::new();
    out.push_str(&format!("## {} ({})\n\n", title, items.len()));
    if items.is_empty() {
        out.push_str("_(none)_\n\n");
        return out;
    }
    let total = items.len();
    let shown = (top as usize).min(total);
    for (qn, path, depth) in items.iter().take(shown) {
        let path_part = if path.is_empty() {
            String::new()
        } else {
            format!(" — `{path}`")
        };
        out.push_str(&format!("- depth {depth}: `{qn}`{path_part}\n"));
    }
    if shown < total {
        out.push_str(&format!("- _… {} more_\n", total - shown));
    }
    out.push('\n');
    out
}

fn handle_impact(db: &Db, params: &Value) -> Value {
    let label = params
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("Function")
        .to_string();
    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("qualified_name")
        .to_string();
    let value = match params.get("value").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return err_text("missing required argument: value".to_string()),
    };
    let depth = params
        .get("depth")
        .and_then(|v| v.as_i64())
        .unwrap_or(3)
        .clamp(1, 6);
    let top = params
        .get("top")
        .and_then(|v| v.as_i64())
        .unwrap_or(15)
        .max(1);

    if !safe_ident(&label) {
        return err_text(format!("invalid label: {label}"));
    }
    if !safe_ident(&key) {
        return err_text(format!("invalid key: {key}"));
    }

    let val_lit = escape_str(&value);

    // Verify the seed exists (and grab its file).
    let seed_q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}}) \
         OPTIONAL MATCH (n)-[:DEFINED_IN]->(f:File) \
         RETURN f.path AS path LIMIT 1"
    );
    let (exists, def_file) = match db.query(&seed_q) {
        Ok(t) if !t.rows.is_empty() => (
            true,
            t.rows[0]
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        Ok(_) => (false, String::new()),
        Err(e) => return err_text(format!("seed lookup failed: {e}")),
    };
    if !exists {
        return ok_text(format!(
            "# Not found\n\nNo `:{label}` with `{key} = {value:?}`.\n"
        ));
    }

    let callees = bfs_collect(db, &label, &key, &value, "CALLS", true, depth);
    let callers = bfs_collect(db, &label, &key, &value, "CALLS", false, depth);

    // One-hop: doc sections that mention this node.
    let mentions_q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}})<-[:MENTIONS]-(s:DocSection) \
         RETURN s.qualified_name AS qn, s.path AS path"
    );
    let mut mentions: Vec<(String, String, i64)> = Vec::new();
    if let Ok(t) = db.query(&mentions_q) {
        for row in &t.rows {
            let qn = row
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let path = row
                .get(1)
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if !qn.is_empty() {
                mentions.push((qn, path, 1));
            }
        }
    }

    // One-hop: BDD steps implemented by this function.
    let steps_q = format!(
        "MATCH (n:{label} {{{key}: {val_lit}}})<-[:IMPLEMENTED_BY]-(st:Step) \
         RETURN st.qualified_name AS qn, st.text AS text"
    );
    let mut steps: Vec<(String, String, i64)> = Vec::new();
    if let Ok(t) = db.query(&steps_q) {
        for row in &t.rows {
            let qn = row
                .first()
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let text = row
                .get(1)
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if !qn.is_empty() {
                steps.push((qn, text, 1));
            }
        }
    }

    let total_radius = callees.len() + callers.len() + mentions.len() + steps.len();
    let mut out = String::new();
    out.push_str(&format!("# Impact: `:{label} {{{key}: {value:?}}}`\n\n"));
    if !def_file.is_empty() {
        out.push_str(&format!("Defined in `{def_file}`. "));
    }
    out.push_str(&format!(
        "**Blast radius: {total_radius}** \
         (callers {}, callees {}, doc mentions {}, scenario steps {}).\n\n",
        callers.len(),
        callees.len(),
        mentions.len(),
        steps.len()
    ));
    out.push_str(&render_impact_section(
        &format!("Callers (transitive, depth ≤ {depth})"),
        &callers,
        top,
    ));
    out.push_str(&render_impact_section(
        &format!("Callees (transitive, depth ≤ {depth})"),
        &callees,
        top,
    ));
    out.push_str(&render_impact_section("Doc mentions", &mentions, top));
    out.push_str(&render_impact_section(
        "Scenario steps implemented",
        &steps,
        top,
    ));

    ok_text(out.trim_end().to_string())
}

/// RFC 3339 / ISO 8601 timestamp without an external dep.
fn chrono_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let nanos = dur.subsec_nanos();
    // Days from epoch → civil date (Howard Hinnant).
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    let h = (secs_of_day / 3600) as u32;
    let mi = ((secs_of_day % 3600) / 60) as u32;
    let s = (secs_of_day % 60) as u32;
    format!(
        "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:06}Z",
        nanos / 1000
    )
}

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

const HELP: &str = "\
codegraph-mcp — MCP server exposing a velr-backed graph database to LLM agents

USAGE:
    codegraph-mcp --db <path>
    codegraph-mcp <path>

The MCP server reads JSON-RPC requests on stdin and writes responses on
stdout. Tools advertised: schema, cypher, begin, write, commit, rollback,
explain. See docs/mcp-tools.md for details.

OPTIONS:
    --db <path>     velr database file
    -h, --help      Show this help and exit
    -V, --version   Print version and exit
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
    let db_path = args
        .iter()
        .zip(args.iter().skip(1))
        .find(|(flag, _)| flag.as_str() == "--db")
        .map(|(_, val)| val.clone())
        .or_else(|| args.iter().skip(1).find(|a| !a.starts_with("--")).cloned())
        .unwrap_or_else(|| {
            eprintln!("Usage: codegraph-mcp --db <path>\n\nRun with --help for details.");
            std::process::exit(1);
        });

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
                    "list_concepts" => handle_list_concepts(&db),
                    "import_pr_notes" => handle_import_pr_notes(&db, params),
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
            "import_pr_notes",
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
