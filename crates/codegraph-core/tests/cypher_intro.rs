//! Probe what velr 0.2.x actually supports for graph introspection.
//! These tests double as documentation — if they break, the schema tool
//! in `codegraph-mcp` needs to follow.

use codegraph_core::Db;

fn seed(db: &Db) {
    db.run("CREATE (:Person {name: 'a'})-[:KNOWS]->(:Person {name: 'b'})")
        .unwrap();
    db.run("CREATE (:Robot {name: 'r'})").unwrap();
}

#[test]
fn labels_function_returns_each_node_label() {
    let db = Db::open_in_memory().unwrap();
    seed(&db);
    let t = db
        .query("MATCH (n) RETURN labels(n) AS lbls")
        .expect("labels(n) must work for the schema tool");
    assert_eq!(t.rows.len(), 3);
}

#[test]
fn type_function_returns_edge_type() {
    let db = Db::open_in_memory().unwrap();
    seed(&db);
    let t = db
        .query("MATCH ()-[r]->() RETURN type(r) AS t")
        .expect("type(r) must work for the schema tool");
    assert_eq!(t.rows.len(), 1);
    let cell = &t.rows[0][0];
    assert_eq!(cell.as_str(), Some("KNOWS"));
}

#[test]
fn distinct_label_query_used_by_schema_tool() {
    let db = Db::open_in_memory().unwrap();
    seed(&db);
    let t = db
        .query("MATCH (n) RETURN DISTINCT labels(n) AS lbls")
        .unwrap();
    assert!(
        t.rows.len() >= 2,
        "expected at least Person and Robot in distinct labels, got {} rows",
        t.rows.len()
    );
}
