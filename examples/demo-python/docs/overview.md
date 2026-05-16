# demoapp

Tiny demo package used to exercise [`codegraph-indexer`](../../../crates/codegraph-indexer) against a Python project.

Two modules:

- `greet`: `hello` and `shout` build greeting strings; `shout` calls `hello`.
- `math_ops`: `add` and `double`; `double` calls `add`.

After indexing this project, queries like

```cypher
MATCH (caller:Function)-[:CALLS]->(callee:Function)
RETURN caller.qualified_name, callee.qualified_name
```

should surface `shout → hello` and `double → add` as `:CALLS` edges, assuming `pyright-langserver` is on `$PATH`.
