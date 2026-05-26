# velr bug report — `MATCH ... DELETE r` with unanchored endpoints silently skips rows

**velr version:** 0.2.16
**driver:** `velr` crate (Rust) 0.2.16, default features
**OS:** Linux 7.0.7-arch1-1, x86_64
**rustc:** 1.95.0 (59807616e 2026-04-14)
**date:** 2026-05-19

## Summary

`MATCH ()-[r:R]->() DELETE r` (or any `MATCH` clause where at least
one endpoint of the relationship is anonymous) deletes only a subset
of the matching relationships in a single execution, instead of all
of them as the openCypher spec requires. A subsequent identical query
deletes some of the remaining ones, and so on. With *N* matching
edges, you need on the order of `log₂(N)+1` repeated invocations to
drain them all.

Anchoring both endpoints to bound variables (`MATCH (a)-[r:R]->(b)
DELETE r`) deletes all matching rows in one pass, as expected.

The smell suggests velr's executor is iterating over the match
bindings lazily and mutating storage in the same pass, which
invalidates its own cursor.

## Minimal reproducer

Three vertices, three relationships:

```cypher
CREATE (a:T {id: 'a'}) CREATE (b:T {id: 'b'}) CREATE (c:T {id: 'c'})
CREATE (a)-[:R]->(b) CREATE (a)-[:R]->(c) CREATE (b)-[:R]->(c)
```

Count check:

```cypher
MATCH ()-[r:R]->() RETURN count(r);   -- returns 3
```

Trigger:

```cypher
MATCH ()-[r:R]->() DELETE r;
MATCH ()-[r:R]->() RETURN count(r);   -- returns 1   (expected: 0)

MATCH ()-[r:R]->() DELETE r;
MATCH ()-[r:R]->() RETURN count(r);   -- returns 0   (the second pass catches up)
```

Same bug with a half-anchored pattern (left endpoint bound, right
endpoint anonymous):

```cypher
MATCH (a)-[r:R]->() DELETE r;
MATCH ()-[r:R]->() RETURN count(r);   -- returns 1   (expected: 0)
```

Fully anchored pattern is correct:

```cypher
MATCH (a)-[r:R]->(b) DELETE r;
MATCH ()-[r:R]->() RETURN count(r);   -- returns 0   (correct)
```

## Workarounds

Either of these forces the bindings to materialise before the DELETE
fires and is correct in a single pass:

```cypher
-- Materialise the variable through WITH:
MATCH ()-[r:R]->() WITH r DELETE r;

-- Or collect-then-unwind:
MATCH ()-[r:R]->() WITH collect(r) AS rs UNWIND rs AS r DELETE r;
```

Both bring `count(r)` to 0 from the same 3-edge starting state in a
single execution.

## Expected behaviour (spec)

openCypher `DELETE r` removes the relationship for every binding of
`r` produced by the preceding `MATCH`, regardless of whether the
endpoint nodes are themselves bound, named or anonymous. The pattern
`MATCH ()-[r:R]->()` is equivalent in binding semantics to
`MATCH (a)-[r:R]->(b)` minus the named-variable exposure of `a`/`b`.

## Why this matters in practice

The downstream caller (a code-graph indexer that periodically rebuilds
derived relationship sets between phases) was originally using
unanchored wipes to clear edge sets:

```cypher
MATCH ()-[r:TESTS]->() DELETE r;
-- then rebuild
```

This silently no-oped on most matching rows, leading to per-pass edge
accumulation that took weeks to spot in production (the consequence
was wildly inflated `node_md` neighbour rankings on heavily-tested
functions). Switching to the fully-anchored form fixed it, but the
workaround required noticing the bug, and the diagnostic is non-local
— the symptom is "neighbour counts are wrong six months later", not
"delete failed".

## Reproducer script

A standalone reproducer using the published `velr` 0.2.16 crate is
attached below. It opens an in-memory DB, runs the three executions
above, and asserts the buggy counts.

```rust
use velr::Db;

fn main() {
    let db = Db::open_in_memory().expect("open");
    db.run("CREATE (a:T {id: 'a'}) CREATE (b:T {id: 'b'}) CREATE (c:T {id: 'c'}) \
            CREATE (a)-[:R]->(b) CREATE (a)-[:R]->(c) CREATE (b)-[:R]->(c)")
        .unwrap();

    let count = |label: &str| {
        let t = db.query("MATCH ()-[r:R]->() RETURN count(r) AS c").unwrap();
        let n = t.rows[0][0].as_i64().unwrap();
        println!("[{label}] count = {n}");
        n
    };

    assert_eq!(count("initial"), 3);

    // BUG: unanchored DELETE skips rows.
    db.run("MATCH ()-[r:R]->() DELETE r").ok();
    let after_unanchored = count("after unanchored DELETE pass 1");
    assert!(after_unanchored > 0, "unanchored DELETE should be buggy");

    // Drain takes multiple passes.
    while count("draining") > 0 {
        db.run("MATCH ()-[r:R]->() DELETE r").ok();
    }

    // Re-seed for the anchored test.
    db.run("MATCH (n:T) DETACH DELETE n").ok();
    db.run("CREATE (a:T {id: 'a'}) CREATE (b:T {id: 'b'}) CREATE (c:T {id: 'c'}) \
            CREATE (a)-[:R]->(b) CREATE (a)-[:R]->(c) CREATE (b)-[:R]->(c)")
        .unwrap();
    assert_eq!(count("re-seed"), 3);

    // Fully anchored: correct in one pass.
    db.run("MATCH (a)-[r:R]->(b) DELETE r").ok();
    assert_eq!(count("after anchored DELETE"), 0);

    // Re-seed and verify the WITH workaround.
    db.run("CREATE (a)-[:R]->(b) CREATE (a)-[:R]->(c) CREATE (b)-[:R]->(c)").ok();
    assert_eq!(count("re-seed via re-create"), 3);
    db.run("MATCH ()-[r:R]->() WITH r DELETE r").ok();
    assert_eq!(count("after WITH r DELETE r"), 0);

    println!("\nReproducer confirms: unanchored MATCH-DELETE is buggy, \
              fully-anchored and WITH-materialised variants are correct.");
}
```

`Cargo.toml`:

```toml
[package]
name = "velr-bug-0001-unanchored-delete"
version = "0.1.0"
edition = "2021"

[dependencies]
velr = "0.2.16"
```

## Possible root cause

The two correct paths both force the relationship bindings to
materialise (an `(a, b)` tuple pinned to vertex addresses, or a
`WITH`-projected `r` variable) before `DELETE` runs. The two broken
paths leave the relationship cursor as the live iterator backing the
`MATCH`. Deleting the edge under the cursor likely advances or
invalidates the iterator inconsistently, so the next `next()` returns
a row past the deleted one and earlier rows are lost.

The simplest fix would be to spool relationship bindings into a
buffer before mutating, matching the way the fully-anchored case
already works (because the vertex binding tuples are already
materialised). The exact-same shape of bug almost certainly applies
to `MATCH ()-[r:R]->() SET r.x = ...` if the SET reuses the same
cursor — worth checking.
