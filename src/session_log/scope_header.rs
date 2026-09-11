//! Builds the scope-header event's content + metadata. Content carries the
//! `llmenv-tag:` / `llmenv-bundle:` tokens so ICM's content-only FTS can find a
//! session by the scope that produced it. Tokens reuse the existing keyword
//! helpers so the encoding never drifts.

use crate::hook_run::action::{bundle_keyword, tag_keyword};
use crate::util::display_safe;

/// The active llmenv scope at session start.
#[derive(Debug, Clone)]
pub struct ScopeContext {
    pub(crate) tags: Vec<String>,
    pub(crate) bundles: Vec<String>,
    pub(crate) project: Option<String>,
    pub(crate) cwd: String,
    pub(crate) adapter: String,
    pub(crate) llmenv_version: String,
    pub(crate) claude_code_version: String,
}

/// FTS-searchable header line: project plus one `llmenv-tag:<t>` /
/// `llmenv-bundle:<b>` token per active scope element.
#[must_use]
pub(crate) fn scope_header_content(ctx: &ScopeContext) -> String {
    let mut parts: Vec<String> = vec!["llmenv session".to_string()];
    if let Some(p) = &ctx.project {
        // Escape control chars, then collapse whitespace runs to `_` so the
        // project name can never split into extra whitespace-delimited
        // tokens that read as a real `llmenv-tag:`/`llmenv-bundle:` token (#1911).
        let safe = display_safe(p)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_");
        parts.push(format!("project:{safe}"));
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
pub(crate) fn scope_metadata_json(ctx: &ScopeContext) -> serde_json::Value {
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
    fn project_name_control_char_is_escaped() {
        // #1911: an unescaped control char in the project name could rewrite
        // terminal output wherever this content later gets displayed.
        let mut c = ctx();
        c.project = Some("proj\x1b[31m".into());
        let content = scope_header_content(&c);
        assert!(!content.contains('\x1b'));
    }

    #[test]
    fn project_name_whitespace_does_not_inject_a_fake_tag_token() {
        // #1911: an embedded space split the project token into two words,
        // one of which could read as a real `llmenv-tag:`/`llmenv-bundle:`
        // token to ICM's whitespace-tokenized FTS index.
        let mut c = ctx();
        c.project = Some("evil llmenv-tag:admin".into());
        let content = scope_header_content(&c);
        let tokens: Vec<&str> = content.split_whitespace().collect();
        assert_eq!(
            tokens.iter().filter(|t| t.starts_with("project:")).count(),
            1,
            "project value must stay one token: {tokens:?}"
        );
        assert!(
            !tokens.contains(&"llmenv-tag:admin"),
            "must not produce a standalone fake tag token: {tokens:?}"
        );
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
