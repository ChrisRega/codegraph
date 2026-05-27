//! Manifest-driven package + source-file discovery for the supported
//! project kinds (Cargo / npm / pyproject / go.mod). Each
//! `index_*_packages` function writes `:Package` + `[:CONTAINS]` +
//! `[:DEPENDS_ON]` edges into the graph; `collect_source_files`
//! enumerates the source paths a downstream LSP pass should open.
//!
//! Extracted from `lib.rs` in nx-3 — kept as a single sibling module
//! because the four discovery paths share a common output schema and
//! evolve together when a new language joins.

use std::path::{Path, PathBuf};

use codegraph_core::{escape_str, Db};
use walkdir::WalkDir;

use crate::run;
use crate::ProjectKind;

pub(crate) fn extract_members(ws_toml: &toml::Value, ws_root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Some(members) = ws_toml
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for member in members {
            if let Some(pattern) = member.as_str() {
                let path = ws_root.join(pattern);
                if path.exists() {
                    result.push(path);
                }
            }
        }
    }
    if result.is_empty() {
        result.push(ws_root.to_path_buf());
    }
    result
}

pub(crate) fn index_packages(db: &Db, members: &[PathBuf], workspace: &Path, ws_name: &str) {
    for member_path in members {
        let cargo_toml = member_path.join("Cargo.toml");
        let Ok(content) = std::fs::read_to_string(&cargo_toml) else {
            continue;
        };
        let Ok(pkg_toml) = content.parse::<toml::Value>() else {
            continue;
        };

        let name = pkg_toml
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        let version = pkg_toml
            .get("package")
            .and_then(|p| p.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0");
        let edition = pkg_toml
            .get("package")
            .and_then(|p| p.get("edition"))
            .and_then(|v| v.as_str())
            .unwrap_or("2021");
        let rel_path = member_path
            .strip_prefix(workspace)
            .unwrap_or(member_path)
            .to_string_lossy()
            .to_string();

        run(
            db,
            &format!(
                "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = {pa}, p.language = 'Rust', p.edition = {e}, p.is_external = false",
                n = escape_str(name),
                v = escape_str(version),
                pa = escape_str(&rel_path),
                e = escape_str(edition),
            ),
        );
        run(
            db,
            &format!(
                "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
                ws = escape_str(ws_name),
                n = escape_str(name),
            ),
        );

        for dep_key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = pkg_toml.get(dep_key).and_then(|d| d.as_table()) {
                let kind = match dep_key {
                    "dev-dependencies" => "Dev",
                    "build-dependencies" => "Build",
                    _ => "Normal",
                };
                for (dep_name, dep_val) in deps {
                    let is_ws = dep_val.get("path").is_some()
                        || dep_val
                            .get("workspace")
                            .and_then(|w| w.as_bool())
                            .unwrap_or(false);
                    if !is_ws {
                        run(
                            db,
                            &format!(
                                "MERGE (ext:Package {{name: {n}}}) SET ext.is_external = true, ext.language = 'Rust'",
                                n = escape_str(dep_name),
                            ),
                        );
                    }
                    run(
                        db,
                        &format!(
                            "MATCH (a:Package {{name: {an}}}), (b:Package {{name: {bn}}}) CREATE (a)-[:DEPENDS_ON {{kind: {k}}}]->(b)",
                            an = escape_str(name),
                            bn = escape_str(dep_name),
                            k = escape_str(kind),
                        ),
                    );
                }
            }
        }
        eprintln!("  [+] Package: {} ({})", name, rel_path);
    }
}

pub(crate) fn collect_source_files(
    members: &[PathBuf],
    workspace: &Path,
    kind: ProjectKind,
) -> Vec<(PathBuf, String, String)> {
    let mut files = Vec::new();
    let extensions = kind.extensions();

    for member_path in members {
        let (src_dirs, pkg_name) = match kind {
            ProjectKind::Rust => {
                let src = member_path.join("src");
                let tests = member_path.join("tests");
                let name = std::fs::read_to_string(member_path.join("Cargo.toml"))
                    .ok()
                    .and_then(|c| c.parse::<toml::Value>().ok())
                    .and_then(|t| t.get("package")?.get("name")?.as_str().map(String::from))
                    .unwrap_or_default();
                let mut dirs = vec![src];
                if tests.is_dir() {
                    dirs.push(tests);
                }
                (dirs, name)
            }
            ProjectKind::Node => {
                let name = std::fs::read_to_string(member_path.join("package.json"))
                    .ok()
                    .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                    .and_then(|v| v.get("name")?.as_str().map(String::from))
                    .unwrap_or_else(|| {
                        member_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    });
                let mut dirs = vec![];
                for d in ["src", "lib", "app", "pages", "components", "."] {
                    let p = member_path.join(d);
                    if p.exists() {
                        dirs.push(p);
                        break;
                    }
                }
                if dirs.is_empty() {
                    dirs.push(member_path.clone());
                }
                (dirs, name)
            }
            ProjectKind::Go => {
                let name = go_module_name(&member_path.join("go.mod")).unwrap_or_else(|| {
                    member_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                });
                // Go layout convention: source lives at the module root and in
                // arbitrary subdirectories. Walk the whole module.
                (vec![member_path.clone()], name)
            }
            ProjectKind::Python => {
                let name = std::fs::read_to_string(member_path.join("pyproject.toml"))
                    .ok()
                    .and_then(|c| c.parse::<toml::Value>().ok())
                    .and_then(|t| {
                        t.get("project")
                            .and_then(|p| p.get("name"))
                            .or_else(|| {
                                t.get("tool")
                                    .and_then(|t| t.get("poetry"))
                                    .and_then(|p| p.get("name"))
                            })
                            .and_then(|n| n.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| {
                        member_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    });
                let mut dirs = vec![];
                for d in ["src", "lib", "app", "."] {
                    let p = member_path.join(d);
                    if p.exists() {
                        dirs.push(p);
                        break;
                    }
                }
                if dirs.is_empty() {
                    dirs.push(member_path.clone());
                }
                (dirs, name)
            }
        };

        let skip_dirs = [
            "node_modules",
            ".git",
            "dist",
            "build",
            "target",
            "__pycache__",
            ".venv",
            "venv",
            ".tox",
            ".mypy_cache",
            ".pytest_cache",
            "egg-info",
            ".eggs",
            "vendor",
        ];

        for src_dir in &src_dirs {
            if !src_dir.exists() {
                continue;
            }
            for entry in WalkDir::new(src_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    !path.components().any(|c| {
                        let s = c.as_os_str().to_string_lossy();
                        skip_dirs.iter().any(|d| s.as_ref() == *d || s.ends_with(d))
                    }) && e.path().extension().is_some_and(|ext| {
                        let ext_str = ext.to_string_lossy();
                        extensions.iter().any(|e| *e == ext_str.as_ref())
                    })
                })
            {
                let abs = entry.path().to_path_buf();
                let rel = abs
                    .strip_prefix(workspace)
                    .unwrap_or(&abs)
                    .to_string_lossy()
                    .to_string();
                files.push((abs, rel, pkg_name.clone()));
            }
        }
    }
    files
}

pub(crate) fn index_node_packages(db: &Db, workspace: &Path, ws_name: &str) {
    let pkg_path = workspace.join("package.json");
    let Ok(content) = std::fs::read_to_string(&pkg_path) else {
        eprintln!("  [!] Cannot read package.json");
        return;
    };
    let Ok(pkg): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
        eprintln!("  [!] Cannot parse package.json");
        return;
    };

    let name = pkg
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("unknown");
    let version = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");

    run(
        db,
        &format!(
            "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = '.', p.language = 'TypeScript', p.is_external = false",
            n = escape_str(name),
            v = escape_str(version),
        ),
    );
    run(
        db,
        &format!(
            "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
            ws = escape_str(ws_name),
            n = escape_str(name),
        ),
    );

    for (dep_key, kind) in [
        ("dependencies", "Normal"),
        ("devDependencies", "Dev"),
        ("peerDependencies", "Normal"),
    ] {
        if let Some(deps) = pkg.get(dep_key).and_then(|d| d.as_object()) {
            for dep_name in deps.keys() {
                run(
                    db,
                    &format!(
                        "MERGE (ext:Package {{name: {n}}}) SET ext.is_external = true, ext.language = 'TypeScript'",
                        n = escape_str(dep_name),
                    ),
                );
                run(
                    db,
                    &format!(
                        "MATCH (a:Package {{name: {an}}}), (b:Package {{name: {bn}}}) CREATE (a)-[:DEPENDS_ON {{kind: {k}}}]->(b)",
                        an = escape_str(name),
                        bn = escape_str(dep_name),
                        k = escape_str(kind),
                    ),
                );
            }
        }
    }

    if let Some(workspaces) = pkg.get("workspaces").and_then(|w| w.as_array()) {
        for ws_pattern in workspaces {
            if let Some(pattern) = ws_pattern.as_str() {
                let base = pattern.trim_end_matches("/*");
                let base_path = workspace.join(base);
                if !base_path.is_dir() {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(&base_path) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let sub_pkg = entry.path().join("package.json");
                    if !sub_pkg.exists() {
                        continue;
                    }
                    let Ok(sub_content) = std::fs::read_to_string(&sub_pkg) else {
                        continue;
                    };
                    let Ok(sub_json): Result<serde_json::Value, _> =
                        serde_json::from_str(&sub_content)
                    else {
                        continue;
                    };
                    let sub_name = sub_json
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown");
                    let sub_version = sub_json
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0.0.0");
                    let rel_path = entry
                        .path()
                        .strip_prefix(workspace)
                        .unwrap_or(&entry.path())
                        .to_string_lossy()
                        .to_string();
                    run(
                        db,
                        &format!(
                            "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = {pa}, p.language = 'TypeScript', p.is_external = false",
                            n = escape_str(sub_name),
                            v = escape_str(sub_version),
                            pa = escape_str(&rel_path),
                        ),
                    );
                    run(
                        db,
                        &format!(
                            "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
                            ws = escape_str(ws_name),
                            n = escape_str(sub_name),
                        ),
                    );
                    eprintln!("  [+] Package: {} ({})", sub_name, rel_path);
                }
            }
        }
    }
    eprintln!("  [+] Package: {} (.)", name);
}

/// Extract the `module` declaration from a `go.mod`.
///
/// Format spec: <https://go.dev/ref/mod#go-mod-file-module>. We only care
/// about the single-line form (`module <path>`) which covers ~all
/// real-world go.mod files; the parenthesised block form is technically
/// allowed but virtually never used.
pub(crate) fn go_module_name(go_mod: &Path) -> Option<String> {
    let content = std::fs::read_to_string(go_mod).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("module ") {
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

pub(crate) fn index_go_packages(db: &Db, workspace: &Path, ws_name: &str) {
    let go_mod = workspace.join("go.mod");
    let Some(module) = go_module_name(&go_mod) else {
        eprintln!("  [!] Cannot read module name from go.mod");
        return;
    };

    // The `go` directive (line like `go 1.22`) doubles as a version-ish
    // marker — Go modules don't carry their own version on disk; tags are
    // upstream. Surface it as the package version so `Package.version` is
    // populated.
    let go_version = std::fs::read_to_string(&go_mod)
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.trim().strip_prefix("go ").map(|v| v.trim().to_string()))
        })
        .unwrap_or_else(|| "0.0.0".to_string());

    run(
        db,
        &format!(
            "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = '.', p.language = 'Go', p.is_external = false",
            n = escape_str(&module),
            v = escape_str(&go_version),
        ),
    );
    run(
        db,
        &format!(
            "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
            ws = escape_str(ws_name),
            n = escape_str(&module),
        ),
    );

    // Parse `require` directives — both single-line and parenthesised
    // blocks. Each entry: `<module-path> <version>` plus optional
    // `// indirect` trailing comment. We keep external modules as
    // `:Package {is_external: true}` and link with `[:DEPENDS_ON]`.
    if let Ok(content) = std::fs::read_to_string(&go_mod) {
        let mut in_require = false;
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            if line.starts_with("require (") {
                in_require = true;
                continue;
            }
            if in_require && line.starts_with(')') {
                in_require = false;
                continue;
            }
            let entry = if let Some(rest) = line.strip_prefix("require ") {
                Some(rest.trim_start_matches('(').trim())
            } else if in_require {
                Some(line)
            } else {
                None
            };
            let Some(entry) = entry else { continue };
            // Drop trailing `// indirect` etc.
            let head = entry.split("//").next().unwrap_or("").trim();
            let mut parts = head.split_whitespace();
            let Some(dep_name) = parts.next() else {
                continue;
            };
            run(
                db,
                &format!(
                    "MERGE (ext:Package {{name: {n}}}) SET ext.is_external = true, ext.language = 'Go'",
                    n = escape_str(dep_name),
                ),
            );
            run(
                db,
                &format!(
                    "MATCH (a:Package {{name: {an}}}), (b:Package {{name: {bn}}}) CREATE (a)-[:DEPENDS_ON {{kind: 'Normal'}}]->(b)",
                    an = escape_str(&module),
                    bn = escape_str(dep_name),
                ),
            );
        }
    }

    eprintln!("  [+] Package: {} (.)", module);
}

pub(crate) fn index_python_packages(db: &Db, workspace: &Path, ws_name: &str) {
    let pyproject_path = workspace.join("pyproject.toml");
    let reqs_path = workspace.join("requirements.txt");

    if let Ok(content) = std::fs::read_to_string(&pyproject_path) {
        if let Ok(toml) = content.parse::<toml::Value>() {
            let name = toml
                .get("project")
                .and_then(|p| p.get("name"))
                .or_else(|| {
                    toml.get("tool")
                        .and_then(|t| t.get("poetry"))
                        .and_then(|p| p.get("name"))
                })
                .and_then(|n| n.as_str())
                .unwrap_or_else(|| {
                    workspace
                        .file_name()
                        .unwrap_or_default()
                        .to_str()
                        .unwrap_or("unknown")
                });
            let version = toml
                .get("project")
                .and_then(|p| p.get("version"))
                .or_else(|| {
                    toml.get("tool")
                        .and_then(|t| t.get("poetry"))
                        .and_then(|p| p.get("version"))
                })
                .and_then(|v| v.as_str())
                .unwrap_or("0.0.0");

            run(
                db,
                &format!(
                    "MERGE (p:Package {{name: {n}}}) SET p.version = {v}, p.path = '.', p.language = 'Python', p.is_external = false",
                    n = escape_str(name),
                    v = escape_str(version),
                ),
            );
            run(
                db,
                &format!(
                    "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
                    ws = escape_str(ws_name),
                    n = escape_str(name),
                ),
            );

            if let Some(deps) = toml
                .get("project")
                .and_then(|p| p.get("dependencies"))
                .and_then(|d| d.as_array())
            {
                for dep in deps {
                    if let Some(dep_str) = dep.as_str() {
                        let dep_name = dep_str
                            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                            .next()
                            .unwrap_or(dep_str);
                        emit_python_dep(db, name, dep_name, "Normal");
                    }
                }
            }

            if let Some(deps) = toml
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.get("dependencies"))
                .and_then(|d| d.as_table())
            {
                for dep_name in deps.keys() {
                    if dep_name == "python" {
                        continue;
                    }
                    emit_python_dep(db, name, dep_name, "Normal");
                }
            }

            if let Some(deps) = toml
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.get("group"))
                .and_then(|g| g.get("dev"))
                .and_then(|d| d.get("dependencies"))
                .and_then(|d| d.as_table())
            {
                for dep_name in deps.keys() {
                    emit_python_dep(db, name, dep_name, "Dev");
                }
            }

            eprintln!("  [+] Package: {} (.) via pyproject.toml", name);
            return;
        }
    }

    if let Ok(content) = std::fs::read_to_string(&reqs_path) {
        let name = workspace
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        run(
            db,
            &format!(
                "MERGE (p:Package {{name: {n}}}) SET p.path = '.', p.language = 'Python', p.is_external = false",
                n = escape_str(&name),
            ),
        );
        run(
            db,
            &format!(
                "MATCH (w:Workspace {{name: {ws}}}), (p:Package {{name: {n}}}) CREATE (w)-[:CONTAINS]->(p)",
                ws = escape_str(ws_name),
                n = escape_str(&name),
            ),
        );
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
                continue;
            }
            let dep_name = line
                .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .next()
                .unwrap_or(line);
            if dep_name.is_empty() {
                continue;
            }
            emit_python_dep(db, &name, dep_name, "Normal");
        }
        eprintln!("  [+] Package: {} (.) via requirements.txt", name);
    }
}

fn emit_python_dep(db: &Db, pkg_name: &str, dep_name: &str, kind: &str) {
    run(
        db,
        &format!(
            "MERGE (ext:Package {{name: {n}}}) SET ext.is_external = true, ext.language = 'Python'",
            n = escape_str(dep_name),
        ),
    );
    run(
        db,
        &format!(
            "MATCH (a:Package {{name: {an}}}), (b:Package {{name: {bn}}}) CREATE (a)-[:DEPENDS_ON {{kind: {k}}}]->(b)",
            an = escape_str(pkg_name),
            bn = escape_str(dep_name),
            k = escape_str(kind),
        ),
    );
}
