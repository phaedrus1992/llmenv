//! Builds the scope-header event's content + metadata. Content carries the
//! `llmenv-tag:` / `llmenv-bundle:` tokens so ICM's content-only FTS can find a
//! session by the scope that produced it. Tokens reuse the existing keyword
//! helpers so the encoding never drifts.

use crate::hook_run::action::{bundle_keyword, tag_keyword};

/// The active llmenv scope at session start.
#[derive(Debug, Clone)]
pub struct ScopeContext {
    pub tags: Vec<String>,
    pub bundles: Vec<String>,
    pub project: Option<String>,
    pub cwd: String,
    pub adapter: String,
    pub llmenv_version: String,
    pub claude_code_version: String,
}

/// FTS-searchable header line: project plus one `llmenv-tag:<t>` /
/// `llmenv-bundle:<b>` token per active scope element.
#[must_use]
pub fn scope_header_content(ctx: &ScopeContext) -> String {
    let mut parts: Vec<String> = vec!["llmenv session".to_string()];
    if let Some(p) = &ctx.project {
        parts.push(format!("project:{p}"));
    }
    for t in &ctx.tags {
        parts.push(tag_keyword(t));
    }
    for b in &ctx.bundles {
        parts.push(bundle_keyword(b));
    }
    parts.join(" ")
}

/// Full structured session metadata for exact inspection / replay.
#[must_use]
pub fn scope_metadata_json(ctx: &ScopeContext) -> serde_json::Value {
    serde_json::json!({
        "tags": ctx.tags,
        "bundles": ctx.bundles,
        "project": ctx.project,
        "cwd": ctx.cwd,
        "adapter": ctx.adapter,
        "llmenv_version": ctx.llmenv_version,
        "claude_code_version": ctx.claude_code_version,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ctx() -> ScopeContext {
        ScopeContext {
            tags: vec!["rust".into(), "work-vpn".into()],
            bundles: vec!["base".into()],
            project: Some("llmenv".into()),
            cwd: "/Users/x/git/llmenv".into(),
            adapter: "claude_code".into(),
            llmenv_version: "3.0.0".into(),
            claude_code_version: "3.4.0".into(),
        }
    }

    #[test]
    fn content_embeds_searchable_tag_and_bundle_tokens() {
        let c = scope_header_content(&ctx());
        assert!(c.contains("llmenv-tag:rust"));
        assert!(c.contains("llmenv-tag:work-vpn"));
        assert!(c.contains("llmenv-bundle:base"));
        assert!(c.contains("llmenv"), "project name present");
    }

    #[test]
    fn metadata_carries_full_structured_fields() {
        let m = scope_metadata_json(&ctx());
        assert_eq!(m["tags"], serde_json::json!(["rust", "work-vpn"]));
        assert_eq!(m["bundles"], serde_json::json!(["base"]));
        assert_eq!(m["adapter"], "claude_code");
        assert_eq!(m["llmenv_version"], "3.0.0");
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn every_tag_and_bundle_appears_as_a_token(
            tags in proptest::collection::vec("[a-z0-9_-]{1,12}", 0..5),
            bundles in proptest::collection::vec("[a-z0-9_-]{1,12}", 0..5),
        ) {
            let c = scope_header_content(&ScopeContext {
                tags: tags.clone(),
                bundles: bundles.clone(),
                project: None,
                cwd: "/".into(),
                adapter: "claude_code".into(),
                llmenv_version: "3.0.0".into(),
                claude_code_version: String::new(),
            });
            for t in &tags {
                let needle = format!("llmenv-tag:{}", t);
                prop_assert!(c.contains(&needle), "missing token {}", needle);
            }
            for b in &bundles {
                let needle = format!("llmenv-bundle:{}", b);
                prop_assert!(c.contains(&needle), "missing token {}", needle);
            }
        }
    }
}
