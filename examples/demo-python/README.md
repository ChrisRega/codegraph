# demo-python

Standalone demo project used by codegraph integration testing.
Intentionally tiny; mirrors `../demo-rust` and `../demo-typescript`.

```bash
# from repo root
codegraph-indexer --workspace examples/demo-python --db /tmp/demo-py.db
```

Requires `pyright-langserver` on `$PATH` (override with `--lsp <path>`).
