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
    fn env_vars(&self, cache_dir: &Path) -> Vec<(String, String)>;

    /// Write the manifest into `out` in the agent-native layout.
    ///
    /// Implementations must be idempotent — callers re-run after cache GC.
    ///
    /// # Errors
    /// Returns any I/O error encountered while creating directories or
    /// copying files.
    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<()>;
}
