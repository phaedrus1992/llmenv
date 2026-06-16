//! The memory action each lifecycle event performs, and how it maps to an ICM
//! MCP tool call.

use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::hook_run::mcp_client::McpHttpClient;
use crate::hook_run::{BundleRecallQuery, TagRecallQuery};

/// The keyword prefix under which tag-scoped memory is stored and recalled.
/// A memory written for tag `work-vpn` carries keyword `llmenv-tag:work-vpn`;
/// recalling that keyword (project-unfiltered) surfaces it from any project.
pub const TAG_KEYWORD_PREFIX: &str = "llmenv-tag:";

/// The `llmenv-tag:<tag>` keyword for a tag. The tag is assumed pre-validated
/// (see `hook_run::validate_tag`) so it contains no recall-query metacharacters.
#[must_use]
pub fn tag_keyword(tag: &str) -> String {
    format!("{TAG_KEYWORD_PREFIX}{tag}")
}

/// The keyword prefix under which bundle-scoped memory is stored and recalled.
/// A memory written for bundle `base` carries keyword `llmenv-bundle:base`;
/// recalling that keyword (project-unfiltered) surfaces it from any project.
pub const BUNDLE_KEYWORD_PREFIX: &str = "llmenv-bundle:";

/// The `llmenv-bundle:<bundle>` keyword for a bundle. The bundle name is
/// assumed pre-validated (see `hook_run::validate_bundle`) so it contains no
/// recall-query metacharacters.
#[must_use]
pub fn bundle_keyword(bundle: &str) -> String {
    format!("{BUNDLE_KEYWORD_PREFIX}{bundle}")
}

/// One memory action against the ICM MCP backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Inject the session wake-up pack (`icm_wake_up`).
    WakeUp,
    /// Inject recalled context for the active tags/project (`icm_memory_recall`).
    /// Project-scoped (cwd default) natural-language recall.
    Recall,
    /// Recall tag-scoped memory for one active tag (`icm_memory_recall`),
    /// **project-unfiltered** and keyed on `llmenv-tag:<tag>`. This is what
    /// makes memory stored under a tag in one project surface when the same tag
    /// activates in another (#197). One action is dispatched per active tag. The
    /// carried [`TagRecallQuery`] is the single source of the tag + keyword, so
    /// the keyword encoding never drifts between dispatch and the tool call.
    RecallTag(TagRecallQuery),
    /// Recall bundle-scoped memory for one active bundle (`icm_memory_recall`),
    /// **project-unfiltered** and keyed on `llmenv-bundle:<bundle>`. Mirrors
    /// `RecallTag` for bundles (#228): one action per active bundle, ensuring
    /// memory stored under a bundle in one project surfaces in another.
    RecallBundle(BundleRecallQuery),
    /// Best-effort store of the active scope context (`icm_memory_store`).
    Store,
}

impl Action {
    /// The ICM MCP tool this action calls.
    pub fn tool_name(&self) -> &'static str {
        match self {
            Action::WakeUp => "icm_wake_up",
            Action::Recall | Action::RecallTag(_) | Action::RecallBundle(_) => "icm_memory_recall",
            Action::Store => "icm_memory_store",
        }
    }

    /// Build the `arguments` object for this action's tool call. `query` is the
    /// recall query (active tags/project), `chunk` is the llmenv context chunk
    /// used as store content. Unused fields are ignored per action.
    ///
    /// `RecallTag` and `RecallBundle` pass `project: ""` to disable ICM's
    /// default cwd project filter (per the tool contract, an empty string
    /// searches all projects) and `keyword: llmenv-tag:<tag>` /
    /// `keyword: llmenv-bundle:<bundle>` to scope the recall.
    pub fn arguments(&self, query: &str, chunk: &str) -> Value {
        match self {
            Action::WakeUp => json!({}),
            Action::Recall => json!({ "query": query }),
            Action::RecallTag(q) => json!({
                "query": q.tag,
                "project": "",
                "keyword": q.keyword,
            }),
            Action::RecallBundle(q) => json!({
                "query": q.bundle,
                "project": "",
                "keyword": q.keyword,
            }),
            Action::Store => json!({ "content": chunk }),
        }
    }

    /// Execute the action: call the tool and return its text result.
    ///
    /// # Errors
    /// Propagates any client/network error from `call_tool`.
    pub async fn run(
        &self,
        client: &McpHttpClient,
        query: &str,
        chunk: &str,
    ) -> anyhow::Result<String> {
        debug!(action = ?self, "dispatching MCP tool call");
        client
            .call_tool(self.tool_name(), self.arguments(query, chunk))
            .await
            .map_err(|e| {
                warn!(action = ?self, error = %e, "MCP tool call failed");
                e
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn action_tool_name_mapping() {
        assert_eq!(Action::WakeUp.tool_name(), "icm_wake_up");
        assert_eq!(Action::Recall.tool_name(), "icm_memory_recall");
        assert_eq!(Action::Store.tool_name(), "icm_memory_store");
        assert_eq!(recall_bundle("base").tool_name(), "icm_memory_recall");
    }

    #[test]
    fn wakeup_arguments_are_empty_object() {
        let args = Action::WakeUp.arguments("query text", "chunk text");
        assert_eq!(args, serde_json::json!({}));
    }

    #[test]
    fn recall_arguments_carry_query() {
        let args = Action::Recall.arguments("rust, work", "chunk");
        assert_eq!(args["query"], serde_json::json!("rust, work"));
    }

    #[test]
    fn store_arguments_carry_content() {
        let args = Action::Store.arguments("query", "## llmenv context\n...");
        assert_eq!(args["content"], serde_json::json!("## llmenv context\n..."));
    }

    #[test]
    fn tag_keyword_prefixes_tag() {
        assert_eq!(tag_keyword("work-vpn"), "llmenv-tag:work-vpn");
        assert_eq!(tag_keyword("rust"), "llmenv-tag:rust");
    }

    fn recall_tag(tag: &str) -> Action {
        Action::RecallTag(TagRecallQuery {
            tag: tag.to_string(),
            keyword: tag_keyword(tag),
        })
    }

    fn recall_bundle(bundle: &str) -> Action {
        Action::RecallBundle(BundleRecallQuery {
            bundle: bundle.to_string(),
            keyword: bundle_keyword(bundle),
        })
    }

    #[test]
    fn recall_tag_tool_is_memory_recall() {
        assert_eq!(recall_tag("work-vpn").tool_name(), "icm_memory_recall");
    }

    #[test]
    fn bundle_keyword_prefixes_bundle() {
        assert_eq!(bundle_keyword("base"), "llmenv-bundle:base");
        assert_eq!(
            bundle_keyword("rust-defaults"),
            "llmenv-bundle:rust-defaults"
        );
    }

    #[test]
    fn recall_bundle_tool_is_memory_recall() {
        assert_eq!(recall_bundle("base").tool_name(), "icm_memory_recall");
    }

    #[test]
    fn recall_bundle_disables_project_filter() {
        let args = recall_bundle("base").arguments("ignored", "ignored");
        assert_eq!(
            args["project"],
            serde_json::json!(""),
            "project must be empty to search across all projects"
        );
    }

    #[test]
    fn recall_bundle_keys_on_llmenv_bundle_keyword() {
        let args = recall_bundle("base").arguments("ignored", "ignored");
        assert_eq!(
            args["keyword"],
            serde_json::json!("llmenv-bundle:base"),
            "recall must be keyed on the llmenv-bundle:<bundle> encoding"
        );
        assert_eq!(args["query"], serde_json::json!("base"));
    }

    #[test]
    fn recall_tag_disables_project_filter() {
        // The defining behavior of #197: tag-scoped recall must be
        // project-unfiltered so memory stored under the tag in one project
        // surfaces in another. An empty project string disables ICM's default
        // cwd filter.
        let args = recall_tag("work-vpn").arguments("ignored", "ignored");
        assert_eq!(
            args["project"],
            serde_json::json!(""),
            "project must be empty to search across all projects"
        );
    }

    #[test]
    fn recall_tag_keys_on_llmenv_tag_keyword() {
        let args = recall_tag("work-vpn").arguments("ignored", "ignored");
        assert_eq!(
            args["keyword"],
            serde_json::json!("llmenv-tag:work-vpn"),
            "recall must be keyed on the llmenv-tag:<tag> encoding"
        );
        assert_eq!(args["query"], serde_json::json!("work-vpn"));
    }

    // ===== Property tests for the tag keyword + RecallTag argument shape =====

    use proptest::prelude::*;

    /// A tag accepted by `hook_run::validate_tag`.
    fn valid_tag() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{1,24}"
    }

    proptest! {
        // tag_keyword always prepends the prefix and preserves the tag exactly —
        // the keyword is `llmenv-tag:` + the unmodified tag for any valid input.
        #[test]
        fn prop_tag_keyword_is_prefix_plus_tag(tag in valid_tag()) {
            let kw = tag_keyword(&tag);
            prop_assert_eq!(&kw, &format!("{TAG_KEYWORD_PREFIX}{tag}"));
            prop_assert!(kw.starts_with(TAG_KEYWORD_PREFIX));
            prop_assert_eq!(&kw[TAG_KEYWORD_PREFIX.len()..], tag.as_str());
        }

        // RecallTag arguments are always exactly {query, project, keyword} with
        // query == tag, project == "" (cross-project), keyword == tag_keyword(tag).
        // The shape can't silently gain/lose a field or mis-bind a value.
        #[test]
        fn prop_recall_tag_arguments_shape(tag in valid_tag()) {
            let args = recall_tag(&tag).arguments("ignored", "ignored");
            let obj = args.as_object().expect("arguments must be a JSON object");
            prop_assert_eq!(obj.len(), 3, "exactly query/project/keyword");
            prop_assert_eq!(&obj["query"], &serde_json::json!(tag));
            prop_assert_eq!(&obj["project"], &serde_json::json!(""));
            prop_assert_eq!(&obj["keyword"], &serde_json::json!(tag_keyword(&tag)));
        }

        // bundle_keyword always prepends the bundle prefix and preserves the bundle
        // name exactly — the keyword is `llmenv-bundle:` + the unmodified bundle.
        #[test]
        fn prop_bundle_keyword_is_prefix_plus_bundle(bundle in valid_tag()) {
            let kw = bundle_keyword(&bundle);
            prop_assert_eq!(&kw, &format!("{BUNDLE_KEYWORD_PREFIX}{bundle}"));
            prop_assert!(kw.starts_with(BUNDLE_KEYWORD_PREFIX));
            prop_assert_eq!(&kw[BUNDLE_KEYWORD_PREFIX.len()..], bundle.as_str());
        }

        // RecallBundle arguments are always exactly {query, project, keyword} with
        // query == bundle, project == "" (cross-project), keyword == bundle_keyword(bundle).
        #[test]
        fn prop_recall_bundle_arguments_shape(bundle in valid_tag()) {
            let args = recall_bundle(&bundle).arguments("ignored", "ignored");
            let obj = args.as_object().expect("arguments must be a JSON object");
            prop_assert_eq!(obj.len(), 3, "exactly query/project/keyword");
            prop_assert_eq!(&obj["query"], &serde_json::json!(bundle));
            prop_assert_eq!(&obj["project"], &serde_json::json!(""));
            prop_assert_eq!(&obj["keyword"], &serde_json::json!(bundle_keyword(&bundle)));
        }
    }
}
