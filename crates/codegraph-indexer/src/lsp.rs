//! LSP client — spawns a language server and queries it for code intelligence.
//!
//! Supports any LSP-compliant server (rust-analyzer, typescript-language-server, clangd).
//! Communication is JSON-RPC 2.0 over stdio with Content-Length framing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;

use lsp_types::*;
use serde_json::{json, Value};

/// Convert a filesystem path to an LSP Uri.
fn file_uri(path: &Path) -> Result<Uri, String> {
    let abs = path
        .canonicalize()
        .map_err(|e| format!("canonicalize: {e}"))?;
    let s = format!("file://{}", abs.display());
    s.parse::<Uri>().map_err(|e| format!("invalid uri: {e}"))
}

/// An active connection to a language server process.
///
/// Uses a background reader thread to drain stdout — prevents pipe deadlock
/// when the server sends many notifications during initialization.
pub struct LspClient {
    process: Child,
    /// Incoming messages from the background reader thread.
    receiver: mpsc::Receiver<Value>,
    next_id: AtomicI64,
    /// Pending responses keyed by request ID.
    pending: HashMap<i64, Value>,
}

impl LspClient {
    /// Spawn a language server and perform the LSP initialize handshake.
    ///
    /// `command` is the server binary (e.g. "rust-analyzer", "typescript-language-server").
    /// `args` are extra CLI arguments (e.g. ["--stdio"]).
    /// `workspace` is the root directory of the project.
    pub fn start(command: &str, args: &[&str], workspace: &Path) -> Result<Self, String> {
        let mut process = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .current_dir(workspace)
            .spawn()
            .map_err(|e| format!("Failed to spawn {command}: {e}"))?;

        let root_uri = file_uri(workspace)?;
        let stdout = process
            .stdout
            .take()
            .ok_or("Failed to take stdout from LSP process")?;

        // Spawn background reader thread to prevent pipe deadlock.
        // RA sends many notifications during init; if nobody reads stdout,
        // the pipe fills up and RA blocks, deadlocking our stdin writes.
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut header_line = String::new();
                match reader.read_line(&mut header_line) {
                    Ok(0) | Err(_) => break, // EOF or error
                    Ok(_) => {}
                }
                let Some(len) = header_line
                    .strip_prefix("Content-Length:")
                    .and_then(|s| s.trim().parse::<usize>().ok())
                else {
                    continue; // skip non-header lines (e.g. Content-Type, blank)
                };
                // Read blank line after headers
                let mut blank = String::new();
                let _ = reader.read_line(&mut blank);
                // Read body
                let mut body = vec![0u8; len];
                if reader.read_exact(&mut body).is_err() {
                    break;
                }
                if let Ok(msg) = serde_json::from_slice::<Value>(&body) {
                    if sender.send(msg).is_err() {
                        break;
                    }
                }
            }
        });

        let mut client = Self {
            process,
            receiver,
            next_id: AtomicI64::new(1),
            pending: HashMap::new(),
        };

        // Send initialize request
        #[allow(deprecated)] // root_uri is the compatible field across LSPs
        let init_params = InitializeParams {
            root_uri: Some(root_uri.clone()),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    document_symbol: Some(DocumentSymbolClientCapabilities {
                        hierarchical_document_symbol_support: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let _result: InitializeResult = client.request("initialize", init_params)?;
        client.notify("initialized", InitializedParams {})?;

        eprintln!("  [*] LSP server started: {command}");
        Ok(client)
    }

    /// Send a JSON-RPC request and wait for the response.
    pub fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<R, String> {
        let id = self.send_request(method, params)?;
        self.await_response(id, method)
    }

    /// Send a JSON-RPC request without waiting — returns the request id
    /// so the caller can collect the response later via `await_response`.
    /// Use with `await_response` to pipeline many requests in parallel,
    /// turning N sequential round-trips into 1 batched send + 1 batched
    /// receive.
    pub fn send_request<P: serde::Serialize>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<i64, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send_message(&msg)?;
        Ok(id)
    }

    /// Wait for the response to a previously-sent `send_request` and
    /// deserialize it into `R`. `method` is used only for error messages.
    pub fn await_response<R: serde::de::DeserializeOwned>(
        &mut self,
        id: i64,
        method: &str,
    ) -> Result<R, String> {
        let response = self.read_response(id)?;
        if let Some(error) = response.get("error") {
            return Err(format!("LSP error: {}", error));
        }
        let result = response.get("result").cloned().unwrap_or(Value::Null);
        serde_json::from_value(result)
            .map_err(|e| format!("Failed to deserialize {method} response: {e}"))
    }

    /// Send a JSON-RPC notification (no response expected).
    pub fn notify<P: serde::Serialize>(&mut self, method: &str, params: P) -> Result<(), String> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_message(&msg)
    }

    /// Tell the server we opened a file (required before querying it).
    pub fn open_file(&mut self, path: &Path) -> Result<(), String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let uri = file_uri(path)?;

        let lang = match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => "rust",
            Some("ts") | Some("tsx") => "typescript",
            Some("js") | Some("jsx") => "javascript",
            Some("py") => "python",
            Some("c") | Some("h") => "c",
            Some("cpp") | Some("cc") | Some("hpp") => "cpp",
            _ => "plaintext",
        };

        self.notify(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: lang.to_string(),
                    version: 0,
                    text: content,
                },
            },
        )
    }

    /// Tell the server a file we previously opened has changed on disk.
    /// Whole-document replace at the given `version`. Use this in
    /// persistent-pool mode instead of a duplicate `didOpen`.
    pub fn change_file(&mut self, path: &Path, version: i32) -> Result<(), String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let uri = file_uri(path)?;
        self.notify(
            "textDocument/didChange",
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri, version },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: content,
                }],
            },
        )
    }

    /// Get all symbols in a file.
    pub fn document_symbols(&mut self, path: &Path) -> Result<Vec<DocumentSymbol>, String> {
        let uri = file_uri(path)?;

        let result: Option<DocumentSymbolResponse> = self.request(
            "textDocument/documentSymbol",
            DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        )?;

        match result {
            Some(DocumentSymbolResponse::Nested(symbols)) => Ok(symbols),
            Some(DocumentSymbolResponse::Flat(infos)) => {
                // Convert SymbolInformation to DocumentSymbol (best-effort)
                #[allow(deprecated)] // DocumentSymbol.deprecated still required by struct layout
                let out = infos
                    .into_iter()
                    .map(|si| DocumentSymbol {
                        name: si.name,
                        detail: None,
                        kind: si.kind,
                        tags: si.tags,
                        deprecated: None,
                        range: si.location.range,
                        selection_range: si.location.range,
                        children: None,
                    })
                    .collect();
                Ok(out)
            }
            None => Ok(vec![]),
        }
    }

    /// Drain all pending notifications from the channel (non-blocking).
    #[allow(dead_code)] // public debug helper, not yet wired into main.
    pub fn drain_notifications(&mut self) {
        while let Ok(msg) = self.receiver.try_recv() {
            // Cache responses, discard notifications
            if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
                self.pending.insert(id, msg);
            }
        }
    }

    /// Wait for the LSP to go quiet — i.e. `silence_ms` of no incoming
    /// messages. Used after a bulk `didOpen` to give rust-analyzer time
    /// to actually finish indexing before we hit `documentSymbol` /
    /// `outgoingCalls`. Bounded by `max_ms` so a chatty server can't
    /// stall the indexer forever.
    ///
    /// Replaces the previous fixed `thread::sleep`. With a warm server
    /// this typically returns in ~1s; cold starts settle in 5–10s
    /// instead of the old fixed 15s.
    pub fn wait_until_idle(&mut self, silence_ms: u64, max_ms: u64) {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::{Duration, Instant};
        let silence = Duration::from_millis(silence_ms);
        let deadline = Instant::now() + Duration::from_millis(max_ms);
        let mut drained = 0u32;
        loop {
            if Instant::now() >= deadline {
                eprintln!("  [lsp] wait_until_idle: hit max_ms={max_ms} after {drained} messages");
                return;
            }
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .min(silence);
            match self.receiver.recv_timeout(remaining) {
                Ok(msg) => {
                    drained = drained.saturating_add(1);
                    // Stash response payloads — read_response will need them.
                    if msg.get("id").is_some() && msg.get("method").is_none() {
                        if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
                            self.pending.insert(id, msg);
                        }
                    } else if msg.get("id").is_some() {
                        // Server-initiated request (e.g. window/workDoneProgress/create) —
                        // must reply or rust-analyzer stalls.
                        let reply = json!({
                            "jsonrpc": "2.0",
                            "id": msg["id"],
                            "result": null
                        });
                        let _ = self.send_message(&reply);
                    }
                    // Other notifications drop on the floor.
                }
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!("  [lsp] wait_until_idle: settled after {drained} messages");
                    return;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Batched outgoing-calls lookup. Pipelines `prepareCallHierarchy`
    /// followed by `callHierarchy/outgoingCalls` in chunks of up to
    /// `MAX_INFLIGHT` requests so we cap rust-analyzer's concurrent
    /// queue but still hide most of the per-request round-trip
    /// latency.
    ///
    /// `positions` is `(abs_path, line, character)`. The returned vec
    /// preserves input order; entries where the prepare call failed,
    /// returned no items, or the subsequent outgoingCalls failed,
    /// become an empty `Vec`.
    pub fn outgoing_calls_batch(
        &mut self,
        positions: &[(std::path::PathBuf, u32, u32)],
    ) -> Vec<Vec<CallHierarchyOutgoingCall>> {
        // Cap concurrent in-flight requests. Firing 285 prepareCallHierarchy
        // at rust-analyzer in one burst empirically caused ~70% of CALLS to
        // come back missing. 32 keeps the pipelining win without overrunning
        // RA's internal request queue.
        const MAX_INFLIGHT: usize = 32;
        let mut out: Vec<Vec<CallHierarchyOutgoingCall>> = Vec::with_capacity(positions.len());
        for chunk in positions.chunks(MAX_INFLIGHT) {
            out.extend(self.outgoing_calls_chunk(chunk));
        }
        out
    }

    fn outgoing_calls_chunk(
        &mut self,
        positions: &[(std::path::PathBuf, u32, u32)],
    ) -> Vec<Vec<CallHierarchyOutgoingCall>> {
        if positions.is_empty() {
            return Vec::new();
        }
        // Phase 1: send every prepareCallHierarchy request, collect the
        // ids in input order.
        let mut prep_ids: Vec<Option<i64>> = Vec::with_capacity(positions.len());
        for (path, line, character) in positions {
            let uri = match file_uri(path) {
                Ok(u) => u,
                Err(_) => {
                    prep_ids.push(None);
                    continue;
                }
            };
            let id = self.send_request(
                "textDocument/prepareCallHierarchy",
                CallHierarchyPrepareParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri },
                        position: Position {
                            line: *line,
                            character: *character,
                        },
                    },
                    work_done_progress_params: Default::default(),
                },
            );
            prep_ids.push(id.ok());
        }
        // Phase 2: collect prepare responses + send outgoingCalls for each.
        let mut call_ids: Vec<Option<i64>> = Vec::with_capacity(positions.len());
        for prep_id in prep_ids {
            let Some(id) = prep_id else {
                call_ids.push(None);
                continue;
            };
            let items: Option<Vec<CallHierarchyItem>> =
                match self.await_response(id, "textDocument/prepareCallHierarchy") {
                    Ok(v) => v,
                    Err(_) => {
                        call_ids.push(None);
                        continue;
                    }
                };
            let item = match items.and_then(|v| v.into_iter().next()) {
                Some(it) => it,
                None => {
                    call_ids.push(None);
                    continue;
                }
            };
            let id = self.send_request(
                "callHierarchy/outgoingCalls",
                CallHierarchyOutgoingCallsParams {
                    item,
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                },
            );
            call_ids.push(id.ok());
        }
        // Phase 3: collect outgoingCalls responses.
        call_ids
            .into_iter()
            .map(|id| match id {
                Some(id) => match self.await_response::<Option<Vec<CallHierarchyOutgoingCall>>>(
                    id,
                    "callHierarchy/outgoingCalls",
                ) {
                    Ok(Some(v)) => v,
                    _ => Vec::new(),
                },
                None => Vec::new(),
            })
            .collect()
    }

    /// Batched `incomingCalls`: mirror of [`outgoing_calls_batch`] for
    /// the reverse direction. Used by the live indexer to repair the
    /// caller-side of the CALLS graph when a fresh function appears in
    /// a file that gets reparsed in isolation (the file that *calls*
    /// it may not be in the same batch, so its outgoing pass never
    /// runs — incomingCalls bridges that).
    pub fn incoming_calls_batch(
        &mut self,
        positions: &[(std::path::PathBuf, u32, u32)],
    ) -> Vec<Vec<CallHierarchyIncomingCall>> {
        const MAX_INFLIGHT: usize = 32;
        let mut out: Vec<Vec<CallHierarchyIncomingCall>> = Vec::with_capacity(positions.len());
        for chunk in positions.chunks(MAX_INFLIGHT) {
            out.extend(self.incoming_calls_chunk(chunk));
        }
        out
    }

    fn incoming_calls_chunk(
        &mut self,
        positions: &[(std::path::PathBuf, u32, u32)],
    ) -> Vec<Vec<CallHierarchyIncomingCall>> {
        if positions.is_empty() {
            return Vec::new();
        }
        let mut prep_ids: Vec<Option<i64>> = Vec::with_capacity(positions.len());
        for (path, line, character) in positions {
            let uri = match file_uri(path) {
                Ok(u) => u,
                Err(_) => {
                    prep_ids.push(None);
                    continue;
                }
            };
            let id = self.send_request(
                "textDocument/prepareCallHierarchy",
                CallHierarchyPrepareParams {
                    text_document_position_params: TextDocumentPositionParams {
                        text_document: TextDocumentIdentifier { uri },
                        position: Position {
                            line: *line,
                            character: *character,
                        },
                    },
                    work_done_progress_params: Default::default(),
                },
            );
            prep_ids.push(id.ok());
        }
        let mut call_ids: Vec<Option<i64>> = Vec::with_capacity(positions.len());
        for prep_id in prep_ids {
            let Some(id) = prep_id else {
                call_ids.push(None);
                continue;
            };
            let items: Option<Vec<CallHierarchyItem>> =
                match self.await_response(id, "textDocument/prepareCallHierarchy") {
                    Ok(v) => v,
                    Err(_) => {
                        call_ids.push(None);
                        continue;
                    }
                };
            let item = match items.and_then(|v| v.into_iter().next()) {
                Some(it) => it,
                None => {
                    call_ids.push(None);
                    continue;
                }
            };
            let id = self.send_request(
                "callHierarchy/incomingCalls",
                CallHierarchyIncomingCallsParams {
                    item,
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                },
            );
            call_ids.push(id.ok());
        }
        call_ids
            .into_iter()
            .map(|id| match id {
                Some(id) => match self.await_response::<Option<Vec<CallHierarchyIncomingCall>>>(
                    id,
                    "callHierarchy/incomingCalls",
                ) {
                    Ok(Some(v)) => v,
                    _ => Vec::new(),
                },
                None => Vec::new(),
            })
            .collect()
    }

    /// Get outgoing calls from a function at a specific position.
    pub fn outgoing_calls(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<CallHierarchyOutgoingCall>, String> {
        let uri = file_uri(path)?;

        // Step 1: prepare call hierarchy at position
        let items: Option<Vec<CallHierarchyItem>> = self.request(
            "textDocument/prepareCallHierarchy",
            CallHierarchyPrepareParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position { line, character },
                },
                work_done_progress_params: Default::default(),
            },
        )?;

        let Some(items) = items else {
            return Ok(vec![]);
        };
        let Some(item) = items.into_iter().next() else {
            return Ok(vec![]);
        };

        // Step 2: get outgoing calls
        let calls: Option<Vec<CallHierarchyOutgoingCall>> = self.request(
            "callHierarchy/outgoingCalls",
            CallHierarchyOutgoingCallsParams {
                item,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        )?;

        Ok(calls.unwrap_or_default())
    }

    /// Find implementations of a symbol (trait/interface).
    #[allow(dead_code)] // reserved for future call-graph extensions.
    pub fn find_implementations(
        &mut self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>, String> {
        let uri = file_uri(path)?;

        let result: Option<GotoDefinitionResponse> = self.request(
            "textDocument/implementation",
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position { line, character },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        )?;

        match result {
            Some(GotoDefinitionResponse::Scalar(loc)) => Ok(vec![loc]),
            Some(GotoDefinitionResponse::Array(locs)) => Ok(locs),
            Some(GotoDefinitionResponse::Link(_)) => Ok(vec![]),
            None => Ok(vec![]),
        }
    }

    /// Shut down the language server cleanly.
    pub fn shutdown(mut self) -> Result<(), String> {
        let _: Value = self.request("shutdown", Value::Null)?;
        self.notify("exit", Value::Null)?;
        let _ = self.process.wait();
        Ok(())
    }

    // ── Transport ────────────────────────────────────────────────────────────

    fn send_message(&mut self, msg: &Value) -> Result<(), String> {
        let body = serde_json::to_string(msg).map_err(|e| format!("JSON serialize error: {e}"))?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        let stdin = self
            .process
            .stdin
            .as_mut()
            .ok_or("LSP process stdin closed")?;
        stdin
            .write_all(header.as_bytes())
            .map_err(|e| format!("Write header failed: {e}"))?;
        stdin
            .write_all(body.as_bytes())
            .map_err(|e| format!("Write body failed: {e}"))?;
        stdin.flush().map_err(|e| format!("Flush failed: {e}"))?;
        Ok(())
    }

    fn read_response(&mut self, expected_id: i64) -> Result<Value, String> {
        // Check if we already have this response cached (from reading past notifications)
        if let Some(resp) = self.pending.remove(&expected_id) {
            return Ok(resp);
        }

        // Read messages from the background thread until we find our response.
        loop {
            let msg = self
                .receiver
                .recv()
                .map_err(|_| "LSP reader thread closed".to_string())?;

            // Three message shapes per JSON-RPC 2.0:
            //   - response:        has `id`, no `method`        → match against expected, cache otherwise
            //   - server request:  has `id` AND `method`        → MUST reply with `result: null`
            //                                                     or rust-analyzer stalls
            //   - notification:    has `method`, no `id`        → drop on the floor
            let has_method = msg.get("method").is_some();
            let id_i64 = msg.get("id").and_then(|v| v.as_i64());
            match (id_i64, has_method) {
                (Some(id), false) => {
                    // Response.
                    if id == expected_id {
                        return Ok(msg);
                    }
                    self.pending.insert(id, msg);
                }
                (Some(_), true) => {
                    // Server-initiated request — e.g. window/workDoneProgress/create.
                    // Replying with null is the LSP-conformant ack: we accept the
                    // token / sample / etc. but don't pretend to do anything with it.
                    // Without this reply rust-analyzer stalls progress trackers and
                    // (worse) starts returning empty results for client requests
                    // that ride on the same progress token. Discovered while
                    // pipelining 285 outgoingCalls in parallel: 64% of CALLS edges
                    // went missing until this reply landed.
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": msg["id"],
                        "result": null,
                    });
                    let _ = self.send_message(&reply);
                }
                (None, _) => {
                    // Notification: log and drop.
                    if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
                        eprintln!("  [lsp] {method}");
                    }
                }
            }
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

// ── Helper: convert LSP SymbolKind to our schema kind string ─────────────

pub fn symbol_kind_str(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::STRUCT => "Struct",
        SymbolKind::ENUM => "Enum",
        SymbolKind::INTERFACE => "Trait",
        SymbolKind::TYPE_PARAMETER => "TypeAlias",
        SymbolKind::CONSTANT => "Const",
        SymbolKind::FUNCTION => "Function",
        SymbolKind::METHOD => "Method",
        SymbolKind::CONSTRUCTOR => "Method",
        SymbolKind::FIELD => "Field",
        SymbolKind::ENUM_MEMBER => "EnumVariant",
        SymbolKind::MODULE => "Module",
        SymbolKind::VARIABLE => "Static",
        _ => "Other",
    }
}

/// Convert an LSP Position to a 1-based line number.
pub fn line_1based(pos: &Position) -> u32 {
    pos.line + 1
}

// ── Persistent LSP pool ──────────────────────────────────────────────────────

use std::collections::HashSet;
use std::path::PathBuf;

/// One entry in [`LspPool`]: a live `LspClient` plus the set of files we've
/// already sent `textDocument/didOpen` for. Subsequent passes use that map
/// to choose between `didChange` (file already known to the LSP) and a
/// fresh `didOpen` (newly touched).
pub struct PooledClient {
    pub client: LspClient,
    pub workspace: PathBuf,
    /// path → next version number to send with `didChange`. The first
    /// `didOpen` is at version 0; the first `didChange` after that uses 1.
    pub opened: HashMap<PathBuf, i32>,
    /// True after the first `index_files_via_lsp` pass — used to skip the
    /// 15s warm-up sleep, since the LSP has already crunched through the
    /// workspace. New per-batch files still get a short settle wait.
    pub warmed_up: bool,
}

/// A pool of live `LspClient`s keyed by command (e.g. `"rust-analyzer"`,
/// `"typescript-language-server"`, `"pyright-langserver"`). Owned by the
/// MCP server's watcher thread so the cold-start cost is paid exactly
/// once per language server, regardless of how many file-change batches
/// arrive.
#[derive(Default)]
pub struct LspPool {
    clients: HashMap<String, PooledClient>,
    /// Commands we've tried to start and failed on. Avoids retrying on
    /// every batch when (e.g.) `rust-analyzer` isn't installed.
    failed: HashSet<String>,
}

impl LspPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a mutable reference to the pooled client for `command`, starting
    /// a fresh process if there isn't one yet. Returns `Err` (and remembers
    /// the failure) if the LSP could not be spawned.
    pub fn get_or_start(
        &mut self,
        command: &str,
        args: &[&str],
        workspace: &Path,
    ) -> Result<&mut PooledClient, String> {
        if self.failed.contains(command) {
            return Err(format!(
                "LSP `{command}` previously failed to start; not retrying"
            ));
        }
        if !self.clients.contains_key(command) {
            match LspClient::start(command, args, workspace) {
                Ok(client) => {
                    self.clients.insert(
                        command.to_string(),
                        PooledClient {
                            client,
                            workspace: workspace.to_path_buf(),
                            opened: HashMap::new(),
                            warmed_up: false,
                        },
                    );
                }
                Err(e) => {
                    self.failed.insert(command.to_string());
                    return Err(e);
                }
            }
        }
        Ok(self.clients.get_mut(command).expect("just inserted above"))
    }

    /// Number of live LSP processes currently in the pool.
    pub fn live_count(&self) -> usize {
        self.clients.len()
    }

    /// List of commands currently pooled, for diagnostics.
    pub fn live_commands(&self) -> Vec<String> {
        let mut v: Vec<String> = self.clients.keys().cloned().collect();
        v.sort();
        v
    }

    /// Send `shutdown` + `exit` to every pooled client. Best-effort: errors
    /// are logged to stderr but do not abort.
    pub fn shutdown_all(&mut self) {
        for (cmd, pc) in std::mem::take(&mut self.clients) {
            if let Err(e) = pc.client.shutdown() {
                eprintln!("[lsp-pool] {cmd} shutdown: {e}");
            }
        }
    }
}

impl Drop for LspPool {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}
