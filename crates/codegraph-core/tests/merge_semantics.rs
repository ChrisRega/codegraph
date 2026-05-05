//! Verify MERGE idempotency in velr 0.2.x. The indexer assumes
//! `MERGE (:Label {prop: val})` produces exactly one node per distinct
//! `(label, prop=val)` tuple regardless of how many times it runs. If
//! velr ever diverges, the workspace / package / git-commit upserts will
//! double up and this test fails first.

use codegraph_core::Db;

#[test]
fn merge_is_idempotent_for_match_or_create() {
    let db = Db::open_in_memory().unwrap();
    for _ in 0..5 {
        db.run("MERGE (:Workspace {name: 'codegraph'})").unwrap();
    }
    let t = db
        .query("MATCH (w:Workspace {name: 'codegraph'}) RETURN count(w) AS c")
        .unwrap();
    let count = t.rows[0][0].as_i64().expect("count(w)");
    assert_eq!(count, 1, "MERGE produced {count} nodes, expected 1");
}

#[test]
fn merge_then_set_updates_in_place() {
    let db = Db::open_in_memory().unwrap();
    db.run("MERGE (:Pkg {name: 'foo'}) SET name='foo'").err(); // best-effort syntax probe
    db.run("MERGE (p:Pkg {name: 'foo'}) SET p.version = '0.1.0'")
        .unwrap();
    db.run("MERGE (p:Pkg {name: 'foo'}) SET p.version = '0.2.0'")
        .unwrap();
    let t = db
        .query("MATCH (p:Pkg {name: 'foo'}) RETURN p.version AS v, count(p) AS c")
        .unwrap();
    assert_eq!(t.rows.len(), 1);
    assert_eq!(t.rows[0][0].as_str(), Some("0.2.0"));
    assert_eq!(t.rows[0][1].as_i64(), Some(1));
}
