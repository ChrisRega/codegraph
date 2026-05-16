# demo-go

Standalone demo project used by codegraph integration testing.
Mirrors `../demo-rust`, `../demo-python`, `../demo-typescript`.

```bash
# from repo root
codegraph-indexer --workspace examples/demo-go --db /tmp/demo-go.db
```

Requires [`gopls`](https://pkg.go.dev/golang.org/x/tools/gopls) on `$PATH` (install with `go install golang.org/x/tools/gopls@latest`, override the binary with `--lsp <path>`).
