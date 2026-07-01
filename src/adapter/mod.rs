pub mod claude_code;

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

    /// The set of native hook-event names this adapter emits. Callers use this
    /// to guard event registration so events an adapter never fires are not
    /// written into its settings file.
    fn supported_hook_events(&self) -> &'static [&'static str];

    /// Environment variables the shell hook should `export` so the agent
    /// discovers `cache_dir` as its config root.
    ///
    /// # Errors
    /// Returns an error if `cache_dir` is not valid UTF-8 — env vars cannot
    /// carry arbitrary bytes on all platforms, so callers that surface a
    /// non-UTF-8 cache root should fail loudly rather than emit a lossy
    /// path the agent will silently mis-parse.
    fn env_vars(&self, cache_dir: &Path) -> anyhow::Result<Vec<(String, String)>>;

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

/// Returns every adapter llmenv ships with, in preference order.
///
/// Callers PATH-gate each entry via [`binary_on_path`] before activating it,
/// so adapters for tools the user has not installed are silently skipped.
///
/// # Extending the registry
/// Add new adapters here once their crate is wired in:
// #506: CrushAdapter appended here
pub fn registered_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![Box::new(claude_code::ClaudeCodeAdapter)]
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

#[cfg(test)]
mod tests {
    use super::{binary_on_path, registered_adapters};

    #[test]
    fn registry_contains_exactly_claude_adapter() {
        let adapters = registered_adapters();
        assert_eq!(
            adapters.len(),
            1,
            "registry should have exactly one adapter"
        );
        assert_eq!(adapters[0].name(), "claude-code");
    }

    #[test]
    fn registry_adapter_trait_probes() {
        let adapters = registered_adapters();
        let a = &*adapters[0];
        assert_eq!(a.binary_name(), "claude");
        assert!(a.supports_plugins(), "ClaudeCodeAdapter supports plugins");
        assert!(!a.supports_lsp(), "ClaudeCodeAdapter does not support LSP");
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
}
