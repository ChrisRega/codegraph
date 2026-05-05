//! End-to-end smoke tests: use codegraph-core to write & query an
//! in-memory velr database. These will catch regressions either on the
//! velr side (API breakage) or in our adapter / escaper.

use codegraph_core::{escape_str, Cell, Db};

#[test]
fn open_in_memory_runs_and_queries() {
    let db = Db::open_in_memory().expect("open in-memory");
    db.run("CREATE (:Item {name: 'alpha', count: 1})")
        .expect("create");
    let table = db
        .query("MATCH (i:Item) RETURN i.name AS n, i.count AS c")
        .expect("query");
    assert_eq!(table.columns, vec!["n".to_string(), "c".to_string()]);
    assert_eq!(table.rows.len(), 1);
    assert_eq!(table.rows[0][0].as_str(), Some("alpha"));
    assert_eq!(table.rows[0][1].as_i64(), Some(1));
}

#[test]
fn escape_str_survives_roundtrip_for_tricky_inputs() {
    let db = Db::open_in_memory().expect("open in-memory");
    let inputs = [
        "plain ascii",
        "with 'single' quote",
        "with \"double\" quote",
        "back\\slash",
        "newline\n inside",
        "tab\t and a quote'",
        "emoji 🚀 and unicode é",
    ];
    for (i, raw) in inputs.iter().enumerate() {
        let cypher = format!("CREATE (:Sample {{idx: {i}, value: {}}})", escape_str(raw));
        db.run(&cypher)
            .unwrap_or_else(|e| panic!("create #{i} failed: {e}\n  {cypher}"));
    }
    let t = db
        .query("MATCH (s:Sample) RETURN s.idx AS idx, s.value AS v ORDER BY s.idx")
        .expect("query");
    assert_eq!(t.rows.len(), inputs.len());
    for (row, raw) in t.rows.iter().zip(inputs.iter()) {
        let v = row.get(1).unwrap();
        assert_eq!(
            v.as_str(),
            Some(*raw),
            "round-trip mismatch for input {raw:?} (got {v:?})"
        );
    }
}

#[test]
fn cell_helpers() {
    let s = Cell::Text("foo".into());
    assert_eq!(s.as_str(), Some("foo"));
    assert!(s.as_i64().is_none());
    assert!(s.as_bool().is_none());
    assert!(!s.is_null());

    let n = Cell::Integer(42);
    assert_eq!(n.as_i64(), Some(42));
    assert_eq!(n.as_f64(), Some(42.0));

    let null = Cell::Null;
    assert!(null.is_null());
    assert!(null.as_str().is_none());
}
