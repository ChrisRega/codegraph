# Examples

## `demo-rust`

A minimum Rust crate (a few functions, one Markdown doc, one Gherkin
feature) you can index right after building the workspace:

```bash
cargo build --release
./target/release/codegraph-indexer \
    --workspace examples/demo-rust \
    --db /tmp/demo.db
```

Then poke at the resulting graph:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"schema"}}' \
  | ./target/release/codegraph-mcp --db /tmp/demo.db
```

This requires `rust-analyzer` on `$PATH`.
