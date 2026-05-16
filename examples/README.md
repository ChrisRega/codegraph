# codegraph examples

Tiny demo projects used for integration-testing the indexer against
each supported language path. All three follow the same shape so the
indexer's output is comparable: a `greet` module (`hello` + `shout`,
where `shout` calls `hello`), a `math` module (`add` + `double`,
where `double` calls `add`), plus a Markdown doc and at least one
test file.

| Project | Manifest | LSP |
| --- | --- | --- |
| [`demo-rust`](demo-rust)             | `Cargo.toml`     | `rust-analyzer` |
| [`demo-python`](demo-python)         | `pyproject.toml` | `pyright-langserver` |
| [`demo-typescript`](demo-typescript) | `package.json`   | `typescript-language-server` |
| [`demo-go`](demo-go)                 | `go.mod`         | `gopls` |

Build once, then index each:

```bash
cargo build --release

./target/release/codegraph-indexer --workspace examples/demo-rust       --db /tmp/demo-rs.db
./target/release/codegraph-indexer --workspace examples/demo-python     --db /tmp/demo-py.db
./target/release/codegraph-indexer --workspace examples/demo-typescript --db /tmp/demo-ts.db
./target/release/codegraph-indexer --workspace examples/demo-go         --db /tmp/demo-go.db
```

The LSP for the chosen language must be on `$PATH` (override with
`--lsp <path>`).

Then poke at the resulting graph:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"schema"}}' \
  | ./target/release/codegraph-mcp --db /tmp/demo-rs.db
```

After indexing, the same Cypher should return one `:CALLS` edge per
language for each pair (`shout → hello`, `double → add`):

```cypher
MATCH (caller:Function)-[:CALLS]->(callee:Function)
RETURN caller.qualified_name, callee.qualified_name
```

All three demos are intentionally excluded from the Cargo workspace
(see top-level `Cargo.toml`'s `exclude` list); they exist only as
fixtures for the indexer.

## What is NOT included today

- **`:Test` discovery is Rust-only.** The indexer adds the `:Test`
  label by scanning function bodies for `#[test]` / `#[tokio::test]`
  attributes (Phase 6, see `crates/codegraph-indexer/src/lib.rs`).
  Python `def test_*` and TS / vitest `it(...)` blocks index as
  ordinary `:Function` nodes today.
- **BDD `[:IMPLEMENTED_BY]` linking** runs only against Rust
  `#[given/when/then]` macros. The `demo-rust/tests/features/`
  Gherkin file is the canonical fixture for that path.
