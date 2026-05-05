//! Markdown indexer — projects every `.md` file into the same graph as the
//! code:
//!
//!   * `:Doc {path, title, line_count}` — one per file
//!   * `:DocSection {qualified_name, heading, level, line}` — one per
//!     `#` / `##` / … header. `qualified_name = "{path}#{heading-slug}"`
//!     so it survives across re-indexes.
//!   * `(:Doc)-[:HAS_SECTION]->(:DocSection)`
//!   * `(:DocSection)-[:MENTIONS]->(:Function|:Symbol)` — resolved
//!     backtick code-spans inside the section body.
//!   * `(:DocSection)-[:LINKS_TO]->(:File|:Doc)` — markdown links resolved
//!     against indexed files relative to the doc's own directory.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use codegraph_core::{escape_str, Db};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

pub fn index_markdown_files(
    db: &Db,
    workspace: &Path,
    is_full: bool,
) -> (usize, usize, usize, usize) {
    if is_full {
        let _ = db.run("MATCH (s:DocSection) DETACH DELETE s");
        let _ = db.run("MATCH (d:Doc) DETACH DELETE d");
    }

    let md_files = collect_markdown_files(workspace);
    if md_files.is_empty() {
        return (0, 0, 0, 0);
    }

    let qn_index = collect_qualified_names(db);
    let name_index = collect_short_names(db);
    let file_index = collect_file_paths(db);

    let mut docs = 0usize;
    let mut sections = 0usize;
    let mut mentions = 0usize;
    let mut links = 0usize;

    for (abs, rel) in &md_files {
        let Ok(src) = std::fs::read_to_string(abs) else { continue };
        let parsed = parse_markdown(&src);
        let title = parsed.title.unwrap_or_else(|| {
            abs.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| rel.clone())
        });
        let line_count = src.lines().count();
        let doc_qn = rel.clone();

        write_doc_node(db, &doc_qn, &title, rel, line_count);
        docs += 1;

        let abs_dir = abs.parent().unwrap_or(workspace);
        for sec in &parsed.sections {
            let section_qn = format!("{}#{}", rel, slug(&sec.heading));
            write_section_node(db, &section_qn, &sec.heading, sec.level, sec.line);
            link_doc_section(db, &doc_qn, &section_qn);
            sections += 1;

            for span in &sec.code_spans {
                if let Some(target_qn) = resolve_code_span(span, &qn_index, &name_index) {
                    write_mentions_edge(db, &section_qn, &target_qn);
                    mentions += 1;
                }
            }

            for href in &sec.links {
                if let Some(target_path) = resolve_link(href, abs_dir, workspace, &file_index) {
                    write_links_to_edge(db, &section_qn, &target_path);
                    links += 1;
                }
            }
        }
    }

    (docs, sections, mentions, links)
}

#[derive(Debug, Default)]
struct ParsedDoc {
    title: Option<String>,
    sections: Vec<ParsedSection>,
}

#[derive(Debug)]
struct ParsedSection {
    heading: String,
    level: u32,
    line: usize,
    code_spans: Vec<String>,
    links: Vec<String>,
}

fn parse_markdown(src: &str) -> ParsedDoc {
    let mut out = ParsedDoc::default();
    let line_for_offset = |offset: usize| src[..offset.min(src.len())].lines().count().max(1);

    let parser = Parser::new(src).into_offset_iter();
    let mut current: Option<ParsedSection> = None;
    let mut buf = String::new();
    let mut in_heading = false;
    let mut heading_level = 0u32;
    let mut heading_offset = 0usize;
    let mut in_code_block = false;
    let mut code_block_lang: Option<String> = None;

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                if let Some(sec) = current.take() {
                    out.sections.push(sec);
                }
                in_heading = true;
                heading_level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                heading_offset = range.start;
                buf.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                let line = line_for_offset(heading_offset);
                let heading = std::mem::take(&mut buf).trim().to_string();
                if heading_level == 1 && out.title.is_none() {
                    out.title = Some(heading.clone());
                }
                current = Some(ParsedSection {
                    heading,
                    level: heading_level,
                    line,
                    code_spans: Vec::new(),
                    links: Vec::new(),
                });
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                if let Some(sec) = current.as_mut() {
                    sec.links.push(dest_url.to_string());
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                code_block_lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        let s = lang.to_string();
                        if s.is_empty() { None } else { Some(s) }
                    }
                    CodeBlockKind::Indented => None,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                code_block_lang = None;
            }
            Event::Code(s) => {
                let span = s.trim().to_string();
                if !span.is_empty() {
                    if let Some(sec) = current.as_mut() {
                        sec.code_spans.push(span);
                    }
                }
            }
            Event::Start(Tag::Emphasis | Tag::Strong) => {}
            Event::Text(t) => {
                if in_heading {
                    buf.push_str(&t);
                }
                if in_code_block && code_block_lang.as_deref() == Some("rust") {
                    if let Some(sec) = current.as_mut() {
                        for hit in scan_rust_idents(&t) {
                            sec.code_spans.push(hit);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(sec) = current.take() {
        out.sections.push(sec);
    }
    out
}

fn scan_rust_idents(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let after_pub = trimmed
            .strip_prefix("pub(crate) fn ")
            .or_else(|| trimmed.strip_prefix("pub fn "))
            .or_else(|| trimmed.strip_prefix("fn "));
        if let Some(rest) = after_pub {
            let name: String =
                rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() {
                out.push(name);
            }
        }
    }
    out
}

// ── Resolution ───────────────────────────────────────────────────────────────

fn resolve_code_span(
    span: &str,
    qn_index: &HashSet<String>,
    name_index: &HashMap<String, String>,
) -> Option<String> {
    let s = span.trim().trim_matches('`');
    if s.is_empty() {
        return None;
    }
    if qn_index.contains(s) {
        return Some(s.to_string());
    }
    if let Some(qn) = name_index.get(s) {
        return Some(qn.clone());
    }
    None
}

fn resolve_link(
    href: &str,
    doc_dir: &Path,
    workspace: &Path,
    file_index: &HashSet<String>,
) -> Option<String> {
    if href.starts_with('#') {
        return None;
    }
    if href.starts_with("http://") || href.starts_with("https://") || href.starts_with("mailto:") {
        return None;
    }
    let path_only = href.split('#').next().unwrap_or(href);
    let path_only = path_only.split('?').next().unwrap_or(path_only);
    if path_only.is_empty() {
        return None;
    }
    let abs = doc_dir.join(path_only);
    let canonical = abs.canonicalize().ok()?;
    let rel = canonical.strip_prefix(workspace).ok()?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if file_index.contains(&rel_str) {
        Some(rel_str)
    } else {
        None
    }
}

// ── DB helpers ───────────────────────────────────────────────────────────────

fn collect_qualified_names(db: &Db) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(t) =
        db.query("MATCH (n) WHERE n:Function OR n:Symbol RETURN n.qualified_name AS qn")
    {
        for s in t.column_strings("qn") {
            out.insert(s);
        }
    }
    out
}

fn collect_short_names(db: &Db) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    if let Ok(t) = db
        .query("MATCH (n) WHERE n:Function OR n:Symbol RETURN n.name AS n, n.qualified_name AS qn")
    {
        let names = t.column_strings("n");
        let qns = t.column_strings("qn");
        for (n, qn) in names.into_iter().zip(qns.into_iter()) {
            out.entry(n).or_insert(qn);
        }
    }
    out
}

fn collect_file_paths(db: &Db) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(t) = db.query("MATCH (f:File) RETURN f.path AS path") {
        for s in t.column_strings("path") {
            out.insert(s);
        }
    }
    out
}

fn write_doc_node(db: &Db, qn: &str, title: &str, rel_path: &str, lines: usize) {
    let _ = db.run(&format!(
        "CREATE (d:Doc {{qualified_name: {qn}, title: {t}, path: {p}, line_count: {lines}}})",
        qn = escape_str(qn),
        t = escape_str(title),
        p = escape_str(rel_path),
    ));
}

fn write_section_node(db: &Db, qn: &str, heading: &str, level: u32, line: usize) {
    let _ = db.run(&format!(
        "CREATE (s:DocSection {{qualified_name: {qn}, heading: {h}, level: {level}, line: {line}}})",
        qn = escape_str(qn),
        h = escape_str(heading),
    ));
}

fn link_doc_section(db: &Db, doc_qn: &str, section_qn: &str) {
    let _ = db.run(&format!(
        "MATCH (d:Doc {{qualified_name: {d}}}), (s:DocSection {{qualified_name: {s}}}) CREATE (d)-[:HAS_SECTION]->(s)",
        d = escape_str(doc_qn),
        s = escape_str(section_qn),
    ));
}

fn write_mentions_edge(db: &Db, section_qn: &str, target_qn: &str) {
    // Target may be a Function or Symbol — match either via OR.
    let _ = db.run(&format!(
        "MATCH (s:DocSection {{qualified_name: {s}}}), (t {{qualified_name: {t}}}) WHERE t:Function OR t:Symbol CREATE (s)-[:MENTIONS]->(t)",
        s = escape_str(section_qn),
        t = escape_str(target_qn),
    ));
}

fn write_links_to_edge(db: &Db, section_qn: &str, file_path: &str) {
    let _ = db.run(&format!(
        "MATCH (s:DocSection {{qualified_name: {s}}}), (f:File {{path: {f}}}) CREATE (s)-[:LINKS_TO]->(f)",
        s = escape_str(section_qn),
        f = escape_str(file_path),
    ));
}

// ── File walker + heading slug ───────────────────────────────────────────────

fn collect_markdown_files(workspace: &Path) -> Vec<(std::path::PathBuf, String)> {
    use walkdir::WalkDir;
    WalkDir::new(workspace)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        })
        .filter(|e| {
            !e.path().components().any(|c| {
                matches!(c.as_os_str().to_str(), Some("target" | "node_modules" | ".git"))
            })
        })
        .map(|e| {
            let abs = e.path().to_path_buf();
            let rel = abs
                .strip_prefix(workspace)
                .unwrap_or(&abs)
                .to_string_lossy()
                .replace('\\', "/");
            (abs, rel)
        })
        .collect()
}

fn slug(heading: &str) -> String {
    let mut out = String::with_capacity(heading.len());
    let mut last_dash = true;
    for c in heading.chars() {
        if c.is_alphanumeric() || c == '_' {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.trim_start_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_and_extracts_title() {
        let src = "# Top\n\nbody\n\n## Sub\n\nmore";
        let p = parse_markdown(src);
        assert_eq!(p.title.as_deref(), Some("Top"));
        assert_eq!(p.sections.len(), 2);
    }

    #[test]
    fn slug_is_stable_across_punctuation() {
        assert_eq!(slug("TOP-3 phase 2 (final)"), "top-3-phase-2-final");
        assert_eq!(slug("`expect_value` semantics"), "expect_value-semantics");
        assert_eq!(slug("###"), "");
    }
}
