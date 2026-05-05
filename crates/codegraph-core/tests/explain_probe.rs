//! Probe how velr exposes EXPLAIN. The MCP `explain` tool routes through
//! whichever shape works.

use codegraph_core::Db;

#[test]
fn explain_via_cypher_keyword() {
    let db = Db::open_in_memory().unwrap();
    db.run("CREATE (:Item {n: 1})").unwrap();
    // Some openCypher engines accept `EXPLAIN <query>` and return a plan as
    // a regular row table. If velr does, the MCP tool can use this path.
    let tables = db
        .query_many("EXPLAIN MATCH (n:Item) RETURN n")
        .expect("EXPLAIN <query> via exec()");
    assert!(!tables.is_empty(), "EXPLAIN produced no tables");
    let any_payload = tables
        .iter()
        .any(|t| !t.columns.is_empty() || !t.rows.is_empty());
    assert!(
        any_payload,
        "EXPLAIN returned only empty tables: {tables:?}"
    );
}
