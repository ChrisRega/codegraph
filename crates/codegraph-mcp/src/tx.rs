//! Buffered transaction state for the MCP `begin` / `write` / `commit` /
//! `rollback` tools. The buffer lives entirely in this module so it can
//! evolve (e.g. to a Vec<(String, Origin)> for richer auditing) without
//! touching the dispatcher.
//!
//! Each `begin` gets a monotonically increasing `tx_id` and an `opened_at`
//! timestamp. The triple (id, age, pending count) is exposed via
//! [`TxState::info`] so `index_status` can surface stuck transactions —
//! the prime suspect for the WAL-bloat / 20 GB DB eskalations we hit
//! during long MCP sessions. Lifecycle events (begin / commit / rollback)
//! also write a one-liner to stderr with the elapsed time so the bug is
//! noticeable in logs even without polling.

use std::time::Instant;

use codegraph_core::Db;
use serde_json::Value;

use crate::util::{err_text, ok_text};

pub struct TxState {
    pub active: bool,
    pub message: Option<String>,
    pub pending: Vec<String>,
    pub tx_id: Option<u64>,
    pub opened_at: Option<Instant>,
    next_tx_id: u64,
}

/// Snapshot of an open transaction for telemetry surfaces like
/// `index_status`. Returned by [`TxState::info`]; `None` means no
/// transaction is currently open.
pub struct TxInfo {
    pub tx_id: u64,
    pub age_secs: u64,
    pub pending: usize,
    pub message: Option<String>,
}

impl TxState {
    pub fn new() -> Self {
        Self {
            active: false,
            message: None,
            pending: Vec::new(),
            tx_id: None,
            opened_at: None,
            next_tx_id: 1,
        }
    }

    /// Snapshot of the currently-open buffered transaction, if any.
    /// Used by `index_status` to surface stuck transactions.
    pub fn info(&self) -> Option<TxInfo> {
        let (id, started) = self.tx_id.zip(self.opened_at)?;
        Some(TxInfo {
            tx_id: id,
            age_secs: started.elapsed().as_secs(),
            pending: self.pending.len(),
            message: self.message.clone(),
        })
    }
}

pub fn handle_begin(tx: &mut TxState, params: &Value) -> Value {
    if tx.active {
        let age = tx
            .opened_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or_default();
        let id = tx.tx_id.unwrap_or(0);
        eprintln!(
            "[tx] WARNING begin while tx#{id} still open ({age}s, {} buffered) — possible leak",
            tx.pending.len()
        );
        return ok_text(format!(
            "transaction already open (tx#{id}, {age}s old, {} queries buffered)",
            tx.pending.len()
        ));
    }
    let id = tx.next_tx_id;
    tx.next_tx_id = tx.next_tx_id.saturating_add(1);
    tx.active = true;
    tx.tx_id = Some(id);
    tx.opened_at = Some(Instant::now());
    tx.message = params
        .get("message")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    tx.pending.clear();
    eprintln!("[tx] begin tx#{id}");
    ok_text(format!("transaction tx#{id} opened"))
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
    let id = tx.tx_id.unwrap_or(0);
    let age = tx
        .opened_at
        .map(|t| t.elapsed().as_secs())
        .unwrap_or_default();
    if tx.pending.is_empty() {
        tx.active = false;
        tx.tx_id = None;
        tx.opened_at = None;
        tx.message = None;
        eprintln!("[tx] commit tx#{id} (empty, {age}s open)");
        return ok_text(format!(
            "transaction tx#{id} committed (nothing to apply, {age}s open)"
        ));
    }

    let queries: Vec<String> = tx.pending.drain(..).collect();
    let n = queries.len();
    tx.active = false;
    tx.tx_id = None;
    tx.opened_at = None;
    let _msg = tx.message.take();

    // Replay inside a velr transaction so failures roll back the batch.
    let velr = db.velr();
    let velr_tx = match velr.begin_tx() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[tx] commit tx#{id} FAILED at velr begin_tx: {e}");
            return err_text(format!("could not begin velr transaction: {e}"));
        }
    };

    for (i, q) in queries.iter().enumerate() {
        if let Err(e) = velr_tx.run(q) {
            eprintln!("[tx] commit tx#{id} FAILED on query #{}", i + 1);
            return err_text(format!(
                "query #{} failed; transaction rolled back:\n  {q}\nError: {e}",
                i + 1
            ));
        }
    }

    if let Err(e) = velr_tx.commit() {
        eprintln!("[tx] commit tx#{id} FAILED at velr commit: {e}");
        return err_text(format!("commit failed: {e}"));
    }
    eprintln!("[tx] commit tx#{id} ({n} queries, {age}s open)");
    ok_text(format!("committed {n} queries (tx#{id}, {age}s open)"))
}

pub fn handle_rollback(tx: &mut TxState) -> Value {
    if !tx.active {
        return err_text("no open transaction".to_string());
    }
    let id = tx.tx_id.unwrap_or(0);
    let age = tx
        .opened_at
        .map(|t| t.elapsed().as_secs())
        .unwrap_or_default();
    let n = tx.pending.len();
    tx.active = false;
    tx.tx_id = None;
    tx.opened_at = None;
    tx.message = None;
    tx.pending.clear();
    eprintln!("[tx] rollback tx#{id} ({n} discarded, {age}s open)");
    ok_text(format!(
        "rolled back tx#{id} ({n} buffered queries discarded, {age}s open, nothing was written)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tx_ids_increment_per_begin() {
        let mut tx = TxState::new();
        handle_begin(&mut tx, &json!({}));
        assert_eq!(tx.tx_id, Some(1));
        handle_rollback(&mut tx);

        handle_begin(&mut tx, &json!({}));
        assert_eq!(tx.tx_id, Some(2));
    }

    #[test]
    fn info_none_when_no_open_tx() {
        let tx = TxState::new();
        assert!(tx.info().is_none());
    }

    #[test]
    fn info_reports_pending_count_and_id() {
        let db = Db::open_in_memory().unwrap();
        let mut tx = TxState::new();
        handle_begin(&mut tx, &json!({"message": "test"}));
        handle_write(&db, &mut tx, &json!({"query": "CREATE (:T {id: 'a'})"}));
        handle_write(&db, &mut tx, &json!({"query": "CREATE (:T {id: 'b'})"}));
        let info = tx.info().expect("expected open tx");
        assert_eq!(info.tx_id, 1);
        assert_eq!(info.pending, 2);
        assert_eq!(info.message.as_deref(), Some("test"));
    }

    #[test]
    fn commit_clears_telemetry() {
        let db = Db::open_in_memory().unwrap();
        let mut tx = TxState::new();
        handle_begin(&mut tx, &json!({}));
        handle_write(&db, &mut tx, &json!({"query": "CREATE (:T {id: 'a'})"}));
        handle_commit(&db, &mut tx);
        assert!(tx.info().is_none());
        assert!(!tx.active);
        assert!(tx.opened_at.is_none());
        assert!(tx.tx_id.is_none());
    }

    #[test]
    fn rollback_clears_telemetry() {
        let mut tx = TxState::new();
        handle_begin(&mut tx, &json!({}));
        handle_rollback(&mut tx);
        assert!(tx.info().is_none());
        assert!(!tx.active);
    }
}
