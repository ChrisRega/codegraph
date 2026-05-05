//! Sidecar metadata persisted next to the velr database to enable
//! incremental re-indexing. velr has no built-in versioning, so we record
//! the last-indexed git commit in a small JSON file alongside the DB.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Meta {
    pub last_commit: String,
    pub indexed_at: String,
}

pub fn sidecar_path(db_path: &str) -> PathBuf {
    PathBuf::from(format!("{db_path}.codegraph-meta.json"))
}

pub fn load(path: &Path) -> Option<Meta> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save(path: &Path, meta: &Meta) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(meta).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}
