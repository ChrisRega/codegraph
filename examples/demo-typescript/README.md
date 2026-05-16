# demo-typescript

Standalone demo project used by codegraph integration testing.
Mirrors `../demo-rust` and `../demo-python`.

```bash
# from repo root
codegraph-indexer --workspace examples/demo-typescript --db /tmp/demo-ts.db
```

Requires `typescript-language-server` on `$PATH` (override with `--lsp <path>`).
No `npm install` needed for indexing — the indexer only reads the source files.
