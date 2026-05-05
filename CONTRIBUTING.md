# Contributing to codegraph

Thanks for taking the time to look at this. Here's the short version of how to land a change.

## Development setup

```bash
git clone <repo-url> codegraph
cd codegraph
cargo build --workspace
cargo test --workspace
```

The optional smoke tests open an in-memory velr database and don't need any special setup.

`codegraph-indexer` itself needs a language server on `$PATH` for the LSP-driven phases:

- `rust-analyzer` for Rust projects
- `typescript-language-server --stdio` for Node/TypeScript projects
- `pyright-langserver --stdio` for Python projects

You can override the binary with `--lsp <path>`.

## Local checks

Before opening a PR, please run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI will run the same set on Linux and macOS.

## Pull requests

- Keep changes focused. One commit per logical change is preferred but a clean PR-level diff is fine.
- Reference any related issue with `Closes #N` / `Fixes #N`.
- Update [`TODO.md`](TODO.md) and [`CHANGELOG.md`](CHANGELOG.md) if your change moves something tracked there.
- The license is dual MIT / Apache-2.0. By submitting a contribution you agree it is released under both, per the [Rust convention](https://www.apache.org/licenses/LICENSE-2.0#contributions).

## Reporting bugs

Use GitHub Issues. Helpful information:

- The exact command line.
- The Cypher query (if any) that triggered it.
- Whether the database is fresh or pre-existing — and a minimal reproducing input where possible.
- velr version (`cargo tree -p velr`) and toolchain (`rustc --version`).

## Reporting security issues

See [SECURITY.md](SECURITY.md) — please do **not** open a public issue for security findings.

## Code of conduct

Participation in this project is governed by the [Contributor Covenant](CODE_OF_CONDUCT.md).
