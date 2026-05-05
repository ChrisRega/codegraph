# demo-rust

Tiny demo crate used to exercise [`codegraph-indexer`](../../../crates/codegraph-indexer).

The library exposes two modules:

- `greet`: contains [`hello`] and [`shout`] producing greeting strings.
- `math`: contains `add` and `double`.

After indexing this project, queries like

```cypher
MATCH (caller:Function)-[:CALLS]->(callee:Function)
RETURN caller.qualified_name, callee.qualified_name
```

will surface `shout → hello` and `double → add` as `:CALLS` edges.
