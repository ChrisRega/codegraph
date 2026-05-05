//! Minimal Gherkin extractor — pulls Feature / Scenario / Step entries out of
//! `.feature` files. We don't need full Gherkin compliance (no
//! Outlines/Examples handling); the goal is enough structure for the
//! `:Step.text` ↔ `:Function.step_regex` linker to work.
//!
//! Recognised lines (after trimming):
//!   - `Feature:` / `Rule:` — opens a Feature
//!   - `Scenario:` / `Scenario Outline:` / `Example:` — opens a Scenario
//!   - `Background:` — opens a Background scenario (treated like Scenario)
//!   - `@tag1 @tag2` — tags attach to the next Feature/Scenario
//!   - `Given/When/Then/And/But …` — emits a Step under the current Scenario
//!
//! Triple-quoted doc-strings (`"""`) and data tables (`|`) attached to a step
//! are ignored — the step text is the line itself.

#[derive(Debug, Clone)]
pub enum FeatureItem {
    Feature {
        name: String,
        file_path: String,
        line: usize,
        tags: Vec<String>,
    },
    Scenario {
        // `feature_name` and `id` are emitted for future consumers that
        // want to re-assemble the feature hierarchy without re-walking
        // the stream; the current main.rs indexer destructures them as
        // `_`. `#[allow(dead_code)]` pinned on each field because the
        // derived impls take precedence over an enum-level allow.
        #[allow(dead_code)]
        feature_name: String,
        name: String,
        line: usize,
        tags: Vec<String>,
        /// Stable id within the file: `feature_name :: scenario_name @ line`.
        #[allow(dead_code)]
        id: String,
    },
    Step {
        #[allow(dead_code)]
        scenario_id: String,
        order: usize,
        kind: String, // "Given" | "When" | "Then" (And/But folded into prev kind)
        text: String,
        line: usize,
    },
}

pub fn parse_feature_file(source: &str, file_path: &str) -> Vec<FeatureItem> {
    let mut out = Vec::new();
    let mut pending_tags: Vec<String> = Vec::new();
    let mut current_feature: Option<String> = None;
    let mut current_scenario_id: Option<String> = None;
    let mut last_step_kind: Option<String> = None;
    let mut step_order: usize = 0;
    let mut in_docstring = false;

    for (i, raw) in source.lines().enumerate() {
        let line_no = i + 1;
        let trimmed = raw.trim();

        // Toggle on `"""` fences — content between fences is opaque step body.
        if trimmed == "\"\"\"" || trimmed == "```" {
            in_docstring = !in_docstring;
            continue;
        }
        if in_docstring {
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Data tables — skip silently.
        if trimmed.starts_with('|') {
            continue;
        }

        // Tags attach to the NEXT Feature / Scenario.
        if trimmed.starts_with('@') {
            for tok in trimmed.split_whitespace() {
                if let Some(stripped) = tok.strip_prefix('@') {
                    pending_tags.push(stripped.to_string());
                }
            }
            continue;
        }

        if let Some(name) = strip_keyword(trimmed, &["Feature:", "Rule:"]) {
            current_feature = Some(name.to_string());
            current_scenario_id = None;
            last_step_kind = None;
            step_order = 0;
            out.push(FeatureItem::Feature {
                name: name.to_string(),
                file_path: file_path.to_string(),
                line: line_no,
                tags: std::mem::take(&mut pending_tags),
            });
            continue;
        }

        if let Some(name) = strip_keyword(
            trimmed,
            &["Scenario:", "Scenario Outline:", "Example:", "Background:"],
        ) {
            let feature_name = current_feature
                .clone()
                .unwrap_or_else(|| "<no feature>".to_string());
            let id = format!("{}::{}@{}", feature_name, name, line_no);
            current_scenario_id = Some(id.clone());
            last_step_kind = None;
            step_order = 0;
            out.push(FeatureItem::Scenario {
                feature_name,
                name: name.to_string(),
                line: line_no,
                tags: std::mem::take(&mut pending_tags),
                id,
            });
            continue;
        }

        // Step lines.
        let (kind_opt, rest) = parse_step_keyword(trimmed);
        if let Some(kind_kw) = kind_opt {
            let Some(scenario_id) = current_scenario_id.clone() else {
                continue;
            };
            // And/But inherit the previous keyword's category.
            let effective_kind = match kind_kw {
                "And" | "But" | "*" => last_step_kind
                    .clone()
                    .unwrap_or_else(|| "Given".to_string()),
                k => {
                    last_step_kind = Some(k.to_string());
                    k.to_string()
                }
            };
            step_order += 1;
            out.push(FeatureItem::Step {
                scenario_id,
                order: step_order,
                kind: effective_kind,
                text: rest.to_string(),
                line: line_no,
            });
            continue;
        }
        // Unknown line — likely a free-form description under Feature:; ignore.
    }

    out
}

fn strip_keyword<'a>(line: &'a str, keywords: &[&str]) -> Option<&'a str> {
    for kw in keywords {
        if let Some(rest) = line.strip_prefix(kw) {
            return Some(rest.trim());
        }
    }
    None
}

fn parse_step_keyword(line: &str) -> (Option<&'static str>, &str) {
    for kw in ["Given", "When", "Then", "And", "But"] {
        if let Some(rest) = line.strip_prefix(kw) {
            // Must be followed by whitespace (avoid matching e.g. `Givenchy`).
            if rest.starts_with(char::is_whitespace) {
                return (Some(kw), rest.trim_start());
            }
        }
    }
    if let Some(rest) = line.strip_prefix('*') {
        if rest.starts_with(char::is_whitespace) {
            return (Some("*"), rest.trim_start());
        }
    }
    (None, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_feature_with_two_scenarios() {
        let src = r#"Feature: Demo
  @smoke
  Scenario: First
    Given a fresh database
    When I run cypher "MATCH (n) RETURN n"
    Then the result has 0 rows for "n"

  Scenario: Second
    Given a fresh database
    Then the database vertex count is 0
"#;
        let items = parse_feature_file(src, "demo.feature");
        let features = items
            .iter()
            .filter(|i| matches!(i, FeatureItem::Feature { .. }))
            .count();
        let scenarios = items
            .iter()
            .filter(|i| matches!(i, FeatureItem::Scenario { .. }))
            .count();
        let steps = items
            .iter()
            .filter(|i| matches!(i, FeatureItem::Step { .. }))
            .count();
        assert_eq!(features, 1);
        assert_eq!(scenarios, 2);
        assert_eq!(steps, 5);
    }

    #[test]
    fn and_inherits_previous_kind() {
        let src = "Feature: F\n  Scenario: S\n    Given a thing\n    And another\n    Then ok\n";
        let items = parse_feature_file(src, "x.feature");
        let kinds: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                FeatureItem::Step { kind, .. } => Some(kind.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec!["Given", "Given", "Then"]);
    }

    #[test]
    fn docstring_body_is_skipped() {
        let src = r#"Feature: F
  Scenario: S
    Given the graph:
      """
      Then this is not a step
      """
    Then ok
"#;
        let items = parse_feature_file(src, "x.feature");
        let step_count = items
            .iter()
            .filter(|i| matches!(i, FeatureItem::Step { .. }))
            .count();
        assert_eq!(step_count, 2);
    }

    #[test]
    fn tags_attach_to_following_node() {
        let src = "@a @b\nFeature: F\n  @c\n  Scenario: S\n    Given x\n";
        let items = parse_feature_file(src, "x.feature");
        let mut feature_tags = vec![];
        let mut scenario_tags = vec![];
        for it in &items {
            match it {
                FeatureItem::Feature { tags, .. } => feature_tags = tags.clone(),
                FeatureItem::Scenario { tags, .. } => scenario_tags = tags.clone(),
                _ => {}
            }
        }
        assert_eq!(feature_tags, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(scenario_tags, vec!["c".to_string()]);
    }
}
