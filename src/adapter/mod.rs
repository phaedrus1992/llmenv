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

    /// The name of the binary on PATH that indicates this adapter is installed.
    /// Used by [`binary_on_path`] to gate materialization.
    fn binary_name(&self) -> &'static str;
}

/// Every registered adapter, in preference order.
///
/// # Extending the registry
/// Add new adapters here once their crate is wired in.
pub fn registered_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![Box::new(claude_code::ClaudeCodeAdapter)]
}

/// Check whether `name` is an executable on PATH.
pub fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let full = dir.join(name);
            full.is_file()
        })
    })
}
