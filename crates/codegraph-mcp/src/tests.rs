use super::*;
use crate::pr_notes::extract_backticked_symbols;
use crate::util::chrono_now_iso;
use crate::views::substitute_view_params;
use crate::watch::{is_indexable_event_path, new_shared_status, rel_paths_from};
use codegraph_core::Table;

#[test]
fn tool_list_advertises_expected_tools() {
    let v = tool_list();
    let names: Vec<&str> = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "schema",
        "cypher",
        "begin",
        "write",
        "commit",
        "rollback",
        "explain",
        "cypher_md",
        "node_md",
        "write_note",
        "list_notes",
        "history",
        "impact",
        "find_symbol",
        "save_view",
        "view",
        "list_views",
        "diff_since",
        "define_concept",
        "concept",
        "list_concepts",
        "coverage_md",
        "explore",
        "graph_export",
        "dead_code",
        "arch_overlay",
        "index_status",
        "import_pr_notes",
        "watch",
        "unwatch",
        "list_watches",
        "worklog_create",
        "worklog_set_status",
        "worklog_comment",
        "worklog_list",
        "worklog_md",
    ] {
        assert!(names.contains(&expected), "missing tool: {expected}");
    }
}

#[test]
fn format_table_md_renders_gfm() {
    let t = Table {
        columns: vec!["name".into(), "n".into()],
        rows: vec![
            vec![Cell::Text("alpha".into()), Cell::Integer(1)],
            vec![Cell::Text("beta|x".into()), Cell::Integer(2)],
        ],
    };
    let md = format_table_md(&t);
    assert!(md.contains("| name | n |"));
    assert!(md.contains("| --- | --- |"));
    // pipe inside a cell must be escaped
    assert!(md.contains("beta\\|x"));
    assert!(md.contains("_2 rows_"));
}

#[test]
fn format_table_md_handles_empty() {
    let t = Table {
        columns: vec![],
        rows: vec![],
    };
    assert_eq!(format_table_md(&t), "_(no results)_");
}

fn seed_db() -> Db {
    let db = Db::open_in_memory().unwrap();
    db.run("CREATE (:Function {qualified_name: 'a::foo', name: 'foo', path: 'src/a.rs'})")
        .unwrap();
    db.run("CREATE (:Function {qualified_name: 'a::bar', name: 'bar', path: 'src/a.rs'})")
        .unwrap();
    db.run(
        "MATCH (a:Function {qualified_name: 'a::foo'}), (b:Function {qualified_name: 'a::bar'}) \
         CREATE (a)-[:CALLS]->(b)",
    )
    .unwrap();
    db
}

fn text_of(v: &Value) -> String {
    v["content"][0]["text"].as_str().unwrap_or("").to_string()
}

#[test]
fn cypher_md_renders_table() {
    let db = seed_db();
    let v = handle_cypher_md(
        &db,
        &json!({ "query": "MATCH (f:Function) RETURN f.name AS name ORDER BY name" }),
    );
    let md = text_of(&v);
    assert!(md.contains("| name |"), "got {md}");
    assert!(md.contains("bar"));
    assert!(md.contains("foo"));
    assert!(md.contains("_2 rows_"));
}

#[test]
fn node_md_lists_neighbours() {
    let db = seed_db();
    let v = handle_node_md(
        &db,
        &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
    );
    let md = text_of(&v);
    assert!(md.contains("# `:Function"));
    assert!(md.contains("## Properties"));
    assert!(md.contains("## Outgoing edges"));
    assert!(md.contains("-[:CALLS]->"));
    assert!(md.contains("a::bar"));
}

#[test]
fn node_md_ranks_neighbours_by_degree() {
    let db = Db::open_in_memory().unwrap();
    // hub has many incoming/outgoing edges; leaf has just one.
    for n in ["seed", "hub", "leaf", "x1", "x2", "x3"] {
        db.run(&format!(
            "CREATE (:Function {{qualified_name: 'r::{n}', name: '{n}'}})"
        ))
        .unwrap();
    }
    // seed -> hub, seed -> leaf
    for tgt in ["hub", "leaf"] {
        db.run(&format!(
            "MATCH (a:Function {{qualified_name: 'r::seed'}}), \
                   (b:Function {{qualified_name: 'r::{tgt}'}}) \
             CREATE (a)-[:CALLS]->(b)"
        ))
        .unwrap();
    }
    // hub gets multiple extra edges so its degree dwarfs leaf's.
    for tgt in ["x1", "x2", "x3"] {
        db.run(&format!(
            "MATCH (a:Function {{qualified_name: 'r::hub'}}), \
                   (b:Function {{qualified_name: 'r::{tgt}'}}) \
             CREATE (a)-[:CALLS]->(b)"
        ))
        .unwrap();
    }
    let v = handle_node_md(
        &db,
        &json!({ "label": "Function", "key": "qualified_name", "value": "r::seed" }),
    );
    let md = text_of(&v);
    // hub must appear before leaf in the outgoing CALLS group.
    let pos_hub = md.find("r::hub").expect("hub missing");
    let pos_leaf = md.find("r::leaf").expect("leaf missing");
    assert!(pos_hub < pos_leaf, "hub should outrank leaf:\n{md}");
    // degree tag rendered for non-zero degree
    assert!(md.contains("_(deg "), "degree tag missing:\n{md}");
}

#[test]
fn index_status_renders_stub_without_watch() {
    let status = new_shared_status();
    let tx = TxState::new();
    let v = handle_index_status(&status, None, &tx, "/nonexistent/codegraph.db");
    let md = text_of(&v);
    assert!(md.contains("# Indexer status"), "{md}");
    assert!(md.contains("not running"), "{md}");
    // DB-section is unconditional now — surfaces sizes even without watcher.
    assert!(md.contains("## Database files"), "{md}");
    assert!(md.contains("**Open buffered tx:** none"), "{md}");
}

#[test]
fn index_status_reflects_watcher_state() {
    let status = new_shared_status();
    // Simulate a completed run.
    if let Ok(mut s) = status.lock() {
        s.state = "idle".to_string();
        s.last_run_at = "2026-05-15T18:55:00Z".to_string();
        s.last_run_mode = "live".to_string();
        s.last_run_duration_ms = 142;
        s.runs_total = 7;
        s.last_paths = vec!["src/lib.rs".into(), "README.md".into()];
        s.head_hash = "abcd1234ef567890".into();
    }
    let tx = TxState::new();
    let md = text_of(&handle_index_status(
        &status,
        Some("/tmp/ws"),
        &tx,
        "/nonexistent/codegraph.db",
    ));
    assert!(md.contains("`/tmp/ws`"), "{md}");
    assert!(md.contains("`idle`"), "{md}");
    assert!(md.contains("`live`"), "{md}");
    assert!(md.contains("142ms"), "{md}");
    assert!(md.contains("Runs total:** 7"), "{md}");
    assert!(md.contains("`abcd1234`"), "{md}");
    assert!(md.contains("src/lib.rs"));
    assert!(md.contains("README.md"));
    // Events line always present (even at zero) so the agent learns the
    // counter exists and starts checking it.
    assert!(md.contains("**Events:** 0 total, 0 pending"), "{md}");
}

/// Verify the new "pending events queued behind a running pass"
/// visibility surface — answers the user complaint that fast editing
/// doesn't show as activity.
#[test]
fn index_status_surfaces_pending_events_during_running_pass() {
    let status = new_shared_status();
    if let Ok(mut s) = status.lock() {
        s.state = "running".to_string();
        s.events_total = 47;
        s.events_pending = 12;
        s.pending_paths = vec![
            "crates/codegraph-mcp/src/main.rs".into(),
            "crates/codegraph-indexer/src/lib.rs".into(),
        ];
    }
    let tx = TxState::new();
    let md = text_of(&handle_index_status(
        &status,
        Some("/tmp/ws"),
        &tx,
        "/nonexistent/codegraph.db",
    ));
    assert!(md.contains("`running`"), "{md}");
    assert!(md.contains("47 total, 12 pending"), "{md}");
    assert!(md.contains("Pending paths"), "{md}");
    assert!(md.contains("crates/codegraph-mcp/src/main.rs"), "{md}");
}

#[test]
fn index_status_surfaces_open_buffered_tx() {
    use crate::tx::handle_begin;
    let status = new_shared_status();
    let mut tx = TxState::new();
    handle_begin(&mut tx, &serde_json::json!({"message": "stuck-tx-demo"}));
    tx.pending.push("CREATE (:T)".to_string());
    let md = text_of(&handle_index_status(
        &status,
        Some("/tmp/ws"),
        &tx,
        "/nonexistent/codegraph.db",
    ));
    assert!(md.contains("## Database files"), "{md}");
    assert!(md.contains("**Open buffered tx:** tx#1"), "{md}");
    assert!(md.contains("1 queries pending"), "{md}");
    assert!(md.contains("stuck-tx-demo"), "{md}");
}

#[test]
fn explore_returns_seed_and_neighbours_within_budget() {
    let db = Db::open_in_memory().unwrap();
    db.run("CREATE (:Function {qualified_name: 'm::root', name: 'root'})")
        .unwrap();
    for n in ["a", "b", "c", "d", "e"] {
        db.run(&format!(
            "CREATE (:Function {{qualified_name: 'm::{n}', name: '{n}'}})"
        ))
        .unwrap();
        db.run(&format!(
            "MATCH (r:Function {{qualified_name: 'm::root'}}), \
                   (n:Function {{qualified_name: 'm::{n}'}}) CREATE (r)-[:CALLS]->(n)"
        ))
        .unwrap();
    }
    // Make `m::a` a hub.
    for tgt in ["b", "c", "d"] {
        db.run(&format!(
            "MATCH (a:Function {{qualified_name: 'm::a'}}), \
                   (b:Function {{qualified_name: 'm::{tgt}'}}) CREATE (a)-[:CALLS]->(b)"
        ))
        .unwrap();
    }

    let v = handle_explore(
        &db,
        &json!({
            "label": "Function", "key": "qualified_name", "value": "m::root",
            "char_budget": 8000, "max_depth": 2
        }),
    );
    let md = text_of(&v);
    assert!(md.contains("# Explore:"), "{md}");
    assert!(md.contains("## Seed"));
    assert!(md.contains("Neighbourhood"));
    let pos_a = md.find("m::a").expect("hub missing");
    let pos_e = md.find("m::e").expect("leaf missing");
    assert!(pos_a < pos_e, "hub should outrank leaf:\n{md}");
    assert!(md.contains("Showed"));
}

#[test]
fn explore_respects_tight_budget_and_reports_drops() {
    let db = Db::open_in_memory().unwrap();
    db.run("CREATE (:Function {qualified_name: 'r::seed', name: 'seed'})")
        .unwrap();
    for i in 0..30 {
        db.run(&format!(
            "CREATE (:Function {{qualified_name: 'r::n{i}', name: 'n{i}'}})"
        ))
        .unwrap();
        db.run(&format!(
            "MATCH (s:Function {{qualified_name: 'r::seed'}}), \
                   (n:Function {{qualified_name: 'r::n{i}'}}) CREATE (s)-[:CALLS]->(n)"
        ))
        .unwrap();
    }
    let md = text_of(&handle_explore(
        &db,
        &json!({
            "label": "Function", "key": "qualified_name", "value": "r::seed",
            "char_budget": 800, "max_depth": 1
        }),
    ));
    assert!(
        md.len() <= 1000,
        "exceeded budget: {} bytes\n{md}",
        md.len()
    );
    assert!(md.contains("dropped"));
    let visible = (0..30).filter(|i| md.contains(&format!("r::n{i}"))).count();
    assert!(
        visible > 0 && visible < 30,
        "expected partial truncation, got {visible}/30:\n{md}"
    );
}

#[test]
fn explore_handles_unknown_seed() {
    let db = Db::open_in_memory().unwrap();
    let md = text_of(&handle_explore(
        &db,
        &json!({"label":"Function","key":"qualified_name","value":"nope"}),
    ));
    assert!(md.contains("# Not found"));
}

#[test]
fn coverage_md_surfaces_orphans_and_untested() {
    let db = Db::open_in_memory().unwrap();
    // Two functions; `caller` calls `callee`. `caller` has no inbound
    // CALLS (orphan); `callee` has inbound CALLS but no `:TESTS`.
    for n in ["caller", "callee"] {
        db.run(&format!(
            "CREATE (:Function {{qualified_name: 'm::{n}', name: '{n}'}})"
        ))
        .unwrap();
    }
    db.run(
        "MATCH (a:Function {qualified_name: 'm::caller'}), \
               (b:Function {qualified_name: 'm::callee'}) CREATE (a)-[:CALLS]->(b)",
    )
    .unwrap();
    // A file with no notes attached.
    db.run("CREATE (:File {path: 'src/lonely.rs'})").unwrap();
    // A package with no doc-mentioned function.
    db.run("CREATE (:Package {name: 'undocumented-pkg'})")
        .unwrap();

    let v = handle_coverage_md(&db, &json!({ "limit": 10 }));
    let md = text_of(&v);
    assert!(md.contains("# Graph coverage report"));
    // orphan section: caller is orphan, callee is not.
    assert!(md.contains("Orphan functions"));
    assert!(md.contains("m::caller"));
    // untested ranked section: callee has fan-in 1 and no TESTS.
    assert!(md.contains("Untested functions"));
    assert!(md.contains("m::callee"));
    // file with no note.
    assert!(md.contains("Files with no `:Note`"));
    assert!(md.contains("src/lonely.rs"));
    // undocumented package.
    assert!(md.contains("Packages with zero doc-mentions"));
    assert!(md.contains("undocumented-pkg"));
}

#[test]
fn coverage_md_excludes_test_functions_from_orphans() {
    let db = Db::open_in_memory().unwrap();
    // A test function with no inbound CALLS should NOT show up in orphans.
    db.run("CREATE (:Function:Test {qualified_name: 'm::it_works', name: 'it_works'})")
        .unwrap();
    let md = text_of(&handle_coverage_md(&db, &json!({})));
    assert!(
        !md.contains("m::it_works"),
        "test fn should be excluded from orphans/untested:\n{md}"
    );
}

#[test]
fn is_indexable_event_path_skips_indexer_outputs() {
    // Sidecar that the indexer itself writes — would feedback-loop the watcher.
    assert!(!is_indexable_event_path(std::path::Path::new(
        "/ws/codegraph.db.codegraph-meta.json"
    )));
    // velr db + its SQLite siblings.
    assert!(!is_indexable_event_path(std::path::Path::new(
        "/ws/codegraph.db"
    )));
    assert!(!is_indexable_event_path(std::path::Path::new(
        "/ws/codegraph.db-wal"
    )));
    assert!(!is_indexable_event_path(std::path::Path::new(
        "/ws/codegraph.db-shm"
    )));
    // Real source still passes.
    assert!(is_indexable_event_path(std::path::Path::new(
        "/ws/src/main.rs"
    )));
    // Other JSON files still indexable (e.g. OpenAPI spec).
    assert!(is_indexable_event_path(std::path::Path::new(
        "/ws/api/openapi.json"
    )));
}

#[test]
fn rel_paths_from_strips_workspace_prefix() {
    let ws = std::path::PathBuf::from("/tmp/ws");
    let abs = vec![
        std::path::PathBuf::from("/tmp/ws/src/a.rs"),
        std::path::PathBuf::from("/tmp/ws/docs/x.md"),
        // Outside workspace ⇒ dropped.
        std::path::PathBuf::from("/etc/passwd"),
    ];
    let rels = rel_paths_from(&ws, &abs);
    assert_eq!(rels, vec!["src/a.rs".to_string(), "docs/x.md".into()]);
}

#[test]
fn write_note_attaches_and_list_finds_it() {
    let db = seed_db();
    let res = handle_write_note(
        &db,
        &json!({
            "match": "MATCH (t:Function {qualified_name: 'a::foo'})",
            "title": "hot path",
            "markdown": "called from request handler",
            "author": "claude",
            "tags": "perf,hot"
        }),
    );
    let txt = text_of(&res);
    assert!(txt.contains("attached to 1 target"), "got {txt}");

    // appears in list_notes (unfiltered)
    let list = text_of(&handle_list_notes(&db, &json!({})));
    assert!(list.contains("hot path"));
    assert!(list.contains("called from request handler"));

    // appears under the function in node_md
    let node = text_of(&handle_node_md(
        &db,
        &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
    ));
    assert!(node.contains("## Notes"));
    assert!(node.contains("hot path"));
}

#[test]
fn write_note_rejects_when_no_target_matches() {
    let db = seed_db();
    let res = handle_write_note(
        &db,
        &json!({
            "match": "MATCH (t:Function {qualified_name: 'does::not::exist'})",
            "markdown": "irrelevant"
        }),
    );
    assert!(res
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    // and no orphan note was left behind
    let count = db
        .query("MATCH (n:Note) RETURN count(n) AS c")
        .unwrap()
        .rows[0][0]
        .as_i64()
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn node_md_rejects_unsafe_label() {
    let db = seed_db();
    let res = handle_node_md(
        &db,
        &json!({ "label": "Function`); MATCH (n) DETACH DELETE n; //",
                 "key": "qualified_name", "value": "x" }),
    );
    assert!(res
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
}

#[test]
fn history_handler_lists_commits() {
    let db = Db::open_in_memory().unwrap();
    db.run("CREATE (:GitCommit {hash: 'aaa111', short_hash: 'aaa111', message: 'first', timestamp: '2026-01-01T00:00:00Z'})").unwrap();
    db.run("CREATE (:GitCommit {hash: 'bbb222', short_hash: 'bbb222', message: 'second', timestamp: '2026-02-01T00:00:00Z'})").unwrap();
    let v = handle_history(&db, &json!({}));
    let md = text_of(&v);
    assert!(md.contains("# Indexed commits"));
    assert!(md.contains("aaa111"));
    assert!(md.contains("bbb222"));
}

#[test]
fn impact_reports_callers_and_callees() {
    let db = Db::open_in_memory().unwrap();
    // Chain a -> b -> c -> d, plus e -> b (so b has two callers transitively from a's POV).
    for name in ["a", "b", "c", "d", "e"] {
        db.run(&format!(
            "CREATE (:Function {{qualified_name: 'x::{name}', name: '{name}', path: 'src/x.rs'}})"
        ))
        .unwrap();
    }
    for (caller, callee) in [("a", "b"), ("b", "c"), ("c", "d"), ("e", "b")] {
        db.run(&format!(
            "MATCH (u:Function {{qualified_name: 'x::{caller}'}}), \
                   (v:Function {{qualified_name: 'x::{callee}'}}) \
             CREATE (u)-[:CALLS]->(v)"
        ))
        .unwrap();
    }
    let v = handle_impact(&db, &json!({ "value": "x::b", "depth": 3, "top": 10 }));
    let md = text_of(&v);
    assert!(md.contains("# Impact:"), "got {md}");
    assert!(md.contains("Blast radius:"));
    // b's callees transitively: c, d
    assert!(md.contains("x::c"));
    assert!(md.contains("x::d"));
    // b's callers: a, e
    assert!(md.contains("x::a"));
    assert!(md.contains("x::e"));
}

#[test]
fn watch_unwatch_lifecycle() {
    let db = seed_db();
    let r = handle_watch(
        &db,
        &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
    );
    let txt = text_of(&r);
    assert!(txt.contains("watching"), "got {txt}");

    let lw = text_of(&handle_list_watches(&db));
    assert!(lw.contains("a::foo"), "got {lw}");

    let r = handle_unwatch(
        &db,
        &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
    );
    assert!(text_of(&r).contains("unwatched"));
    let lw2 = text_of(&handle_list_watches(&db));
    assert!(lw2.contains("nothing is watched"), "got {lw2}");
}

#[test]
fn watch_rejects_unknown_node() {
    let db = seed_db();
    let r = handle_watch(
        &db,
        &json!({ "label": "Function", "key": "qualified_name", "value": "no::such" }),
    );
    assert!(r.get("isError").and_then(|v| v.as_bool()).unwrap_or(false));
}

#[test]
fn extract_backticked_symbols_strips_calls_and_codeblocks() {
    let body = "Looking at `foo` and `a::bar()`. Don't include\n```\n`fenced::not_a_symbol`\n```\nbut keep `Baz`.";
    let syms = extract_backticked_symbols(body);
    assert!(syms.contains(&"foo".to_string()), "{syms:?}");
    assert!(syms.contains(&"a::bar".to_string()), "{syms:?}");
    assert!(syms.contains(&"Baz".to_string()), "{syms:?}");
    assert!(
        !syms.iter().any(|s| s.contains("fenced")),
        "fenced block leaked: {syms:?}"
    );
}

#[test]
fn import_pr_notes_attaches_to_matching_function() {
    let db = seed_db();
    let v = handle_import_pr_notes(
        &db,
        &json!({
            "pr": "42",
            "comments": [
                { "author": "alice", "body": "I think `foo` calls `bar` redundantly here.", "url": "https://example/c/1" },
                { "author": "bob", "body": "totally unrelated chatter, nothing to see" },
                { "author": "carol", "body": "see also `does_not_exist` for reference" }
            ]
        }),
    );
    let txt = text_of(&v);
    assert!(txt.contains("Processed 3 comments"), "got {txt}");
    assert!(txt.contains("created 1 notes"), "got {txt}");
    assert!(txt.contains("attached to 2"), "got {txt}");

    let dossier = text_of(&handle_node_md(
        &db,
        &json!({ "label": "Function", "key": "qualified_name", "value": "a::foo" }),
    ));
    assert!(dossier.contains("PR 42 — alice"), "{dossier}");
    assert!(dossier.contains("redundantly"));
}

#[test]
fn concept_lifecycle_define_then_render() {
    let db = seed_db();
    // Define a concept covering all functions in 'a::'.
    let r = handle_define_concept(
        &db,
        &json!({
            "name": "module-a",
            "description": "everything in module a",
            "match": "MATCH (t:Function) WHERE t.qualified_name STARTS WITH 'a::'"
        }),
    );
    let txt = text_of(&r);
    assert!(txt.contains("describes 2 members"), "got {txt}");

    // list_concepts shows it
    let lc = text_of(&handle_list_concepts(&db));
    assert!(lc.contains("module-a"), "got {lc}");
    assert!(lc.contains("everything in module a"));

    // dossier
    let c = text_of(&handle_concept(&db, &json!({ "name": "module-a" })));
    assert!(c.contains("# Concept `module-a`"));
    assert!(c.contains("everything in module a"));
    assert!(c.contains("a::foo"));
    assert!(c.contains("a::bar"));
    // No tests in seed_db, but section header still appears.
    assert!(c.contains("Functions in scope"));
}

#[test]
fn concept_unknown_returns_not_found() {
    let db = seed_db();
    let v = handle_concept(&db, &json!({ "name": "nope" }));
    assert!(text_of(&v).contains("# Not found"));
}

#[test]
fn diff_since_lists_commits_and_added_nodes() {
    let db = Db::open_in_memory().unwrap();
    // Three commits in chronological order.
    for (h, sh, ts, msg) in [
        ("aaa1aaa1", "aaa1aaa", "2026-01-01T00:00:00Z", "first"),
        ("bbb2bbb2", "bbb2bbb", "2026-01-02T00:00:00Z", "second"),
        ("ccc3ccc3", "ccc3ccc", "2026-01-03T00:00:00Z", "third"),
    ] {
        db.run(&format!(
            "CREATE (:GitCommit {{hash: '{h}', short_hash: '{sh}', \
             timestamp: '{ts}', message: '{msg}'}})"
        ))
        .unwrap();
    }
    // Workspace + SNAPSHOT_OF on HEAD (ccc3).
    db.run("CREATE (:Workspace {name: 'ws'})").unwrap();
    db.run(
        "MATCH (c:GitCommit {hash: 'ccc3ccc3'}), (w:Workspace) \
         CREATE (c)-[:SNAPSHOT_OF]->(w)",
    )
    .unwrap();
    // Functions with first_seen at different commits.
    db.run("CREATE (:Function {qualified_name: 'old::a', first_seen_commit: 'aaa1aaa1'})")
        .unwrap();
    db.run("CREATE (:Function {qualified_name: 'mid::b', first_seen_commit: 'bbb2bbb2'})")
        .unwrap();
    db.run("CREATE (:Function {qualified_name: 'new::c', first_seen_commit: 'ccc3ccc3'})")
        .unwrap();
    // File added at bbb.
    db.run("CREATE (:File {path: 'src/new.rs', first_seen_commit: 'bbb2bbb2'})")
        .unwrap();

    let v = handle_diff_since(&db, &json!({ "commit": "aaa1aaa" }));
    let md = text_of(&v);
    // commits in range: bbb + ccc (not aaa itself; baseline excluded).
    assert!(md.contains("bbb2bbb"), "got {md}");
    assert!(md.contains("ccc3ccc"));
    // added Functions: mid::b and new::c, but NOT old::a
    assert!(md.contains("mid::b"));
    assert!(md.contains("new::c"));
    assert!(!md.contains("old::a"));
    // added File
    assert!(md.contains("src/new.rs"));
    // tombstone caveat present
    assert!(md.contains("Removals are"));
}

#[test]
fn diff_since_unknown_commit_returns_message() {
    let db = Db::open_in_memory().unwrap();
    db.run(
        "CREATE (:GitCommit {hash: 'aaa', short_hash: 'aaa', timestamp: '2026-01-01T00:00:00Z'})",
    )
    .unwrap();
    db.run("CREATE (:Workspace {name: 'ws'})").unwrap();
    db.run("MATCH (c:GitCommit), (w:Workspace) CREATE (c)-[:SNAPSHOT_OF]->(w)")
        .unwrap();
    let v = handle_diff_since(&db, &json!({ "commit": "zzznope" }));
    let md = text_of(&v);
    assert!(md.contains("no `:GitCommit` matches"), "got {md}");
}

#[test]
fn substitute_view_params_replaces_tokens() {
    let mut m = serde_json::Map::new();
    m.insert("name".into(), Value::String("a::foo".into()));
    m.insert("limit".into(), Value::Number(10.into()));
    let out = substitute_view_params(
        "MATCH (f:Function {qualified_name: $name}) RETURN f LIMIT $limit",
        &m,
    );
    // 'a::foo' must be string-escaped, 10 must come through as a string-escaped numeric.
    assert!(out.contains("'a::foo'"), "got {out}");
    assert!(out.contains("'10'"), "got {out}");
    // unknown token stays:
    let out2 = substitute_view_params("RETURN $unknown", &m);
    assert_eq!(out2, "RETURN $unknown");
}

#[test]
fn save_view_then_view_runs_with_params() {
    let db = seed_db();
    let r = handle_save_view(
        &db,
        &json!({
            "name": "by-name",
            "cypher": "MATCH (f:Function {name: $name}) RETURN f.qualified_name AS qn",
            "description": "find a function by short name"
        }),
    );
    assert!(text_of(&r).contains("saved view"));

    // list_views surfaces it
    let lv = text_of(&handle_list_views(&db));
    assert!(lv.contains("by-name"), "got {lv}");
    assert!(lv.contains("find a function by short name"));

    // running with params returns the matching row
    let v = text_of(&handle_view(
        &db,
        &json!({ "name": "by-name", "params": { "name": "foo" } }),
    ));
    assert!(v.contains("a::foo"), "got {v}");
    assert!(v.contains("```cypher"));
}

#[test]
fn view_unknown_name_returns_empty_message() {
    let db = seed_db();
    let v = text_of(&handle_view(&db, &json!({ "name": "does-not-exist" })));
    assert!(v.contains("no view named"));
}

#[test]
fn save_view_rejects_invalid_name() {
    let db = seed_db();
    let r = handle_save_view(
        &db,
        &json!({ "name": "bad name with spaces", "cypher": "RETURN 1" }),
    );
    assert!(r.get("isError").and_then(|v| v.as_bool()).unwrap_or(false));
}

#[test]
fn find_symbol_ranks_exact_above_substring() {
    let db = Db::open_in_memory().unwrap();
    // Names where "format" appears in different positions / completeness.
    for (qn, name, kind, line) in [
        ("a::format_table", "format_table", "fn", 10),
        ("a::format", "format", "fn", 20), // exact
        ("a::reformat_input", "reformat_input", "fn", 30),
        ("a::unrelated", "unrelated", "fn", 40),
    ] {
        db.run(&format!(
            "CREATE (s:Function {{qualified_name: '{qn}', name: '{name}', kind: '{kind}', \
             line_start: {line}, body: 'fn {name}() {{}}' }})"
        ))
        .unwrap();
    }
    db.run("CREATE (:File {path: 'src/a.rs'})").unwrap();
    db.run(
        "MATCH (s:Function), (f:File {path: 'src/a.rs'}) \
         WHERE s.qualified_name STARTS WITH 'a::' \
         CREATE (s)-[:DEFINED_IN]->(f)",
    )
    .unwrap();

    let v = handle_find_symbol(&db, &json!({ "query": "format", "limit": 10 }));
    let md = text_of(&v);
    assert!(md.contains("a::format"), "got {md}");
    // Exact match must come before substring matches.
    let pos_exact = md.find("a::format").unwrap();
    let pos_table = md.find("a::format_table").unwrap();
    let pos_re = md.find("a::reformat_input").unwrap();
    assert!(pos_exact < pos_table, "exact should rank first");
    assert!(
        pos_table < pos_re,
        "name-startsWith should outrank tail-substring"
    );
    assert!(!md.contains("unrelated"));
}

#[test]
fn find_symbol_returns_no_match_message() {
    let db = seed_db();
    let v = handle_find_symbol(&db, &json!({ "query": "nonexistent_zzzz" }));
    let md = text_of(&v);
    assert!(
        md.contains("no `:Function` or `:Symbol` matched"),
        "got {md}"
    );
}

#[test]
fn impact_handles_unknown_seed() {
    let db = seed_db();
    let v = handle_impact(&db, &json!({ "value": "does::not::exist" }));
    let md = text_of(&v);
    assert!(md.contains("# Not found"));
}

#[test]
fn chrono_now_iso_is_iso_shaped() {
    let s = chrono_now_iso();
    assert!(s.ends_with('Z'), "got {s}");
    assert_eq!(s.as_bytes()[4], b'-');
    assert_eq!(s.as_bytes()[7], b'-');
    assert_eq!(s.as_bytes()[10], b'T');
}

#[test]
fn format_table_renders_rows_as_tsv() {
    let t = Table {
        columns: vec!["name".into(), "n".into()],
        rows: vec![
            vec![Cell::Text("alpha".into()), Cell::Integer(1)],
            vec![Cell::Text("beta".into()), Cell::Integer(2)],
        ],
    };
    let out = format_table(&t);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "name\tn");
    assert!(lines[1].contains("alpha"));
    assert!(lines[1].ends_with("\t1"));
    assert!(lines[2].contains("beta"));
}

#[test]
fn format_table_handles_empty() {
    let t = Table {
        columns: vec![],
        rows: vec![],
    };
    assert_eq!(format_table(&t), "(no results)");
}
