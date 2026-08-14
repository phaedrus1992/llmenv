//! `PreToolUse` guard against codebase-memory-mcp's project-name clobber
//! (#1331, upstream `DeusData/codebase-memory-mcp#1578`).
//!
//! `index_repository` takes a `name` parameter that overrides the project key
//! derived from `repo_path`. Upstream validates it only for path-traversal
//! characters — nothing checks whether the name already belongs to a
//! *different* repo. Since the index lives at `<CBM_CACHE_DIR>/<name>.db` and
//! a full reindex unlinks and recreates that file, one call can silently
//! replace an unrelated project's index with this repo's data.
//!
//! `CBM_ALLOWED_ROOT` doesn't help: llmenv pins it to the project root, which
//! bounds the tree that gets *read*, not the project key that gets *written*.
//! llmenv's default `CBM_CACHE_DIR` is one directory shared by every project,
//! so every project a user has indexed is a reachable target.
//!
//! Re-tiering `index_repository` to `ask` was the obvious alternative and is
//! the wrong trade: llmenv fires it on every `SessionStart`, so a prompt would
//! land in every session and the feature's whole point is that it doesn't.
//! Denying the `name` override specifically keeps the auto-index unprompted —
//! llmenv's own call passes `repo_path` alone (see
//! `build_index_repository_command`), so nothing llmenv does trips this.
//!
//! Stateless, like `cd_guard`: the decision comes from the current call's
//! arguments alone.

/// Tool name this guard fires on. Callers register a `PreToolUse` matcher for
/// exactly this string, so the guard costs one anchored regex when idle.
pub(crate) const INDEX_REPOSITORY_TOOL: &str = "mcp__codebase-memory-mcp__index_repository";

/// Handle a `PreToolUse` event for codebase-memory-mcp's `index_repository`.
/// Returns a `__DENY__:`-prefixed reason when the call carries a `name`
/// override, or an empty string when it doesn't apply (different tool, or no
/// override — the shape llmenv's own auto-index uses).
pub(crate) fn handle_pre_tool_use(stdin_payload: &serde_json::Value) -> String {
    if stdin_payload.get("tool_name").and_then(|v| v.as_str()) != Some(INDEX_REPOSITORY_TOOL) {
        return String::new();
    }
    // Absent, null, or empty `name` all mean "derive the key from repo_path",
    // which is the safe path. Only a non-empty override can land on another
    // project's key.
    let Some(name) = stdin_payload
        .get("tool_input")
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str())
        .filter(|n| !n.trim().is_empty())
    else {
        return String::new();
    };
    format!(
        "__DENY__:llmenv blocked `index_repository` with name=\"{name}\". The name overrides the \
         project key the index is written under, and codebase-memory-mcp doesn't check whether \
         that key already belongs to a different repository — the call would replace that \
         project's index with this one's (upstream DeusData/codebase-memory-mcp#1578). Re-run \
         without `name` to index this repository under its own key. If you genuinely need a \
         custom key, run codebase-memory-mcp directly so the overwrite is a deliberate choice."
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn payload(tool: &str, input: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "tool_name": tool, "tool_input": input })
    }

    #[test]
    fn denies_a_name_override() {
        let out = handle_pre_tool_use(&payload(
            INDEX_REPOSITORY_TOOL,
            serde_json::json!({ "repo_path": "/repo", "name": "other-project" }),
        ));
        assert!(out.starts_with("__DENY__:"), "expected a deny, got {out:?}");
        assert!(
            out.contains("other-project"),
            "reason names the key: {out:?}"
        );
    }

    #[test]
    fn allows_the_shape_llmenv_itself_sends() {
        // `build_index_repository_command` passes `repo_path` and nothing
        // else; if this ever denied, every SessionStart auto-index would die.
        assert_eq!(
            handle_pre_tool_use(&payload(
                INDEX_REPOSITORY_TOOL,
                serde_json::json!({ "repo_path": "/repo" }),
            )),
            ""
        );
    }

    #[test]
    fn allows_an_empty_or_null_name() {
        for name in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("   "),
        ] {
            assert_eq!(
                handle_pre_tool_use(&payload(
                    INDEX_REPOSITORY_TOOL,
                    serde_json::json!({ "repo_path": "/repo", "name": name }),
                )),
                "",
                "{name:?} means derive-from-repo_path, not an override"
            );
        }
    }

    #[test]
    fn ignores_other_tools_including_sibling_cbm_calls() {
        for tool in [
            "Bash",
            "mcp__codebase-memory-mcp__search_code",
            "mcp__codebase-memory-mcp__delete_project",
            // Substring, not the tool: an unanchored match would deny this.
            "mcp__other__mcp__codebase-memory-mcp__index_repository",
        ] {
            assert_eq!(
                handle_pre_tool_use(&payload(tool, serde_json::json!({ "name": "victim" }))),
                "",
                "{tool} is not the guarded tool"
            );
        }
    }

    #[test]
    fn tolerates_a_malformed_payload() {
        for p in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({ "tool_name": INDEX_REPOSITORY_TOOL }),
            serde_json::json!({ "tool_name": INDEX_REPOSITORY_TOOL, "tool_input": 7 }),
            // A non-string `name` can't be a project key; upstream would
            // reject it, and guessing an intent here would be worse.
            serde_json::json!({
                "tool_name": INDEX_REPOSITORY_TOOL,
                "tool_input": { "name": ["a"] },
            }),
        ] {
            assert_eq!(handle_pre_tool_use(&p), "", "{p} should pass through");
        }
    }

    proptest! {
        /// Any non-blank name denies, and the deny always keeps the prefix
        /// `run()` looks for — a reason that lost it would silently become an
        /// allow.
        #[test]
        fn every_non_blank_name_is_denied(name in "\\PC{1,64}") {
            let out = handle_pre_tool_use(&payload(
                INDEX_REPOSITORY_TOOL,
                serde_json::json!({ "repo_path": "/repo", "name": name }),
            ));
            if name.trim().is_empty() {
                prop_assert_eq!(out, "");
            } else {
                prop_assert!(out.starts_with("__DENY__:"));
            }
        }
    }
}
