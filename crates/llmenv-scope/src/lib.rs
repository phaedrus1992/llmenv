mod content_walk;
pub mod marker_file;
mod marker_link;
pub mod matcher;
pub mod network;
pub(crate) mod probe;
pub mod ssid;
pub(crate) mod tags;
#[cfg(test)]
pub(crate) mod test_log_capture;

pub use matcher::Env;

use llmenv_config::Config;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// A tag or bundle name failed [`tag_keyword`]/[`bundle_keyword`]'s charset
/// check before a recall-query keyword could be built from it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeywordError {
    #[error("tag '{0}' contains invalid characters (only alphanumeric, -, _ allowed, non-empty)")]
    InvalidTag(String),
    #[error(
        "bundle '{0}' contains invalid characters (only alphanumeric, -, _ allowed, non-empty)"
    )]
    InvalidBundle(String),
}

/// The keyword prefix under which tag-scoped memory is stored and recalled.
/// A memory written for tag `work-vpn` carries keyword `llmenv-tag:work-vpn`;
/// recalling that keyword (project-unfiltered) surfaces it from any project.
const TAG_KEYWORD_PREFIX: &str = "llmenv-tag:";

/// The `llmenv-tag:<tag>` keyword for a tag.
///
/// Enforces the charset check in-crate (via [`matcher::is_valid_tag_charset`])
/// rather than trusting the caller to have validated the tag first, so an
/// unvalidated call is a runtime error here instead of a silent recall-query
/// injection risk.
///
/// # Errors
/// Returns [`KeywordError::InvalidTag`] if `tag` is empty or contains a
/// character outside `[a-zA-Z0-9_-]`.
pub fn tag_keyword(tag: &str) -> Result<String, KeywordError> {
    if !matcher::is_valid_tag_charset(tag) {
        return Err(KeywordError::InvalidTag(tag.to_string()));
    }
    Ok(format!("{TAG_KEYWORD_PREFIX}{tag}"))
}

/// The keyword prefix under which bundle-scoped memory is stored and recalled.
/// A memory written for bundle `base` carries keyword `llmenv-bundle:base`;
/// recalling that keyword (project-unfiltered) surfaces it from any project.
const BUNDLE_KEYWORD_PREFIX: &str = "llmenv-bundle:";

/// The `llmenv-bundle:<bundle>` keyword for a bundle.
///
/// Enforces the charset check in-crate (via [`matcher::is_valid_tag_charset`])
/// rather than trusting the caller to have validated the bundle name first, so
/// an unvalidated call is a runtime error here instead of a silent
/// recall-query injection risk.
///
/// # Errors
/// Returns [`KeywordError::InvalidBundle`] if `bundle` is empty or contains a
/// character outside `[a-zA-Z0-9_-]`.
pub fn bundle_keyword(bundle: &str) -> Result<String, KeywordError> {
    if !matcher::is_valid_tag_charset(bundle) {
        return Err(KeywordError::InvalidBundle(bundle.to_string()));
    }
    Ok(format!("{BUNDLE_KEYWORD_PREFIX}{bundle}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod keyword_tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn tag_keyword_prefixes_tag() {
        assert_eq!(tag_keyword("work-vpn").unwrap(), "llmenv-tag:work-vpn");
        assert_eq!(tag_keyword("rust").unwrap(), "llmenv-tag:rust");
    }

    #[test]
    fn bundle_keyword_prefixes_bundle() {
        assert_eq!(bundle_keyword("base").unwrap(), "llmenv-bundle:base");
        assert_eq!(
            bundle_keyword("rust-defaults").unwrap(),
            "llmenv-bundle:rust-defaults"
        );
    }

    #[test]
    fn tag_keyword_rejects_empty_tag() {
        assert!(matches!(tag_keyword(""), Err(KeywordError::InvalidTag(t)) if t.is_empty()));
    }

    #[test]
    fn tag_keyword_rejects_query_injection_characters() {
        assert!(tag_keyword("tag with space").is_err());
        assert!(tag_keyword("tag:colon").is_err());
        assert!(tag_keyword("tag\"quote").is_err());
    }

    #[test]
    fn bundle_keyword_rejects_empty_bundle() {
        assert!(matches!(bundle_keyword(""), Err(KeywordError::InvalidBundle(b)) if b.is_empty()));
    }

    #[test]
    fn bundle_keyword_rejects_query_injection_characters() {
        assert!(bundle_keyword("bundle with space").is_err());
        assert!(bundle_keyword("bundle:colon").is_err());
    }

    /// A tag accepted by [`tag_keyword`]'s charset check.
    fn valid_tag() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{1,24}"
    }

    /// A string guaranteed to fail [`is_valid_tag_charset`] — non-empty but
    /// containing at least one character outside the allowed charset.
    fn invalid_tag() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{0,12}[^a-zA-Z0-9_-][a-zA-Z0-9_-]{0,12}"
    }

    proptest! {
        // tag_keyword always prepends the prefix and preserves the tag exactly —
        // the keyword is `llmenv-tag:` + the unmodified tag for any valid input.
        #[test]
        fn prop_tag_keyword_is_prefix_plus_tag(tag in valid_tag()) {
            let kw = tag_keyword(&tag).unwrap();
            prop_assert_eq!(&kw, &format!("{TAG_KEYWORD_PREFIX}{tag}"));
            prop_assert!(kw.starts_with(TAG_KEYWORD_PREFIX));
            prop_assert_eq!(&kw[TAG_KEYWORD_PREFIX.len()..], tag.as_str());
        }

        // bundle_keyword always prepends the bundle prefix and preserves the bundle
        // name exactly — the keyword is `llmenv-bundle:` + the unmodified bundle.
        #[test]
        fn prop_bundle_keyword_is_prefix_plus_bundle(bundle in valid_tag()) {
            let kw = bundle_keyword(&bundle).unwrap();
            prop_assert_eq!(&kw, &format!("{BUNDLE_KEYWORD_PREFIX}{bundle}"));
            prop_assert!(kw.starts_with(BUNDLE_KEYWORD_PREFIX));
            prop_assert_eq!(&kw[BUNDLE_KEYWORD_PREFIX.len()..], bundle.as_str());
        }

        // Any string containing a charset-violating character is rejected,
        // regardless of what else surrounds it.
        #[test]
        fn prop_tag_keyword_rejects_invalid_charset(tag in invalid_tag()) {
            prop_assert!(tag_keyword(&tag).is_err());
        }

        #[test]
        fn prop_bundle_keyword_rejects_invalid_charset(bundle in invalid_tag()) {
            prop_assert!(bundle_keyword(&bundle).is_err());
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ActiveScope {
    pub id: String,
    pub kind: &'static str,
    pub tags: Vec<String>,
    /// On-disk root the scope matched against. Populated only for
    /// `kind == "project"` (the directory containing `.llmenv.yaml`).
    /// `None` for all other scope kinds.
    pub project_root: Option<PathBuf>,
    /// Bundles this scope manually enables (from `.llmenv.yaml`'s
    /// `enable_bundles` list). Only populated for project scopes.
    pub enable_bundles: Vec<String>,
    /// Bundles this scope removes from the firing set (from `.llmenv.yaml`'s
    /// `disable_bundles` list, #194). Only populated for project scopes.
    /// Disable always wins over any scope's tag-firing or `enable_bundles`,
    /// including this same scope's own `enable_bundles`.
    pub disable_bundles: Vec<String>,
    /// Project display name (from `.llmenv.yaml` `name` field or folder
    /// basename). Only present for `kind == "project"`.
    pub name: Option<String>,
    /// Project description (from `.llmenv.yaml` `description` field).
    /// Only present for `kind == "project"`.
    pub description: Option<String>,
    /// Unknown fields from `.llmenv.yaml` (for warnings in `llmenv doctor`).
    /// Only populated for project scopes.
    pub unknown_fields: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ActiveScopes {
    pub scopes: Vec<ActiveScope>,
    pub tags: BTreeSet<String>,
    /// Tags from `$LLMENV_EXTRA_TAGS`. Not attached to any scope, so tracked
    /// separately from `tags` — `non_project_tags()` excludes them from
    /// host-level plugin/MCP resolution, matching the trust level of the
    /// `.llmenv.yaml` project tags they're the escape hatch for (#696, #1020).
    pub extra_tags: BTreeSet<String>,
}

impl ActiveScopes {
    /// Tags from all scopes whose `kind` is not `"project"`.
    ///
    /// Project-scoped tags describe the *project's* domain (e.g.
    /// `lang-typescript`, `agent-coding`) and must not leak into host-level
    /// plugin collection selection — a project's `.llmenv.yaml` should not
    /// cause plugins like `fullstack-dev-skills` to appear in the generated
    /// host `settings.json` (#696). `$LLMENV_EXTRA_TAGS` tags get the same
    /// treatment: they're the documented substitute for a committed
    /// `.llmenv.yaml`, so they must not reach a more privileged surface than
    /// the project tags they stand in for.
    ///
    /// Non-project tags (network, host, user, content) describe the
    /// *environment*, which is the correct scope for host plugin decisions.
    pub fn non_project_tags(&self) -> BTreeSet<String> {
        // Tags from non-project scopes (network, host, user, content).
        let mut result: BTreeSet<String> = self
            .scopes
            .iter()
            .filter(|s| s.kind != "project")
            .flat_map(|s| s.tags.iter().cloned())
            .collect();
        // Preserve tags in the union that no scope contributed (e.g. OS
        // tag added by evaluate()) and that aren't extra_tags. These are
        // never project-scoped.
        result.extend(
            self.tags
                .iter()
                .filter(|t| {
                    !self.scopes.iter().any(|s| s.tags.contains(t)) && !self.extra_tags.contains(*t)
                })
                .cloned(),
        );
        result
    }
}

pub fn evaluate(cfg: &Config, env: &Env) -> ActiveScopes {
    let mut scopes = Vec::new();
    for s in &cfg.scope.network {
        if network::matches_network(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "network",
                tags: crate::tags::sanitize_tags(s.tags.clone(), "config.yaml scope tags"),
                ..Default::default()
            });
        }
    }
    for s in &cfg.scope.host {
        if matcher::matches_host(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "host",
                tags: crate::tags::sanitize_tags(s.tags.clone(), "config.yaml scope tags"),
                ..Default::default()
            });
        }
    }
    for s in &cfg.scope.user {
        if matcher::matches_user(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "user",
                tags: crate::tags::sanitize_tags(s.tags.clone(), "config.yaml scope tags"),
                ..Default::default()
            });
        }
    }
    let cwd = std::path::Path::new(&env.cwd);
    if cwd.exists() && !cfg.scope.content.is_empty() {
        let matched_ids = matcher::matches_content_all(&cfg.scope.content, cwd);
        for s in &cfg.scope.content {
            if matched_ids.contains(s.id.as_str()) {
                scopes.push(ActiveScope {
                    id: s.id.clone(),
                    kind: "content",
                    tags: crate::tags::sanitize_tags(s.tags.clone(), "config.yaml scope tags"),
                    ..Default::default()
                });
            }
        }
    }
    if let Some(p) = marker_file::discover_project(env) {
        scopes.push(ActiveScope {
            id: p.id,
            kind: "project",
            tags: p.tags,
            project_root: Some(p.root),
            enable_bundles: p.enable_bundles,
            disable_bundles: p.disable_bundles,
            name: Some(p.name),
            description: p.description,
            unknown_fields: p.unknown_fields,
        });
    }
    let mut tags: BTreeSet<String> = scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
    if !env.os.is_empty() {
        tags.insert(env.os.clone());
    }
    let extra_tags: BTreeSet<String> = env.extra_tags.iter().cloned().collect();
    tags.extend(extra_tags.iter().cloned());
    let tags = crate::tags::cap_aggregate_tags(tags);
    ActiveScopes {
        scopes,
        tags,
        extra_tags,
    }
}

#[cfg(test)]
mod tests;
