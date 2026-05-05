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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_appends_suffix() {
        let p = sidecar_path("/tmp/foo.db");
        assert_eq!(p, PathBuf::from("/tmp/foo.db.codegraph-meta.json"));
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = std::env::temp_dir().join("codegraph-meta-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meta.json");
        let _ = std::fs::remove_file(&path);

        let m = Meta {
            last_commit: "deadbeef".into(),
            indexed_at: "2026-05-05T12:00:00Z".into(),
        };
        save(&path, &m).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded.last_commit, "deadbeef");
        assert_eq!(loaded.indexed_at, "2026-05-05T12:00:00Z");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_returns_none() {
        let bogus = PathBuf::from("/nonexistent/codegraph-meta-test-no-such-path");
        assert!(load(&bogus).is_none());
    }
}
