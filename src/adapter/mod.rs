pub mod claude_code;

use std::path::Path;

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

    /// Write the manifest into `out` in the agent-native layout.
    ///
    /// Implementations must be idempotent — callers re-run after cache GC.
    ///
    /// # Errors
    /// Returns any I/O error encountered while creating directories or
    /// copying files.
    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<()>;

    /// Format injected hook context in the engine's native hook-output shape so
    /// the agent runtime adds it to the model's context. Empty input returns an
    /// empty string, which suppresses any output.
    fn emit_hook_context(&self, text: &str) -> String;
}
