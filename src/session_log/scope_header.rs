//! Builds the scope-header event's content + metadata. Content carries the
//! `llmenv-tag:` / `llmenv-bundle:` tokens so ICM's content-only FTS can find a
//! session by the scope that produced it. Tokens reuse the existing keyword
//! helpers so the encoding never drifts.

use llmenv_scope::{bundle_keyword, tag_keyword};

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
        // `p` is a free-form display name from `.llmenv.yaml`'s `name:` field
        // (unlike tags/bundles, not charset-restricted), so it gets control
        // characters escaped rather than the tag/bundle charset rule (#1578).
        parts.push(format!("project:{}", display_safe(p)));
    }
    for t in &ctx.tags {
        match tag_keyword(t) {
            Ok(kw) => parts.push(kw),
            Err(e) => tracing::warn!(tag = %t, error = %e, "skipping invalid tag in scope header"),
        }
    }
    for b in &ctx.bundles {
        match bundle_keyword(b) {
            Ok(kw) => parts.push(kw),
            Err(e) => {
                tracing::warn!(bundle = %b, error = %e, "skipping invalid bundle in scope header");
            }
        }
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
    fn content_escapes_control_characters_in_project_name() {
        let c = scope_header_content(&ScopeContext {
            tags: vec![],
            bundles: vec![],
            project: Some("evil\x1b[2Kname\ninjected".into()),
            cwd: "/".into(),
            adapter: "claude_code".into(),
            llmenv_version: "3.0.0".into(),
            claude_code_version: String::new(),
        });
        assert!(!c.contains('\x1b'));
        assert!(!c.contains('\n'));
        assert!(c.contains("evil"), "non-control text is preserved: {c}");
    }

    #[test]
    fn content_skips_invalid_tag_and_bundle_without_panicking() {
        let c = scope_header_content(&ScopeContext {
            tags: vec!["rust".into(), "has space".into()],
            bundles: vec!["base".into(), "bad:bundle".into()],
            project: None,
            cwd: "/".into(),
            adapter: "claude_code".into(),
            llmenv_version: "3.0.0".into(),
            claude_code_version: String::new(),
        });
        assert!(c.contains("llmenv-tag:rust"));
        assert!(c.contains("llmenv-bundle:base"));
        assert!(!c.contains("has space"));
        assert!(!c.contains("bad:bundle"));
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
