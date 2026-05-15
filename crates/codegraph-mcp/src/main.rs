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

fn render_neighbours(db: &Db, query: &str, limit_per_rel: i64) -> String {
    let t = match db.query(query) {
        Ok(t) => t,
        Err(e) => return format!("_(query error: {e})_\n"),
    };
    if t.rows.is_empty() {
        return "_(none)_\n".to_string();
    }
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let rel_i = t.col("rel");
    let lbl_i = t.col("lbls");
    let qn_i = t.col("qn");
    let nm_i = t.col("nm");
    let pa_i = t.col("path");
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
        let identity = qn_i
            .and_then(|i| row.get(i))
            .and_then(|c| c.as_str())
            .or_else(|| nm_i.and_then(|i| row.get(i)).and_then(|c| c.as_str()))
            .or_else(|| pa_i.and_then(|i| row.get(i)).and_then(|c| c.as_str()))
            .unwrap_or("?")
            .to_string();
        groups.entry(rel).or_default().push((lbls, identity));
    }
    let mut out = String::new();
    for (rel, mut items) in groups {
        let total = items.len();
        let truncated = total > limit_per_rel as usize;
        items.truncate(limit_per_rel as usize);
        out.push_str(&format!(
            "- **`-[:{rel}]->`** ({total}{})\n",
            if truncated {
                format!(", showing {limit_per_rel}")
            } else {
                String::new()
            }
        ));
        for (lbls, ident) in items {
            out.push_str(&format!("  - `{lbls}` `{ident}`\n"));
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
