# demoapp (Go)

Tiny Go module used to exercise [`codegraph-indexer`](../../../crates/codegraph-indexer) against the Go path.

Two packages:

- `greet`: `Hello` and `Shout` build greeting strings; `Shout` calls `Hello`.
- `mathops`: `Add` and `Double`; `Double` calls `Add`.

After indexing this project, queries like

```cypher
MATCH (caller:Function)-[:CALLS]->(callee:Function)
RETURN caller.qualified_name, callee.qualified_name
```

should surface `Shout → Hello` and `Double → Add` as `:CALLS` edges, assuming `gopls` is on `$PATH`.
