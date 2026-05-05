//! API specification reader — indexes OpenAPI, GraphQL SDL, and Protobuf specs
//! into the code graph as APIEndpoint / APIType nodes.

use std::path::Path;

use codegraph_core::{escape_str, Db};
use walkdir::WalkDir;

pub fn index_api_specs(db: &Db, workspace: &Path, pkg_name: &str) -> (u32, u32) {
    let mut endpoints = 0u32;
    let mut types = 0u32;

    for entry in WalkDir::new(workspace).into_iter().filter_map(|e| e.ok()).filter(|e| {
        e.file_type().is_file()
            && !e.path().components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                s == "node_modules" || s == ".git" || s == "target" || s == "dist" || s == ".venv"
            })
    }) {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let ext = path.extension().unwrap_or_default().to_string_lossy();
        let rel = path.strip_prefix(workspace).unwrap_or(path).to_string_lossy().to_string();

        if is_openapi_file(&name, &ext) {
            let (e, t) = index_openapi(db, path, &rel, pkg_name);
            endpoints += e;
            types += t;
        } else if ext == "graphqls" || ext == "gql" || name == "schema.graphql" {
            let t = index_graphql_sdl(db, path, &rel, pkg_name);
            types += t;
        } else if ext == "proto" {
            let (e, t) = index_protobuf(db, path, &rel, pkg_name);
            endpoints += e;
            types += t;
        }
    }

    if endpoints + types > 0 {
        eprintln!("  [+] API specs: {endpoints} endpoints, {types} types");
    }
    (endpoints, types)
}

fn is_openapi_file(name: &str, _ext: &str) -> bool {
    let n = name.to_lowercase();
    n == "openapi.yaml"
        || n == "openapi.yml"
        || n == "openapi.json"
        || n == "swagger.yaml"
        || n == "swagger.yml"
        || n == "swagger.json"
        || n.ends_with(".openapi.yaml")
        || n.ends_with(".openapi.yml")
        || n.ends_with(".openapi.json")
}

fn index_openapi(db: &Db, path: &Path, rel_path: &str, pkg_name: &str) -> (u32, u32) {
    let Ok(content) = std::fs::read_to_string(path) else { return (0, 0) };
    let spec: serde_json::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  [!] OpenAPI parse error: {} — {e}", rel_path);
            return (0, 0);
        }
    };

    let file_lit = escape_str(rel_path);
    let pkg_lit = escape_str(pkg_name);
    let mut endpoints = 0u32;
    let mut types = 0u32;

    if let Some(paths) = spec.get("paths").and_then(|p| p.as_object()) {
        for (path_str, methods) in paths {
            let Some(methods) = methods.as_object() else { continue };
            for (method, op) in methods {
                let method_upper = method.to_uppercase();
                if !["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"]
                    .contains(&method_upper.as_str())
                {
                    continue;
                }
                let op_id = op.get("operationId").and_then(|v| v.as_str()).unwrap_or("");
                let summary = op.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                let tags = op
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")
                    })
                    .unwrap_or_default();

                run(db, &format!(
                    "CREATE (ep:APIEndpoint {{method: {m}, path: {p}, operationId: {o}, summary: {s}, tags: {t}, spec_file: {file_lit}}})",
                    m = escape_str(&method_upper),
                    p = escape_str(path_str),
                    o = escape_str(op_id),
                    s = escape_str(summary),
                    t = escape_str(&tags),
                ));
                run(db, &format!(
                    "MATCH (p:Package {{name: {pkg_lit}}}), (ep:APIEndpoint {{operationId: {o}, spec_file: {file_lit}}}) CREATE (p)-[:EXPOSES]->(ep)",
                    o = escape_str(op_id),
                ));
                endpoints += 1;

                for schema_ref in extract_schema_refs(op) {
                    let type_name = schema_ref.rsplit('/').next().unwrap_or(&schema_ref);
                    run(db, &format!(
                        "MERGE (t:APIType {{name: {n}, spec_file: {file_lit}}})",
                        n = escape_str(type_name),
                    ));
                    run(db, &format!(
                        "MATCH (ep:APIEndpoint {{operationId: {o}, spec_file: {file_lit}}}), (t:APIType {{name: {n}, spec_file: {file_lit}}}) CREATE (ep)-[:USES_SCHEMA]->(t)",
                        o = escape_str(op_id),
                        n = escape_str(type_name),
                    ));
                }
            }
        }
    }

    let schemas = spec
        .get("components")
        .and_then(|c| c.get("schemas"))
        .or_else(|| spec.get("definitions"))
        .and_then(|s| s.as_object());

    if let Some(schemas) = schemas {
        for (type_name, schema) in schemas {
            let kind = schema.get("type").and_then(|v| v.as_str()).unwrap_or("object");
            run(db, &format!(
                "MERGE (t:APIType {{name: {n}, spec_file: {file_lit}}}) SET t.kind = {k}",
                n = escape_str(type_name),
                k = escape_str(kind),
            ));

            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                for (i, (field_name, field_def)) in props.iter().enumerate() {
                    let field_type = field_def
                        .get("type")
                        .and_then(|v| v.as_str())
                        .or_else(|| {
                            field_def
                                .get("$ref")
                                .and_then(|v| v.as_str())
                                .map(|r| r.rsplit('/').next().unwrap_or(r))
                        })
                        .unwrap_or("any");
                    run(db, &format!(
                        "MATCH (t:APIType {{name: {tn}, spec_file: {file_lit}}}) CREATE (t)-[:HAS_FIELD]->(:Field {{name: {fn_l}, type_name: {ft}, kind: 'Named', index: {i}}})",
                        tn = escape_str(type_name),
                        fn_l = escape_str(field_name),
                        ft = escape_str(field_type),
                    ));
                }
            }
            types += 1;
        }
    }

    eprintln!("  [+] OpenAPI: {} — {endpoints} endpoints, {types} schemas", rel_path);
    (endpoints, types)
}

fn extract_schema_refs(val: &serde_json::Value) -> Vec<String> {
    let mut refs = Vec::new();
    match val {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(r)) = map.get("$ref") {
                refs.push(r.clone());
            }
            for v in map.values() {
                refs.extend(extract_schema_refs(v));
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                refs.extend(extract_schema_refs(v));
            }
        }
        _ => {}
    }
    refs
}

fn index_graphql_sdl(db: &Db, path: &Path, rel_path: &str, pkg_name: &str) -> u32 {
    let Ok(content) = std::fs::read_to_string(path) else { return 0 };

    let file_lit = escape_str(rel_path);
    let pkg_lit = escape_str(pkg_name);
    let mut types = 0u32;

    for line in content.lines() {
        let trimmed = line.trim();
        for keyword in ["type", "input", "enum", "interface", "union", "scalar"] {
            if trimmed.starts_with(keyword) && trimmed.len() > keyword.len() {
                let rest = trimmed[keyword.len()..].trim();
                let type_name = rest
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                if type_name.is_empty() || type_name == "{" {
                    continue;
                }
                let kind = match keyword {
                    "type" => "ObjectType",
                    "input" => "InputType",
                    "enum" => "Enum",
                    "interface" => "Interface",
                    "union" => "Union",
                    "scalar" => "Scalar",
                    _ => "Other",
                };
                run(db, &format!(
                    "MERGE (t:APIType {{name: {n}, spec_file: {file_lit}}}) SET t.kind = {k}",
                    n = escape_str(type_name),
                    k = escape_str(kind),
                ));
                run(db, &format!(
                    "MATCH (p:Package {{name: {pkg_lit}}}), (t:APIType {{name: {n}, spec_file: {file_lit}}}) CREATE (p)-[:EXPOSES]->(t)",
                    n = escape_str(type_name),
                ));
                types += 1;
                break;
            }
        }
    }

    if types > 0 {
        eprintln!("  [+] GraphQL: {} — {types} types", rel_path);
    }
    types
}

fn index_protobuf(db: &Db, path: &Path, rel_path: &str, pkg_name: &str) -> (u32, u32) {
    let Ok(content) = std::fs::read_to_string(path) else { return (0, 0) };

    let file_lit = escape_str(rel_path);
    let pkg_lit = escape_str(pkg_name);
    let mut endpoints = 0u32;
    let mut types = 0u32;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("service ") {
            let name = trimmed
                .strip_prefix("service ")
                .unwrap_or("")
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if !name.is_empty() {
                run(db, &format!(
                    "MERGE (t:APIType {{name: {n}, spec_file: {file_lit}}}) SET t.kind = 'Service'",
                    n = escape_str(name),
                ));
                run(db, &format!(
                    "MATCH (p:Package {{name: {pkg_lit}}}), (t:APIType {{name: {n}, spec_file: {file_lit}}}) CREATE (p)-[:EXPOSES]->(t)",
                    n = escape_str(name),
                ));
                types += 1;
            }
        }

        if trimmed.starts_with("rpc ") {
            let rest = trimmed.strip_prefix("rpc ").unwrap_or("").trim();
            let method_name = rest.split('(').next().unwrap_or("").trim();
            if !method_name.is_empty() {
                run(db, &format!(
                    "CREATE (ep:APIEndpoint {{method: 'gRPC', path: {n}, operationId: {n}, spec_file: {file_lit}}})",
                    n = escape_str(method_name),
                ));
                endpoints += 1;
            }
        }

        if trimmed.starts_with("message ") {
            let name = trimmed
                .strip_prefix("message ")
                .unwrap_or("")
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if !name.is_empty() {
                run(db, &format!(
                    "MERGE (t:APIType {{name: {n}, spec_file: {file_lit}}}) SET t.kind = 'Message'",
                    n = escape_str(name),
                ));
                types += 1;
            }
        }

        if trimmed.starts_with("enum ") {
            let name = trimmed
                .strip_prefix("enum ")
                .unwrap_or("")
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            if !name.is_empty() {
                run(db, &format!(
                    "MERGE (t:APIType {{name: {n}, spec_file: {file_lit}}}) SET t.kind = 'Enum'",
                    n = escape_str(name),
                ));
                types += 1;
            }
        }
    }

    if endpoints + types > 0 {
        eprintln!("  [+] Protobuf: {} — {endpoints} RPCs, {types} types", rel_path);
    }
    (endpoints, types)
}

fn run(db: &Db, cypher: &str) {
    if let Err(e) = db.run(cypher) {
        eprintln!("  [!] API spec query failed: {}\n      {}", e, cypher);
    }
}
