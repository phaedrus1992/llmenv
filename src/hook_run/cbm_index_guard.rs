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
//! A scoping root wouldn't help here either way (llmenv no longer sets
//! `CBM_ALLOWED_ROOT` at all — #1495): it bounds the tree that gets *read*,
//! not the project key that gets *written*. codebase-memory-mcp's own
//! default `CBM_CACHE_DIR` is one directory shared by every project, so
//! every project a user has indexed is a reachable target.
//!
//! Re-tiering `index_repository` to `ask` was the obvious alternative and is
//! the wrong trade: llmenv fires it on every `SessionStart`, so a prompt would
//! land in every session and the feature's whole point is that it doesn't.
//! Denying the `name` override specifically keeps the auto-index unprompted —
//! llmenv's own call passes `repo_path` alone (see
//! `build_index_repository_command`), so nothing llmenv does trips this.
//!
//! The same hook also denies `persistence: true`, which makes the tool write
//! `.codebase-memory/graph.db.zst` into the indexed repository. The tool sits in
//! the unprompted `Mutation` tier, so without this a model could add that
//! artifact to a repo silently.
//!
//! Stateless, like `cd_guard`: the decision comes from the current call's
//! arguments alone.

/// The `__DENY__` text for an `index_repository` call whose `repo_path` is outside the roots
/// codebase-memory-mcp may index (#2406), or an empty string.
pub(crate) fn handle_roots(
    stdin_payload: &serde_json::Value,
    config: &crate::config::Config,
) -> String {
    if stdin_payload.get("tool_name").and_then(|v| v.as_str()) != Some(INDEX_REPOSITORY_TOOL) {
        return String::new();
    }
    let Some(repo_path) = stdin_payload
        .get("tool_input")
        .and_then(|v| v.get("repo_path"))
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
    else {
        return String::new();
    };
    // The hook runs in the project folder, as the SessionStart index does.
    let project_root = match std::env::current_dir() {
        Ok(dir) => dir,
        // The guard denies a call outside the allowed roots, so a folder that cannot be read
        // denies too. An allow here would open the guard on the one input it cannot check.
        Err(e) => {
            return format!(
                "__DENY__:cannot check {repo_path} against the codebase-memory allowed roots: \
                 the session folder cannot be read: {e}"
            );
        }
    };
    crate::mcp::cbm_roots::guard_decision(repo_path, config, &project_root)
        .map_or_else(String::new, |reason| format!("__DENY__:{reason}"))
}

/// Tool name this guard fires on. Callers register a `PreToolUse` matcher for
/// exactly this string, so the guard costs one anchored regex when idle.
pub(crate) const INDEX_REPOSITORY_TOOL: &str = "mcp__codebase-memory-mcp__index_repository";

/// Handle a `PreToolUse` event for codebase-memory-mcp's `index_repository`.
/// Returns a `__DENY__:`-prefixed reason when the call carries a `name`
/// override or `persistence: true`, or an empty string when it doesn't apply
/// (different tool, or neither — the shape llmenv's own auto-index uses).
/// The `name` reason wins when both are present.
pub(crate) fn handle_pre_tool_use(stdin_payload: &serde_json::Value) -> String {
    if stdin_payload.get("tool_name").and_then(|v| v.as_str()) != Some(INDEX_REPOSITORY_TOOL) {
        return String::new();
    }
    let input = stdin_payload.get("tool_input");
    // Absent, null, or empty `name` all mean "derive the key from repo_path",
    // which is the safe path. Only a non-empty override can land on another
    // project's key.
    if let Some(name) = input
        .and_then(|v| v.get("name"))
        .and_then(|v| v.as_str())
        .filter(|n| !n.trim().is_empty())
    {
        return name_override_reason(name);
    }
    // A non-boolean `persistence` is invalid input that the tool rejects itself.
    if input
        .and_then(|v| v.get("persistence"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return PERSISTENCE_REASON.to_string();
    }
    String::new()
}

const PERSISTENCE_REASON: &str = "__DENY__:llmenv blocked `index_repository` with \
    persistence=true. It writes .codebase-memory/graph.db.zst into the repository. Call it \
    without persistence. To share a graph artifact on purpose, run codebase-memory-mcp from a \
    shell.";

fn name_override_reason(name: &str) -> String {
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
    fn denies_persistence_true() {
        let out = handle_pre_tool_use(&payload(
            INDEX_REPOSITORY_TOOL,
            serde_json::json!({ "repo_path": "/repo", "persistence": true }),
        ));
        assert!(out.starts_with("__DENY__:"), "expected a deny, got {out:?}");
        assert!(
            out.contains("graph.db.zst"),
            "reason names the artifact: {out:?}"
        );
    }

    #[test]
    fn allows_persistence_that_is_not_true() {
        for persistence in [
            serde_json::json!(false),
            serde_json::Value::Null,
            serde_json::json!("true"),
        ] {
            assert_eq!(
                handle_pre_tool_use(&payload(
                    INDEX_REPOSITORY_TOOL,
                    serde_json::json!({ "repo_path": "/repo", "persistence": persistence }),
                )),
                "",
                "{persistence:?} is not a request to persist"
            );
        }
    }

    #[test]
    fn name_reason_wins_over_persistence() {
        let out = handle_pre_tool_use(&payload(
            INDEX_REPOSITORY_TOOL,
            serde_json::json!({ "name": "other-project", "persistence": true }),
        ));
        assert!(out.contains("other-project"), "{out:?}");
        assert!(!out.contains("graph.db.zst"), "{out:?}");
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

    fn config_with_entry(index_path: &std::path::Path) -> crate::config::Config {
        crate::config::Config {
            features: Some(crate::config::Features {
                codebase_memory: vec![crate::config::CodebaseMemory {
                    when: vec!["p".into()],
                    index_path: Some(index_path.display().to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_roots_guard_denies_a_folder_outside_the_roots_and_allows_the_project() {
        let cache = tempfile::tempdir().unwrap();
        let config = config_with_entry(cache.path());
        let project = std::env::current_dir().unwrap();
        let inside = payload(
            INDEX_REPOSITORY_TOOL,
            serde_json::json!({ "repo_path": project.display().to_string() }),
        );
        assert_eq!(handle_roots(&inside, &config), "");
        let outside = payload(
            INDEX_REPOSITORY_TOOL,
            serde_json::json!({ "repo_path": "/usr" }),
        );
        let out = handle_roots(&outside, &config);
        assert!(out.starts_with("__DENY__:"), "{out:?}");
        assert!(out.contains("allowed_roots"), "{out:?}");
        // A blank path is not a repo_path: the server rejects it, the guard does not.
        let blank = payload(
            INDEX_REPOSITORY_TOOL,
            serde_json::json!({ "repo_path": "  " }),
        );
        assert_eq!(handle_roots(&blank, &config), "");
        // Another tool is never guarded.
        let other = payload("Bash", serde_json::json!({ "repo_path": "/usr" }));
        assert_eq!(handle_roots(&other, &config), "");
    }

    // Changes the working folder of the process, so it needs a process of its own: nextest runs
    // each test in one.
    #[test]
    fn the_roots_guard_denies_when_the_session_folder_cannot_be_read() {
        let cache = tempfile::tempdir().unwrap();
        let config = config_with_entry(cache.path());
        let gone = tempfile::tempdir().unwrap();
        std::env::set_current_dir(gone.path()).unwrap();
        std::fs::remove_dir(gone.path()).unwrap();
        let call = payload(
            INDEX_REPOSITORY_TOOL,
            serde_json::json!({ "repo_path": "/usr" }),
        );
        let out = handle_roots(&call, &config);
        assert!(out.starts_with("__DENY__:"), "{out:?}");
        assert!(out.contains("cannot be read"), "{out:?}");
    }

    #[test]
    fn the_roots_guard_ignores_other_tools_and_calls_without_a_repo_path() {
        let config = crate::config::Config::default();
        for p in [
            payload("Bash", serde_json::json!({ "repo_path": "/x" })),
            payload(INDEX_REPOSITORY_TOOL, serde_json::json!({})),
            payload(
                INDEX_REPOSITORY_TOOL,
                serde_json::json!({ "repo_path": "  " }),
            ),
            payload(INDEX_REPOSITORY_TOOL, serde_json::json!({ "repo_path": 7 })),
            // No codebase_memory entry: the guard has no roots to enforce.
            payload(
                INDEX_REPOSITORY_TOOL,
                serde_json::json!({ "repo_path": "/x" }),
            ),
        ] {
            assert_eq!(handle_roots(&p, &config), "", "{p}");
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
