# Security policy

## Reporting a vulnerability

Please **do not** open a public GitHub issue for a security finding.

Email **chris.vdop@gmail.com** with:

- A short description of the issue.
- Steps or a proof-of-concept that reproduces it.
- The version of `codegraph-*` and `velr` you tested against.

You should expect an acknowledgement within a few business days. If the
finding is confirmed, we'll coordinate a fix and a disclosure date with
you before any public discussion.

GitHub's [private security advisories](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability)
are also accepted on the project repository.

## Scope

This policy covers the three crates published from this workspace:

- `codegraph-core`
- `codegraph-indexer`
- `codegraph-mcp`

Issues in the upstream [velr](https://crates.io/crates/velr) graph
database itself should be reported to that project. We will, of course,
coordinate when a velr issue surfaces through one of our crates.

## What we treat as a security issue

- Cypher injection via inputs that the indexer or MCP server passes
  through to velr without escaping.
- Sandbox escapes from the MCP `cypher` / `write` tools (e.g. arbitrary
  filesystem or process access from a Cypher payload).
- Crashes or memory-corruption reachable via malformed inputs to the
  indexer (Rust source, Markdown, Gherkin, OpenAPI / GraphQL / Protobuf
  files).
- Information disclosure in error messages or logs.
