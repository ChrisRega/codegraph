//! Buffered transaction state for the MCP `begin` / `write` / `commit` /
//! `rollback` tools. The buffer lives entirely in this module so it can
//! evolve (e.g. to a Vec<(String, Origin)> for richer auditing) without
//! touching the dispatcher.

use codegraph_core::Db;
use serde_json::Value;

use crate::util::{err_text, ok_text};

pub struct TxState {
    pub active: bool,
    pub message: Option<String>,
    pub pending: Vec<String>,
}

impl TxState {
    pub fn new() -> Self {
        Self {
            active: false,
            message: None,
            pending: Vec::new(),
        }
    }
}

pub fn handle_begin(tx: &mut TxState, params: &Value) -> Value {
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

pub fn handle_write(db: &Db, tx: &mut TxState, params: &Value) -> Value {
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

pub fn handle_commit(db: &Db, tx: &mut TxState) -> Value {
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

pub fn handle_rollback(tx: &mut TxState) -> Value {
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
