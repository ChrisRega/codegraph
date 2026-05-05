//! BDD coverage visualiser — renders an interactive HTML graph of
//!
//!   Package → Feature → Scenario → Step → Function
//!
//! from a codegraph database (velr-backed). Open the resulting HTML in any
//! browser; nodes / edges are styled with cytoscape.js + dagre.
//!
//! Usage:
//!   bdd-viz --db code-graph.db --out bdd.html

use std::collections::{BTreeMap, HashMap};

use codegraph_core::{Db, Table};
use serde_json::{json, Map, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db_path = flag(&args, "--db").unwrap_or_else(|| "code-graph.db".to_string());
    let out_path = flag(&args, "--out").unwrap_or_else(|| "bdd.html".to_string());

    let db = Db::open(&db_path).unwrap_or_else(|e| {
        eprintln!("cannot open {db_path}: {e}");
        std::process::exit(1);
    });

    let pkg_file_rows = query(
        &db,
        "MATCH (p:Package)-[:CONTAINS]->(f:File) RETURN p.name AS pkg, f.path AS path",
    );
    let pkgs = pkg_file_rows.column_strings("pkg");
    let paths = pkg_file_rows.column_strings("path");
    let mut file_to_pkg: HashMap<String, String> = HashMap::new();
    for (p, f) in pkgs.iter().zip(paths.iter()) {
        file_to_pkg.insert(f.clone(), p.clone());
    }

    let feat_rows = query(
        &db,
        "MATCH (feat:Feature) RETURN feat.qualified_name AS qn, feat.name AS name, feat.file_path AS fp",
    );
    let feat_qns = feat_rows.column_strings("qn");
    let feat_names = feat_rows.column_strings("name");
    let feat_fps = feat_rows.column_strings("fp");

    let sc_rows = query(
        &db,
        "MATCH (f:Feature)-[:HAS_SCENARIO]->(sc:Scenario) RETURN f.qualified_name AS fqn, sc.qualified_name AS qn, sc.name AS name",
    );
    let sc_fqns = sc_rows.column_strings("fqn");
    let sc_qns = sc_rows.column_strings("qn");
    let sc_names = sc_rows.column_strings("name");

    let step_rows = query(
        &db,
        "MATCH (sc:Scenario)-[:HAS_STEP]->(st:Step) RETURN sc.qualified_name AS scqn, st.qualified_name AS qn, st.kind AS kind, st.text AS text, st.step_order AS ord",
    );
    let st_scqns = step_rows.column_strings("scqn");
    let st_qns = step_rows.column_strings("qn");
    let st_kinds = step_rows.column_strings("kind");
    let st_texts = step_rows.column_strings("text");
    let _st_orders = col_ints(&step_rows, "ord");

    let link_rows = query(
        &db,
        "MATCH (st:Step)-[:IMPLEMENTED_BY]->(fn:Function) RETURN st.qualified_name AS stqn, fn.qualified_name AS fnqn, fn.name AS name",
    );
    let lk_st = link_rows.column_strings("stqn");
    let lk_fn = link_rows.column_strings("fnqn");
    let lk_fnnames = link_rows.column_strings("name");

    let mut nodes: Vec<Value> = Vec::new();
    let mut edges: Vec<Value> = Vec::new();
    let mut seen_nodes: BTreeMap<String, ()> = BTreeMap::new();
    let push_node = |id: &str,
                     label: &str,
                     kind: &str,
                     extra: Map<String, Value>,
                     buf: &mut Vec<Value>,
                     seen: &mut BTreeMap<String, ()>| {
        if seen.contains_key(id) {
            return;
        }
        seen.insert(id.to_string(), ());
        let mut data = Map::new();
        data.insert("id".into(), Value::String(id.to_string()));
        data.insert("label".into(), Value::String(label.to_string()));
        data.insert("kind".into(), Value::String(kind.to_string()));
        for (k, v) in extra {
            data.insert(k, v);
        }
        buf.push(json!({"data": data}));
    };
    let push_edge = |src: &str, tgt: &str, kind: &str, buf: &mut Vec<Value>| {
        buf.push(json!({"data": {
            "id": format!("{kind}:{src}->{tgt}"),
            "source": src,
            "target": tgt,
            "kind": kind,
        }}));
    };

    let mut packages_used: BTreeMap<String, ()> = BTreeMap::new();

    for (qn, (name, fp)) in feat_qns.iter().zip(feat_names.iter().zip(feat_fps.iter())) {
        let pkg = file_to_pkg.get(fp).cloned().unwrap_or_else(|| pkg_from_path_fallback(fp));
        let pkg_id = format!("pkg::{pkg}");
        if !packages_used.contains_key(&pkg_id) {
            packages_used.insert(pkg_id.clone(), ());
            push_node(&pkg_id, &pkg, "Package", Map::new(), &mut nodes, &mut seen_nodes);
        }
        let feat_id = format!("feat::{qn}");
        let mut extra = Map::new();
        extra.insert("file_path".into(), Value::String(fp.clone()));
        push_node(&feat_id, name, "Feature", extra, &mut nodes, &mut seen_nodes);
        push_edge(&pkg_id, &feat_id, "HAS_FEATURE", &mut edges);
    }

    for (fqn, (qn, name)) in sc_fqns.iter().zip(sc_qns.iter().zip(sc_names.iter())) {
        let feat_id = format!("feat::{fqn}");
        let sc_id = format!("sc::{qn}");
        push_node(&sc_id, name, "Scenario", Map::new(), &mut nodes, &mut seen_nodes);
        push_edge(&feat_id, &sc_id, "HAS_SCENARIO", &mut edges);
    }

    for (((scqn, qn), kind), text) in
        st_scqns.iter().zip(st_qns.iter()).zip(st_kinds.iter()).zip(st_texts.iter())
    {
        let sc_id = format!("sc::{scqn}");
        let st_id = format!("st::{qn}");
        let mut extra = Map::new();
        extra.insert("text".into(), Value::String(text.clone()));
        extra.insert("step_kind".into(), Value::String(kind.clone()));
        push_node(
            &st_id,
            &format!("{kind} {}", truncate(text, 60)),
            "Step",
            extra,
            &mut nodes,
            &mut seen_nodes,
        );
        push_edge(&sc_id, &st_id, "HAS_STEP", &mut edges);
    }

    for ((stqn, fnqn), fnname) in lk_st.iter().zip(lk_fn.iter()).zip(lk_fnnames.iter()) {
        let st_id = format!("st::{stqn}");
        let fn_id = format!("fn::{fnqn}");
        let mut extra = Map::new();
        extra.insert("qualified_name".into(), Value::String(fnqn.clone()));
        push_node(&fn_id, fnname, "Function", extra, &mut nodes, &mut seen_nodes);
        push_edge(&st_id, &fn_id, "IMPLEMENTED_BY", &mut edges);
    }

    let graph = json!({"nodes": nodes, "edges": edges});
    let html = render_html(&graph);
    std::fs::write(&out_path, html).expect("write output");
    eprintln!(
        "  wrote {out_path}: {} nodes / {} edges",
        graph["nodes"].as_array().map(|a| a.len()).unwrap_or(0),
        graph["edges"].as_array().map(|a| a.len()).unwrap_or(0),
    );
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn query(db: &Db, cypher: &str) -> Table {
    db.query(cypher).unwrap_or_else(|e| {
        eprintln!("query failed: {e}\n  {cypher}");
        Table { columns: Vec::new(), rows: Vec::new() }
    })
}

fn col_ints(t: &Table, alias: &str) -> Vec<i64> {
    let Some(idx) = t.col(alias) else { return Vec::new() };
    t.rows.iter().filter_map(|r| r.get(idx).and_then(|c| c.as_i64())).collect()
}

fn pkg_from_path_fallback(path: &str) -> String {
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() >= 2 && segs[0] == "crates" {
        segs[1].to_string()
    } else {
        "(unknown)".to_string()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn render_html(graph: &Value) -> String {
    let data_json = serde_json::to_string(graph).unwrap();
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>BDD Coverage — Package → Feature → Scenario → Step → Function</title>
<script src="https://unpkg.com/cytoscape@3.30.2/dist/cytoscape.min.js"></script>
<script src="https://unpkg.com/dagre@0.8.5/dist/dagre.min.js"></script>
<script src="https://unpkg.com/cytoscape-dagre@2.5.0/cytoscape-dagre.js"></script>
<style>
  html, body {{ margin: 0; padding: 0; height: 100%; font-family: ui-sans-serif, system-ui, sans-serif; background: #0e1016; color: #e6e6e6; }}
  #cy {{ width: 100%; height: calc(100vh - 52px); background: #0e1016; }}
  #bar {{ height: 52px; padding: 8px 12px; display: flex; align-items: center; gap: 12px; background: #13151d; border-bottom: 1px solid #24262f; }}
  #bar h1 {{ font-size: 13px; font-weight: 600; margin: 0; color: #cfd3dc; }}
  #bar .legend {{ display: flex; gap: 10px; font-size: 11px; color: #9aa0aa; }}
  .dot {{ display: inline-block; width: 10px; height: 10px; border-radius: 50%; margin-right: 4px; vertical-align: middle; }}
  #search {{ flex: 1; padding: 5px 8px; background: #1a1d27; border: 1px solid #2a2e3b; border-radius: 4px; color: #e6e6e6; }}
  #info {{ position: absolute; right: 12px; top: 64px; width: 320px; max-height: 40vh; overflow: auto; background: #13151d; border: 1px solid #2a2e3b; border-radius: 6px; padding: 10px 12px; font-size: 12px; display: none; }}
  #info h3 {{ margin: 0 0 6px 0; font-size: 13px; }}
  #info .kv {{ color: #9aa0aa; }}
  #info code {{ color: #e6e6e6; background: #1a1d27; padding: 1px 4px; border-radius: 3px; }}
</style>
</head>
<body>
<div id="bar">
  <h1>BDD Coverage</h1>
  <input id="search" type="text" placeholder="Filter by label (case-insensitive)…" />
  <div class="legend">
    <span><span class="dot" style="background:#7e6af0"></span>Package</span>
    <span><span class="dot" style="background:#4a9eff"></span>Feature</span>
    <span><span class="dot" style="background:#43c4a0"></span>Scenario</span>
    <span><span class="dot" style="background:#e4a94a"></span>Step</span>
    <span><span class="dot" style="background:#e4725a"></span>Function</span>
  </div>
</div>
<div id="cy"></div>
<div id="info"></div>
<script>
const GRAPH = {data_json};

const cy = cytoscape({{
  container: document.getElementById('cy'),
  elements: {{ nodes: GRAPH.nodes, edges: GRAPH.edges }},
  style: [
    {{ selector: 'node', style: {{
      'label': 'data(label)',
      'font-size': 10,
      'color': '#e6e6e6',
      'text-wrap': 'ellipsis',
      'text-max-width': '180px',
      'text-valign': 'center',
      'text-halign': 'center',
      'background-color': '#555',
      'border-width': 1,
      'border-color': '#222',
      'width': 'label',
      'height': 24,
      'padding': '6px',
      'shape': 'round-rectangle',
    }} }},
    {{ selector: 'node[kind = "Package"]',  style: {{ 'background-color': '#7e6af0', 'font-weight': 700 }} }},
    {{ selector: 'node[kind = "Feature"]',  style: {{ 'background-color': '#4a9eff' }} }},
    {{ selector: 'node[kind = "Scenario"]', style: {{ 'background-color': '#43c4a0' }} }},
    {{ selector: 'node[kind = "Step"]',     style: {{ 'background-color': '#e4a94a', 'color': '#1a1208' }} }},
    {{ selector: 'node[kind = "Function"]', style: {{ 'background-color': '#e4725a' }} }},
    {{ selector: 'edge', style: {{
      'width': 1,
      'line-color': '#3a3d48',
      'target-arrow-color': '#3a3d48',
      'target-arrow-shape': 'triangle',
      'curve-style': 'bezier',
      'arrow-scale': 0.7,
    }} }},
    {{ selector: 'edge[kind = "IMPLEMENTED_BY"]', style: {{
      'line-color': '#e4725a',
      'target-arrow-color': '#e4725a',
      'line-style': 'dashed'
    }} }},
    {{ selector: ':selected', style: {{ 'border-color': '#fff', 'border-width': 2 }} }},
    {{ selector: '.dim', style: {{ 'opacity': 0.1 }} }},
  ],
  layout: {{
    name: 'dagre',
    rankDir: 'LR',
    nodeSep: 12,
    rankSep: 80,
    edgeSep: 6,
  }},
  wheelSensitivity: 0.25,
}});

const info = document.getElementById('info');
cy.on('tap', 'node', evt => {{
  const d = evt.target.data();
  let extra = '';
  for (const k of Object.keys(d)) {{
    if (['id','label','kind'].includes(k)) continue;
    extra += `<div><span class="kv">${{k}}:</span> <code>${{escapeHtml(String(d[k]))}}</code></div>`;
  }}
  info.innerHTML = `<h3>${{d.kind}} — ${{escapeHtml(d.label)}}</h3>${{extra}}`;
  info.style.display = 'block';
}});
cy.on('tap', e => {{ if (e.target === cy) info.style.display = 'none'; }});

document.getElementById('search').addEventListener('input', e => {{
  const q = e.target.value.trim().toLowerCase();
  if (!q) {{ cy.elements().removeClass('dim'); return; }}
  const matches = cy.nodes().filter(n => (n.data('label') || '').toLowerCase().includes(q));
  const keep = matches.union(matches.closedNeighborhood());
  cy.elements().addClass('dim');
  keep.removeClass('dim');
}});

function escapeHtml(s) {{ return s.replace(/[&<>"']/g, c => ({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c])); }}
</script>
</body>
</html>
"##,
        data_json = data_json,
    )
}
