pub mod claude_code;
pub mod crush;
pub(crate) mod skills;

use std::path::{Path, PathBuf};

use crate::merge::MergedManifest;

/// Per-agent rules for translating a [`MergedManifest`] into an on-disk layout
/// and a set of environment variables that point the agent at it.
///
/// Adapters are stateless value types; instantiate with `default()` or a unit
/// constructor at the call site.
pub trait AgentAdapter {
    /// Stable identifier used as the cache subdirectory and in diagnostics.
    fn name(&self) -> &'static str;

    /// Binary name that must be present on `PATH` for this adapter to be
    /// active. Used by [`binary_on_path`] to PATH-gate the adapter during
    /// export orchestration — if the binary is absent, the adapter is skipped
    /// entirely so a machine without the tool installed sees zero change.
    fn binary_name(&self) -> &'static str;

    /// Whether this adapter supports Claude Code–style plugins (skills,
    /// marketplaces, `installed_plugins.json`). Callers that write plugin
    /// artefacts consult this before invoking plugin rendering paths.
    fn supports_plugins(&self) -> bool;

    /// Whether this adapter supports LSP integration. Reserved for adapters
    /// that wire in language-server configuration natively; Claude Code does
    /// not (it has its own built-in language tooling).
    fn supports_lsp(&self) -> bool;

    /// Whether this adapter supports multiple model providers and
    /// default-model selection. Claude Code does not (Anthropic-only, no
    /// provider switching).
    fn supports_model_providers(&self) -> bool;

    /// The set of native hook-event names this adapter emits. Callers use this
    /// to guard event registration so events an adapter never fires are not
    /// written into its settings file.
    fn supported_hook_events(&self) -> &'static [&'static str];

    /// Environment variables the shell hook should `export` so the agent
    /// discovers `cache_dir` as its config root and `state_dir` for durable state.
    ///
    /// Implementations may create adapter-specific subdirectories under
    /// `state_dir` as a side effect (e.g. so a directory referenced by an emitted
    /// env var exists on disk before the agent launches) — this is the only place
    /// that knows both the exact path and that it must exist.
    ///
    /// # Arguments
    /// * `cache_dir` — hashed config directory (garbage-collected on content change)
    /// * `state_dir` — stable state directory (persists across config changes)
    ///
    /// # Errors
    /// Returns an error if either path is not valid UTF-8 — env vars cannot
    /// carry arbitrary bytes on all platforms, so callers that surface a
    /// non-UTF-8 path should fail loudly rather than emit a lossy path the agent
    /// will silently mis-parse. Also returns an error if creating a required
    /// subdirectory fails.
    fn env_vars(&self, cache_dir: &Path, state_dir: &Path)
    -> anyhow::Result<Vec<(String, String)>>;

    /// Write the manifest into `out` in the agent-native layout, returning the
    /// set of paths the adapter wrote, each relative to `out`. The returned set
    /// is llmenv's *owned* set for `out`: callers union it with the generic
    /// copied files to build the [`crate::materialize::manifest::CacheManifest`]
    /// and to reconcile ghost files on a version-mode re-render (#196).
    ///
    /// Implementations must be idempotent — callers re-run after cache GC and
    /// re-render in place in version mode. Files an implementation merges over
    /// (rather than overwrites) to preserve foreign in-session state — e.g.
    /// `settings.json`, which a plugin may self-register hooks into (#175) — are
    /// still reported as owned, because llmenv authored their llmenv-controlled
    /// keys.
    ///
    /// # Errors
    /// Returns any I/O error encountered while creating directories or
    /// copying files.
    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>>;

    /// Format injected hook context in the engine's native hook-output shape so
    /// the agent runtime adds it to the model's context. Empty input returns an
    /// empty string, which suppresses any output.
    ///
    /// # Arguments
    /// * `hook_event_name` — the event name from the hook payload (e.g.
    ///   `"SessionStart"`), echoed back as `hookEventName` inside
    ///   `hookSpecificOutput` for runtimes that validate it.
    /// * `text` — the injected memory context, placed as `additionalContext`
    ///   inside `hookSpecificOutput`.
    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String;
}

/// Detect which adapter is running in the current process by checking each
/// registered adapter's environment signal. Falls back to Claude Code when
/// no signal is found (backward-compatible default).
///
/// Used by hook-run and throttle subcommands that are invoked as subprocesses
/// by the LLM CLI and don't receive the adapter identity through stdin.
#[must_use]
pub fn active_adapter() -> Box<dyn AgentAdapter> {
    registered_adapters()
        .into_iter()
        .find(|a| match a.name() {
            "claude-code" => std::env::var("CLAUDE_CONFIG_DIR").is_ok(),
            "crush" => std::env::var("CRUSH_GLOBAL_CONFIG").is_ok(),
            _ => false,
        })
        .unwrap_or_else(|| Box::new(claude_code::ClaudeCodeAdapter))
}

/// Returns every adapter llmenv ships with, in preference order.
///
/// Callers PATH-gate each entry via [`binary_on_path`] before activating it,
/// so adapters for tools the user has not installed are silently skipped.
///
/// # Extending the registry
/// Add new adapters here once their crate is wired in:
pub fn registered_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(crush::CrushAdapter),
    ]
}

/// Resolve an adapter by its engine ID (the underscore form from `--engine` flags,
/// e.g. `"claude_code"` or `"crush"`). Falls back to env-sniffing
/// [`active_adapter`] when no registered adapter matches the given engine ID.
///
/// Used by hook-run to honour the caller's `--engine` flag instead of
/// re-sniffing environment variables for adapter detection.
#[must_use]
pub fn adapter_for_engine(engine: &str) -> Box<dyn AgentAdapter> {
    registered_adapters()
        .into_iter()
        .find(|a| engine_id(a.as_ref()) == engine)
        .unwrap_or_else(active_adapter)
}

/// Normalise an adapter's identity to the underscore form used by `--engine`
/// flags, `native.<engine>` config keys, and `disabled_engines` entries.
/// [`AgentAdapter::name`] is the hyphenated cache-dir form (`claude-code`);
/// this converts it to `claude_code` for comparison against those
/// user-facing engine-id strings.
#[must_use]
pub(crate) fn engine_id(adapter: &dyn AgentAdapter) -> String {
    adapter.name().replace('-', "_")
}

/// Every registered adapter's [`engine_id`], for validating user-facing
/// engine-id strings (`--engine`, `disabled_engines`) against what's actually
/// registered.
#[must_use]
pub(crate) fn known_engine_ids() -> Vec<String> {
    registered_adapters()
        .iter()
        .map(|a| engine_id(a.as_ref()))
        .collect()
}

/// Returns `true` when `name` resolves to an executable on the current `PATH`.
///
/// Uses the platform `which` command so the result matches what a shell would
/// find. Returns `false` on any I/O error or when `which` exits non-zero.
///
/// Names containing `/` or ASCII whitespace are unconditionally rejected;
/// they cannot be plain binary names and would produce confusing `which` behaviour.
#[must_use]
pub fn binary_on_path(name: &str) -> bool {
    if name.contains('/') || name.chars().any(char::is_whitespace) {
        return false;
    }
    std::process::Command::new("which")
        .arg(name)
        .output()
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

/// Resolve bundle-relative paths in a hook command string.
/// Scans whitespace-separated tokens and resolves those containing '/' (but not
/// starting with '/', '~', '$', or '-') to absolute paths relative to `bundle_dir`.
///
/// Shared across adapters: any engine that renders a hook `command` string must
/// resolve bundle-relative script paths the same way, since a bundle is authored
/// once and materialized for every engine.
pub(crate) fn resolve_bundle_relative_paths(command: &str, bundle_dir: &Path) -> Option<String> {
    let mut resolved = false;
    let mut result = String::new();
    for (i, token) in command.split_whitespace().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        if token.contains('/')
            && !token.starts_with('/')
            && !token.starts_with('~')
            && !token.starts_with('$')
            && !token.starts_with('-')
            && !crate::paths::is_unsafe_join_target(token)
        {
            let abs_path = bundle_dir.join(token);
            result.push_str(&abs_path.to_string_lossy());
            resolved = true;
        } else {
            result.push_str(token);
        }
    }
    if resolved { Some(result) } else { None }
}

/// Rewrite bundle-authored hook commands that reference files copied into the
/// cache directory, even when the command uses shell variables or absolute
/// paths that `resolve_bundle_relative_paths` cannot match.
///
/// For each whitespace-delimited token that contains `/`, checks whether the
/// token **ends with** any relative path in `known_files` at a path-component
/// boundary. When it does, the matched suffix is replaced with
/// `cache_dir.join(rel)`, re-anchoring the reference to the materialized copy.
/// When multiple known files match the same token, the **longest** suffix wins.
/// Tokens that don't match any known file are left untouched.
///
/// This handles cases like:
/// ```text
/// bash ${HOME}/git/my-llmenv/bundles/base/hooks/guard.sh
/// ```
/// where the token `${HOME}/git/my-llmenv/bundles/base/hooks/guard.sh` ends
/// with `hooks/guard.sh` — a file that was copied into the cache.
pub(crate) fn resolve_command_paths_against_files(
    command: &str,
    cache_dir: &Path,
    known_files: &std::collections::BTreeMap<PathBuf, PathBuf>,
) -> Option<String> {
    // Pre-compute string representations once so the inner loop stays O(1)
    // per candidate rather than O(files) allocations.
    // Sort by key length descending so the first filter+max_by_key pass
    // naturally prefers the longest (most specific) suffix.
    let mut candidates: Vec<(&Path, String)> = known_files
        .keys()
        .map(|k| {
            let s = k.to_string_lossy().into_owned();
            (k.as_path(), s)
        })
        .collect();
    candidates.sort_by_key(|(_, b)| std::cmp::Reverse(b.len()));

    let mut resolved = false;
    let mut result = String::new();
    for (i, token) in command.split_whitespace().enumerate() {
        if i > 0 {
            result.push(' ');
        }
        // Unlike resolve_bundle_relative_paths, we never join the token
        // itself — the join operand is `rel`, a trusted key from known_files.
        // So is_unsafe_join_target on the token is not needed here; absolute
        // paths and even `../`-prefixed paths can be safely suffix-matched.
        if token.contains('/')
            && let Some((rel, _suffix)) = candidates.iter().find(|(_, s)| {
                // Require a path-component boundary before the suffix:
                // the suffix starts at position 0 in the token, or the
                // character immediately before it is '/'.
                let prefix_len = token.len().saturating_sub(s.len());
                token.ends_with(s.as_str())
                    && (prefix_len == 0 || token.as_bytes().get(prefix_len - 1) == Some(&b'/'))
            })
        {
            // Defense in depth: rel is trusted (it came from a filesystem
            // walk + strip_prefix), but guard against future changes that add
            // user-supplied paths to known_files.
            debug_assert!(
                !crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()),
                "known_files key contains traversal: {}",
                rel.display()
            );
            let abs_path = cache_dir.join(rel);
            result.push_str(&abs_path.to_string_lossy());
            resolved = true;
            continue;
        }
        result.push_str(token);
    }
    if resolved { Some(result) } else { None }
}

/// Map a resolved remote transport onto the `type` discriminator string shared
/// by every engine's remote-MCP config shape (`"http"` / `"sse"`).
///
/// `ResolvedKind::Remote` never actually carries `McpTransport::Stdio` (stdio
/// servers always resolve to `ResolvedKind::Stdio` instead — see
/// `crate::mcp::resolve`), so that arm is unreachable in practice; it is
/// folded to `"http"` defensively rather than panicking.
pub(crate) fn remote_transport_type_str(transport: crate::config::McpTransport) -> &'static str {
    use crate::config::McpTransport;
    match transport {
        McpTransport::Sse => "sse",
        McpTransport::Http | McpTransport::Stdio => "http",
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        binary_on_path, engine_id, known_engine_ids, registered_adapters,
        remote_transport_type_str, resolve_bundle_relative_paths,
        resolve_command_paths_against_files,
    };

    #[test]
    fn registry_contains_claude_and_crush_adapters() {
        let adapters = registered_adapters();
        assert_eq!(
            adapters.len(),
            2,
            "registry should have exactly two adapters"
        );
        assert_eq!(adapters[0].name(), "claude-code");
        assert_eq!(adapters[1].name(), "crush");
    }

    #[test]
    fn registry_adapter_trait_probes() {
        let adapters = registered_adapters();

        // ClaudeCodeAdapter
        let a = &*adapters[0];
        assert_eq!(a.binary_name(), "claude");
        assert!(a.supports_plugins(), "ClaudeCodeAdapter supports plugins");
        assert!(a.supports_lsp(), "ClaudeCodeAdapter supports LSP (#556)");
        assert!(
            !a.supports_model_providers(),
            "ClaudeCodeAdapter does not support model providers"
        );
        let events = a.supported_hook_events();
        for expected in [
            "SessionStart",
            "SessionEnd",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "Stop",
            "SubagentStop",
            "PreCompact",
        ] {
            assert!(
                events.contains(&expected),
                "supported_hook_events missing {expected}"
            );
        }

        // CrushAdapter
        let c = &*adapters[1];
        assert_eq!(c.binary_name(), "crush");
        assert!(
            !c.supports_plugins(),
            "CrushAdapter does not support plugins"
        );
        assert!(c.supports_lsp(), "CrushAdapter supports LSP");
        assert!(
            c.supports_model_providers(),
            "CrushAdapter supports model providers"
        );
        assert!(
            c.supported_hook_events().contains(&"PreToolUse"),
            "CrushAdapter must support PreToolUse"
        );
    }

    #[test]
    fn engine_id_normalises_hyphen_to_underscore() {
        let adapters = registered_adapters();
        assert_eq!(engine_id(adapters[0].as_ref()), "claude_code");
        assert_eq!(engine_id(adapters[1].as_ref()), "crush");
    }

    #[test]
    fn known_engine_ids_matches_registered_adapters() {
        assert_eq!(known_engine_ids(), vec!["claude_code", "crush"]);
    }

    #[test]
    fn binary_on_path_true_for_sh() {
        assert!(binary_on_path("sh"), "sh must be on PATH in any POSIX env");
    }

    #[test]
    fn binary_on_path_false_for_bogus_binary() {
        assert!(
            !binary_on_path("__llmenv_no_such_binary_xyzzy__"),
            "bogus binary must not be found on PATH"
        );
    }

    #[test]
    fn binary_on_path_rejects_slash() {
        assert!(
            !binary_on_path("/usr/bin/sh"),
            "path with '/' must be rejected without spawning which"
        );
    }

    #[test]
    fn binary_on_path_rejects_whitespace() {
        assert!(
            !binary_on_path("sh -c echo"),
            "name with whitespace must be rejected without spawning which"
        );
        assert!(
            !binary_on_path("sh\techo"),
            "name with tab must be rejected without spawning which"
        );
    }

    #[test]
    fn engine_id_matches_baked_engine_flag_default() {
        // The `--engine` flag default baked into hook commands is the underscore
        // form of an adapter's name (`claude_code`), while name() is hyphenated
        // (`claude-code`). Guard that at least one registered adapter's normalised
        // identity equals the baked default, so warn_if_unknown_engine (which
        // normalises the same way) never spuriously warns on the default path.
        let adapters = registered_adapters();
        assert!(
            adapters
                .iter()
                .any(|a| engine_id(a.as_ref()) == "claude_code"),
            "no registered adapter's engine id matches the baked --engine default 'claude_code'"
        );
    }

    #[test]
    fn resolve_bundle_relative_paths_rewrites_relative_token() {
        let dir = std::path::Path::new("/bundles/foo");
        let resolved = resolve_bundle_relative_paths("bash hooks/guard.sh", dir);
        assert_eq!(
            resolved,
            Some("bash /bundles/foo/hooks/guard.sh".to_string())
        );
    }

    #[test]
    fn resolve_bundle_relative_paths_leaves_absolute_and_shell_tokens_alone() {
        let dir = std::path::Path::new("/bundles/foo");
        assert!(resolve_bundle_relative_paths("bash /abs/path.sh", dir).is_none());
        assert!(resolve_bundle_relative_paths("bash ${HOME}/x.sh", dir).is_none());
        assert!(resolve_bundle_relative_paths("bash ~/x.sh", dir).is_none());
        assert!(resolve_bundle_relative_paths("echo hello", dir).is_none());
    }

    #[test]
    fn remote_transport_type_str_maps_http_and_sse() {
        use crate::config::McpTransport;
        assert_eq!(remote_transport_type_str(McpTransport::Http), "http");
        assert_eq!(remote_transport_type_str(McpTransport::Sse), "sse");
        assert_eq!(
            remote_transport_type_str(McpTransport::Stdio),
            "http",
            "unreachable in practice, but must not panic"
        );
    }

    // ---- resolve_command_paths_against_files ----

    fn known_files_from_paths(paths: &[&str]) -> std::collections::BTreeMap<PathBuf, PathBuf> {
        paths
            .iter()
            .map(|p| (PathBuf::from(p), PathBuf::from(format!("/source/{p}"))))
            .collect()
    }

    #[test]
    fn suffix_matches_shell_var_prefixed_token() {
        let files = known_files_from_paths(&["hooks/guard.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files(
            "bash ${HOME}/bundles/base/hooks/guard.sh",
            cache,
            &files,
        );
        assert_eq!(resolved, Some("bash /cache/hooks/guard.sh".to_string()));
    }

    #[test]
    fn picks_longest_suffix_when_multiple_match() {
        let files = known_files_from_paths(&["guard.sh", "hooks/guard.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files(
            "bash ${HOME}/bundles/base/hooks/guard.sh",
            cache,
            &files,
        );
        assert_eq!(
            resolved,
            Some("bash /cache/hooks/guard.sh".to_string()),
            "must pick hooks/guard.sh (longer), not guard.sh"
        );
    }

    #[test]
    fn requires_path_component_boundary_before_suffix() {
        // "my-hooks/guard.sh" ends with "hooks/guard.sh" but the substring
        // crosses a component boundary — it should not match.
        let files = known_files_from_paths(&["hooks/guard.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files("bash my-hooks/guard.sh", cache, &files);
        assert_eq!(
            resolved, None,
            "must not match suffix that crosses a path-component boundary"
        );
    }

    #[test]
    fn matches_absolute_path_token() {
        // Absolute-path tokens are not blocked — the join operand is the
        // trusted `rel` key, not the untrusted token.
        let files = known_files_from_paths(&["hooks/guard.sh"]);
        let cache = Path::new("/cache");
        let resolved =
            resolve_command_paths_against_files("bash /abs/path/hooks/guard.sh", cache, &files);
        assert_eq!(resolved, Some("bash /cache/hooks/guard.sh".to_string()));
    }

    #[test]
    fn empty_known_files_never_matches() {
        let files: std::collections::BTreeMap<PathBuf, PathBuf> = std::collections::BTreeMap::new();
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files("bash hooks/guard.sh", cache, &files);
        assert_eq!(resolved, None);
    }

    #[test]
    fn token_without_slash_never_matches() {
        let files = known_files_from_paths(&["guard.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files("bash guard.sh", cache, &files);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolves_multiple_tokens_in_command() {
        let files = known_files_from_paths(&["hooks/pre.sh", "hooks/post.sh"]);
        let cache = Path::new("/cache");
        let resolved = resolve_command_paths_against_files(
            "bash /some/where/hooks/pre.sh /other/where/hooks/post.sh",
            cache,
            &files,
        );
        assert_eq!(
            resolved,
            Some("bash /cache/hooks/pre.sh /cache/hooks/post.sh".to_string())
        );
    }
}
