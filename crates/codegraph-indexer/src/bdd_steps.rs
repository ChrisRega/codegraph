//! BDD step-impl extractor — the only syn touch point left in the indexer.
//!
//! LSPs don't expose macro attribute literals, so for cucumber-rs's
//! `#[given/when/then(regex = "…")]` we still need to walk the source AST.
//! This module is intentionally tiny and only runs over files that the LSP
//! indexer has already promoted to `:Function` nodes.
//!
//! Pipeline contract (called from `main`):
//!
//!   1. Caller queries the DB for test-file paths likely to contain step
//!      impls (e.g. anything under `tests/` with `cucumber` in its
//!      `Cargo.toml` dev-deps — or just every `.rs` under `tests/`).
//!   2. For each path, [`extract_step_impls_from_file`] returns the
//!      `(fn_name, kind, regex)` triples of every function carrying a
//!      cucumber attribute.
//!   3. Caller updates the corresponding `:Function` node (by `name`)
//!      with `step_kind` / `step_regex` properties and sets `kind = 'Step'`.
//!   4. The downstream regex-linker matches `:Step.text` against the now-
//!      populated `step_regex` and creates `IMPLEMENTED_BY` edges.
//!
//! Out of scope: anything else syn was doing (struct/enum extraction, call
//! graph, …) — the LSP path owns that surface now.

use syn::Attribute;

#[derive(Debug, Clone)]
pub struct StepImpl {
    /// The bare function name, matching `:Function.name` in the graph.
    pub fn_name: String,
    /// `"Given"` / `"When"` / `"Then"`.
    pub step_kind: String,
    /// Raw regex pattern from `regex = "…"`.
    pub step_regex: String,
}

/// Parse `source` and return one `StepImpl` per function carrying a
/// `#[given(regex = "…")]` / `#[when(regex = "…")]` / `#[then(regex = "…")]`
/// attribute. Files that don't parse cleanly yield an empty Vec — silent
/// because cargo-test failures will already flag them.
pub fn extract_step_impls_from_file(source: &str) -> Vec<StepImpl> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in &file.items {
        if let syn::Item::Fn(f) = item {
            if let Some((kind, regex)) = extract_step_attr(&f.attrs) {
                out.push(StepImpl {
                    fn_name: f.sig.ident.to_string(),
                    step_kind: kind,
                    step_regex: regex,
                });
            }
        }
    }
    out
}

/// Detect `#[given(regex = "…")]`, `#[when(regex = "…")]`, `#[then(regex = "…")]`.
fn extract_step_attr(attrs: &[Attribute]) -> Option<(String, String)> {
    for attr in attrs {
        let seg = attr.path().segments.last()?;
        let kind = match seg.ident.to_string().as_str() {
            "given" => "Given",
            "when" => "When",
            "then" => "Then",
            _ => continue,
        };
        let mut found_regex: Option<String> = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("regex") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                found_regex = Some(lit.value());
            } else if meta.input.peek(syn::Token![=]) {
                // Skip other meta items (e.g. `expr = …`) without erroring.
                let value = meta.value()?;
                let _: proc_macro2::TokenStream = value.parse()?;
            }
            Ok(())
        });
        if let Some(r) = found_regex {
            return Some((kind.to_string(), r));
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn extracts_given_when_then() {
        let src = r##"
            #[given(regex = r"^a fresh database$")]
            fn fresh_db() {}

            #[when(regex = r#"^I run cypher "([^"]+)"$"#)]
            fn run_cypher() {}

            #[then(regex = r"^the result has (\d+) rows?$")]
            fn assert_rows() {}

            fn untouched() {}
        "##;
        let impls = extract_step_impls_from_file(src);
        assert_eq!(impls.len(), 3);
        assert_eq!(impls[0].fn_name, "fresh_db");
        assert_eq!(impls[0].step_kind, "Given");
        assert_eq!(impls[0].step_regex, "^a fresh database$");
        assert_eq!(impls[1].step_kind, "When");
        assert_eq!(impls[2].step_kind, "Then");
    }

    #[test]
    fn ignores_attrs_without_regex() {
        let src = r#"
            #[test]
            fn just_a_test() {}

            #[given(expr = "no regex here")]
            fn weird() {}
        "#;
        assert!(extract_step_impls_from_file(src).is_empty());
    }

    #[test]
    fn empty_source_is_safe() {
        assert!(extract_step_impls_from_file("").is_empty());
        assert!(extract_step_impls_from_file("not valid rust {").is_empty());
    }
}
