//! codegraph MCP server — exposes a velr-backed graph database as Claude tools.
//!
//! Speaks the Model Context Protocol (JSON-RPC 2.0 over stdio).
//!
//! Tools:
//!   • `schema()`           — list vertex labels and edge types (sampled)
//!   • `cypher(query)`      — run an openCypher read query
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

use codegraph_core::{Cell, Db, Table};
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
            "schema", "cypher", "begin", "write", "commit", "rollback", "explain",
        ] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
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
