//! Phase: agent-driven architecture overlay.
//!
//! Only runs when `IndexOptions::with_arch_agent` is set AND the pass
//! is a full reindex (not live). Spawns `claude -p <prompt>`, expects
//! a fenced JSON block back with the architecture plan, MERGEs it
//! into the graph as `:ArchModule` plus `[:CONTAINS]` /
//! `[:GROUPS]` / `[:USES]` edges.
//!
//! Failure modes (missing `claude` CLI, bad JSON, write errors) all
//! degrade silently — the previous overlay was already wiped, so the
//! graph ends with no `:ArchModule` rather than a partial one. The
//! caller logs to stderr so the user knows.
//!
//! Test boundary: every pure step (gather → prompt → parse → apply)
//! is its own function so unit tests can exercise them without
//! invoking the subprocess.

use std::collections::HashMap;
use std::process::Command;

use codegraph_core::{escape_str, Db};
use serde::Deserialize;

use crate::run;

/// Context handed to the agent. JSON-serialisable so the prompt
/// embeds it verbatim — the model sees the structure as a single,
/// stable shape rather than free-form prose.
#[derive(Debug, serde::Serialize)]
struct ArchContext {
    workspace: String,
    packages: Vec<PackageCtx>,
    cross_package_calls: Vec<CrossCallCtx>,
    manifest_deps: Vec<ManifestEdgeCtx>,
}

#[derive(Debug, serde::Serialize)]
struct PackageCtx {
    name: String,
    path: String,
    language: String,
    /// Top functions by inbound + outbound degree, used as
    /// "what does this package actually do" hints for the agent.
    top_functions: Vec<FunctionCtx>,
}

#[derive(Debug, serde::Serialize)]
struct FunctionCtx {
    qualified_name: String,
    kind: String,
    signature_hint: String,
}

#[derive(Debug, serde::Serialize)]
struct CrossCallCtx {
    src_package: String,
    dst_package: String,
    weight: u64,
}

#[derive(Debug, serde::Serialize)]
struct ManifestEdgeCtx {
    src_package: String,
    dst_package: String,
    kind: String,
}

/// Plan returned by the agent. Forgiving on missing fields so a partial
/// response still produces a usable overlay.
#[derive(Debug, Deserialize, Default)]
struct ArchPlan {
    #[serde(default)]
    modules: Vec<PlannedModule>,
    #[serde(default)]
    edges: Vec<PlannedEdge>,
}

#[derive(Debug, Deserialize, Default)]
struct PlannedModule {
    name: String,
    #[serde(default)]
    semantic_kind: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    layer_hint: i64,
    #[serde(default)]
    contains_packages: Vec<String>,
    #[serde(default)]
    groups_functions: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct PlannedEdge {
    from: String,
    to: String,
    #[serde(default)]
    semantic_kind: String,
}

/// Orchestrator. No-op for live mode (caller already checked) — this
/// is only called from `run_indexer_inner` when both `is_full` and the
/// `--with-arch-agent` flag are set.
pub fn phase_arch_agent(db: &Db, workspace_name: &str) {
    eprintln!("  [+] ArchAgent: gathering context …");
    // Wipe last overlay first so a failure leaves us with no overlay
    // rather than a stale one.
    run(db, "MATCH (a:ArchModule) DETACH DELETE a");

    let ctx = match gather_context(db, workspace_name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  [!] ArchAgent: gather_context failed: {e}");
            return;
        }
    };
    if ctx.packages.is_empty() {
        eprintln!("  [!] ArchAgent: no internal packages, skipping");
        return;
    }
    let instruction = build_instruction();
    let context_json = serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".to_string());
    eprintln!(
        "  [+] ArchAgent: invoking `claude -p` ({} packages, {} cross-package edges, \
         {} KiB context via stdin) …",
        ctx.packages.len(),
        ctx.cross_package_calls.len(),
        context_json.len() / 1024
    );
    let raw = match call_claude_cli(&instruction, &context_json) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  [!] ArchAgent: claude CLI failed ({e}) — overlay skipped");
            return;
        }
    };
    let plan = match parse_agent_response(&raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  [!] ArchAgent: response parse failed ({e}) — overlay skipped");
            return;
        }
    };
    apply_plan(db, &plan);
    eprintln!(
        "  [+] ArchAgent: wrote {} module(s), {} edge(s)",
        plan.modules.len(),
        plan.edges.len()
    );
}

// ── Pure DB → context fetch ──────────────────────────────────────────

fn gather_context(db: &Db, workspace_name: &str) -> Result<ArchContext, String> {
    let pkgs_t = db
        .query(
            "MATCH (p:Package) WHERE p.is_external = false \
             RETURN p.name AS name, p.path AS path, p.language AS lang",
        )
        .map_err(|e| e.to_string())?;
    let mut packages: Vec<PackageCtx> = Vec::new();
    for row in &pkgs_t.rows {
        let name = row
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let path = row
            .get(1)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let lang = row
            .get(2)
            .and_then(|c| c.as_str())
            .unwrap_or("Unknown")
            .to_string();
        if name.is_empty() || path.is_empty() {
            continue;
        }
        packages.push(PackageCtx {
            name,
            path,
            language: lang,
            top_functions: Vec::new(),
        });
    }

    // Top functions per package: pull all functions + path + degree, then
    // assign per longest path-prefix on the package side and trim per pkg.
    // DISTINCT guards against duplicate :Function or :DEFINED_IN edges
    // that accumulate across reindex passes (see velr-bugs/MERGE-on-rel).
    let funcs_t = db
        .query(
            "MATCH (f:Function)-[:DEFINED_IN]->(file:File) \
             OPTIONAL MATCH (f)-[r:CALLS]-() \
             RETURN DISTINCT f.qualified_name AS qn, f.kind AS kind, file.path AS path, count(r) AS deg",
        )
        .map_err(|e| e.to_string())?;
    // Pre-compute (name, path) pairs sorted by descending path length so
    // longest matching prefix wins. Owned copies decouple the iteration
    // below from the &mut Vec<PackageCtx> we hand back into.
    let mut sorted: Vec<(String, String)> = packages
        .iter()
        .map(|p| (p.name.clone(), p.path.clone()))
        .collect();
    sorted.sort_by_key(|(_, path)| std::cmp::Reverse(path.len()));
    let mut by_pkg: HashMap<String, Vec<FunctionCtx>> = HashMap::new();
    for row in &funcs_t.rows {
        let qn = row
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let kind = row
            .get(1)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let path = row.get(2).and_then(|c| c.as_str()).unwrap_or("");
        let deg = row.get(3).and_then(|c| c.as_i64()).unwrap_or(0);
        if qn.is_empty() {
            continue;
        }
        let Some((pkg_name, _)) = sorted.iter().find(|(_, p)| path.starts_with(p)) else {
            continue;
        };
        let bucket = by_pkg.entry(pkg_name.clone()).or_default();
        bucket.push(FunctionCtx {
            qualified_name: qn,
            kind,
            signature_hint: format!("deg {deg}"),
        });
    }
    for pkg in &mut packages {
        if let Some(mut list) = by_pkg.remove(&pkg.name) {
            // Stable sort: degree appears in signature_hint, parse back.
            list.sort_by_key(|f| {
                let n: i64 = f
                    .signature_hint
                    .strip_prefix("deg ")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                std::cmp::Reverse(n)
            });
            list.truncate(15);
            pkg.top_functions = list;
        }
    }

    // Cross-package CALLS rollup. Same path-prefix join, done in Rust to
    // skip velr's expensive aggregation surface.
    // DISTINCT for dedup (same accumulation pattern as above).
    let calls_t = db
        .query(
            "MATCH (src:Function)-[:CALLS]->(dst:Function), \
                   (src)-[:DEFINED_IN]->(sf:File), \
                   (dst)-[:DEFINED_IN]->(df:File) \
             RETURN DISTINCT src.qualified_name AS sname, dst.qualified_name AS dname, \
                    sf.path AS sp, df.path AS dp",
        )
        .map_err(|e| e.to_string())?;
    // Drop the extra columns once dedup is done — they're only there to
    // make the call edge identity unique, not for the aggregation.
    let mut cross_counts: HashMap<(String, String), u64> = HashMap::new();
    for row in &calls_t.rows {
        // Cols are now: sname, dname, sp, dp (added for DISTINCT identity).
        let sp = row.get(2).and_then(|c| c.as_str()).unwrap_or("");
        let dp = row.get(3).and_then(|c| c.as_str()).unwrap_or("");
        let Some((sname, _)) = sorted.iter().find(|(_, p)| sp.starts_with(p)) else {
            continue;
        };
        let Some((dname, _)) = sorted.iter().find(|(_, p)| dp.starts_with(p)) else {
            continue;
        };
        if sname == dname {
            continue;
        }
        *cross_counts
            .entry((sname.clone(), dname.clone()))
            .or_insert(0) += 1;
    }
    let mut cross_package_calls: Vec<CrossCallCtx> = cross_counts
        .into_iter()
        .map(|((s, d), w)| CrossCallCtx {
            src_package: s,
            dst_package: d,
            weight: w,
        })
        .collect();
    cross_package_calls.sort_by(|a, b| {
        a.src_package
            .cmp(&b.src_package)
            .then_with(|| a.dst_package.cmp(&b.dst_package))
    });

    // Manifest deps already in the graph: [:DEPENDS_ON {kind}] between
    // Packages. We restrict to edges originating from internal packages —
    // the full transitive external graph can be 10k+ edges (and 2 MiB+
    // of JSON) and adds no architectural signal beyond "our crates use
    // these crates".
    let deps_t = db
        .query(
            "MATCH (a:Package)-[r:DEPENDS_ON]->(b:Package) WHERE a.is_external = false \
             RETURN DISTINCT a.name AS src, b.name AS dst, r.kind AS kind",
        )
        .map_err(|e| e.to_string())?;
    let mut manifest_deps: Vec<ManifestEdgeCtx> = Vec::new();
    for row in &deps_t.rows {
        let src = row
            .first()
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let dst = row
            .get(1)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let kind = row
            .get(2)
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        if src.is_empty() || dst.is_empty() {
            continue;
        }
        manifest_deps.push(ManifestEdgeCtx {
            src_package: src,
            dst_package: dst,
            kind,
        });
    }

    Ok(ArchContext {
        workspace: workspace_name.to_string(),
        packages,
        cross_package_calls,
        manifest_deps,
    })
}

// ── Prompt construction ──────────────────────────────────────────────

/// Build the system-level instruction that goes as the `-p` arg.
/// The workspace context is piped via stdin (see `call_claude_cli`) —
/// embedding it in the arg overflows `E2BIG` on Linux for repos with
/// many functions, even though the limit is nominally 128 KiB.
fn build_instruction() -> String {
    "You are an architecture analyst. The workspace's static graph context \
     (packages, hot functions per package, cross-package CALLS, manifest \
     dependencies) is provided on stdin as a single JSON object.\n\
     \n\
     Propose a coarse-grained architecture view: a small set of ArchModules \
     that the codebase naturally decomposes into.\n\
     \n\
     An ArchModule MAY contain multiple :Package nodes (group small related \
     crates) and MAY group functions across packages by role \
     (`groups_functions`) when a package's responsibility actually splits. \
     Prefer module counts in the 3-7 range for repos this size; one module \
     per package is a smell unless every package is genuinely a separate \
     context.\n\
     \n\
     `semantic_kind` ∈ {core, adapter, protocol, cli, lib, app, test, infra}\n\
     `layer_hint` ∈ 0 (sink/foundational) .. higher (closer to user)\n\
     \n\
     Output ONLY a single fenced ```json block with this schema, nothing else:\n\
     \n\
     ```json\n\
     {\n\
       \"modules\": [\n\
         {\n\
           \"name\": \"...\",\n\
           \"semantic_kind\": \"...\",\n\
           \"description\": \"1-2 sentences\",\n\
           \"layer_hint\": 0,\n\
           \"contains_packages\": [\"pkg-name\"],\n\
           \"groups_functions\": [\"qualified::name\"]\n\
         }\n\
       ],\n\
       \"edges\": [\n\
         {\"from\": \"module-a\", \"to\": \"module-b\", \"semantic_kind\": \"data|control|protocol|test\"}\n\
       ]\n\
     }\n\
     ```"
        .to_string()
}

// ── Subprocess wrapper ───────────────────────────────────────────────

/// Invoke `claude -p <instruction>` with the workspace context piped
/// via stdin. Splitting prompt and context across arg + stdin is
/// mandatory: on Linux the cumulative `argv + envp` size is bounded
/// (often as low as 128 KiB, but trips much earlier for us — `E2BIG`
/// even for ~10 KiB contexts) and a big context overflows it.
fn call_claude_cli(instruction: &str, context_json: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("claude")
        .arg("-p")
        .arg(instruction)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "stdin not piped".to_string())?;
        stdin
            .write_all(context_json.as_bytes())
            .map_err(|e| format!("stdin write failed: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait failed: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "claude exited {} — stderr: {}",
            out.status,
            stderr.trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("non-utf8 output: {e}"))
}

// ── Response parsing ─────────────────────────────────────────────────

fn parse_agent_response(text: &str) -> Result<ArchPlan, String> {
    // Find ```json … ``` first; fall back to first … last `{` … `}` window.
    let block = extract_fenced_json(text).or_else(|| extract_brace_window(text));
    let raw = block.ok_or("no JSON block found in agent response")?;
    serde_json::from_str::<ArchPlan>(&raw).map_err(|e| format!("JSON deserialise failed: {e}"))
}

fn extract_fenced_json(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let start_marker = lower.find("```json")?;
    let after = start_marker + "```json".len();
    let end_rel = text[after..].find("```")?;
    Some(text[after..after + end_rel].trim().to_string())
}

fn extract_brace_window(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start {
        Some(text[start..=end].to_string())
    } else {
        None
    }
}

// ── Plan → graph writes ──────────────────────────────────────────────

fn apply_plan(db: &Db, plan: &ArchPlan) {
    for m in &plan.modules {
        if m.name.is_empty() {
            continue;
        }
        run(
            db,
            &format!(
                "MERGE (a:ArchModule {{name: {n}}}) \
                 SET a.semantic_kind = {sk}, a.description = {d}, a.layer_hint = {lh}",
                n = escape_str(&m.name),
                sk = escape_str(&m.semantic_kind),
                d = escape_str(&m.description),
                lh = m.layer_hint,
            ),
        );
        for pkg in &m.contains_packages {
            run(
                db,
                &format!(
                    "MATCH (a:ArchModule {{name: {an}}}), (p:Package {{name: {pn}}}) \
                     MERGE (a)-[:CONTAINS]->(p)",
                    an = escape_str(&m.name),
                    pn = escape_str(pkg),
                ),
            );
        }
        for qn in &m.groups_functions {
            run(
                db,
                &format!(
                    "MATCH (a:ArchModule {{name: {an}}}), (f:Function {{qualified_name: {qn}}}) \
                     MERGE (a)-[:GROUPS]->(f)",
                    an = escape_str(&m.name),
                    qn = escape_str(qn),
                ),
            );
        }
    }
    for e in &plan.edges {
        if e.from.is_empty() || e.to.is_empty() {
            continue;
        }
        run(
            db,
            &format!(
                "MATCH (a:ArchModule {{name: {f}}}), (b:ArchModule {{name: {t}}}) \
                 MERGE (a)-[r:USES]->(b) SET r.semantic_kind = {sk}",
                f = escape_str(&e.from),
                t = escape_str(&e.to),
                sk = escape_str(&e.semantic_kind),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ctx() -> ArchContext {
        ArchContext {
            workspace: "codegraph".into(),
            packages: vec![PackageCtx {
                name: "codegraph-core".into(),
                path: "crates/codegraph-core".into(),
                language: "Rust".into(),
                top_functions: vec![FunctionCtx {
                    qualified_name: "codegraph-core::escape_str".into(),
                    kind: "Free".into(),
                    signature_hint: "deg 42".into(),
                }],
            }],
            cross_package_calls: vec![],
            manifest_deps: vec![],
        }
    }

    #[test]
    fn build_instruction_contains_schema() {
        let p = build_instruction();
        assert!(p.contains("ArchModule"), "no module mention");
        assert!(p.contains("```json"), "no JSON fence");
        assert!(p.contains("contains_packages"), "no schema field");
        assert!(p.contains("stdin"), "stdin handoff not mentioned");
    }

    #[test]
    fn context_serialises_to_compact_json() {
        let json = serde_json::to_string(&sample_ctx()).unwrap();
        assert!(json.contains("codegraph-core"));
        assert!(json.contains("escape_str"));
    }

    #[test]
    fn parse_extracts_fenced_block() {
        let text = "Thinking…\n\n```json\n{\"modules\": [{\"name\": \"x\", \"semantic_kind\": \"core\", \"contains_packages\": [\"a\"]}]}\n```\nand more prose";
        let plan = parse_agent_response(text).expect("must parse");
        assert_eq!(plan.modules.len(), 1);
        assert_eq!(plan.modules[0].name, "x");
        assert_eq!(plan.modules[0].semantic_kind, "core");
        assert_eq!(plan.modules[0].contains_packages, vec!["a".to_string()]);
    }

    #[test]
    fn parse_falls_back_to_brace_window() {
        let text = "Some prose. {\"modules\": [], \"edges\": []} done.";
        let plan = parse_agent_response(text).expect("must parse");
        assert!(plan.modules.is_empty());
        assert!(plan.edges.is_empty());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_agent_response("nothing here").is_err());
        assert!(parse_agent_response("```json\nnot json\n```").is_err());
    }

    #[test]
    fn apply_plan_writes_modules_and_edges() {
        let db = Db::open_in_memory().unwrap();
        db.run("CREATE (:Package {name: 'a', is_external: false, path: 'crates/a'})")
            .unwrap();
        db.run("CREATE (:Package {name: 'b', is_external: false, path: 'crates/b'})")
            .unwrap();
        db.run("CREATE (:Function {qualified_name: 'a::foo'})")
            .unwrap();
        let plan = ArchPlan {
            modules: vec![
                PlannedModule {
                    name: "core".into(),
                    semantic_kind: "core".into(),
                    description: "the cold heart".into(),
                    layer_hint: 0,
                    contains_packages: vec!["a".into()],
                    groups_functions: vec!["a::foo".into()],
                },
                PlannedModule {
                    name: "edge".into(),
                    semantic_kind: "adapter".into(),
                    description: "".into(),
                    layer_hint: 1,
                    contains_packages: vec!["b".into()],
                    groups_functions: vec![],
                },
            ],
            edges: vec![PlannedEdge {
                from: "edge".into(),
                to: "core".into(),
                semantic_kind: "data".into(),
            }],
        };
        apply_plan(&db, &plan);
        let mods = db
            .query("MATCH (a:ArchModule) RETURN count(a) AS n")
            .unwrap();
        assert_eq!(mods.rows[0][0].as_i64(), Some(2));
        let contains = db
            .query("MATCH (a:ArchModule)-[:CONTAINS]->(p:Package) RETURN count(*) AS n")
            .unwrap();
        assert_eq!(contains.rows[0][0].as_i64(), Some(2));
        let groups = db
            .query("MATCH (a:ArchModule)-[:GROUPS]->(f:Function) RETURN count(*) AS n")
            .unwrap();
        assert_eq!(groups.rows[0][0].as_i64(), Some(1));
        let uses = db
            .query("MATCH (:ArchModule)-[r:USES]->(:ArchModule) RETURN r.semantic_kind AS k")
            .unwrap();
        assert_eq!(uses.rows[0][0].as_str(), Some("data"));
    }
}
