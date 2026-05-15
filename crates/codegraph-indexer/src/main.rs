//! Thin CLI wrapper around `codegraph_indexer::run_indexer`.

use std::path::PathBuf;
use std::process::ExitCode;

use codegraph_indexer::{run_indexer, IndexOptions};

const HELP: &str = "\
codegraph-indexer — projects a codebase into a velr graph database

USAGE:
    codegraph-indexer [OPTIONS]

OPTIONS:
    --workspace <path>   Project root to index (default: .)
    --db        <path>   velr database file to write to (default: code-graph.db)
    --lsp       <bin>    Override the language-server binary
    --full               Force a full re-index (ignore the sidecar metadata)
    -h, --help           Show this help and exit
    -V, --version        Print version and exit

The first run on a fresh DB does a full index; subsequent runs use git diff
between the last-indexed commit (recorded in <db>.codegraph-meta.json) and
HEAD to re-parse only changed files.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("codegraph-indexer {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let workspace = flag(&args, "--workspace").unwrap_or_else(|| ".".to_string());
    let db_path = flag(&args, "--db").unwrap_or_else(|| "code-graph.db".to_string());
    let force_full = args.iter().any(|a| a == "--full");
    let lsp_cmd_override = flag(&args, "--lsp");

    let opts = IndexOptions {
        workspace: PathBuf::from(workspace),
        db_path,
        lsp_cmd_override,
        force_full,
    };

    match run_indexer(opts) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
