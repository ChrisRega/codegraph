# demo-typescript

Tiny TypeScript package used to exercise [`codegraph-indexer`](../../../crates/codegraph-indexer).

Two modules:

- `greet`: `hello` and `shout` build greeting strings; `shout` calls `hello`.
- `math`: `add` and `double`; `double` calls `add`.

After indexing this project, queries like

```cypher
MATCH (caller:Function)-[:CALLS]->(callee:Function)
RETURN caller.qualified_name, callee.qualified_name
```

should surface `shout → hello` and `double → add` as `:CALLS` edges, assuming `typescript-language-server` is on `$PATH`.
