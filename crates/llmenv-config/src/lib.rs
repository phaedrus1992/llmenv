mod schema;
mod template;
mod validate;

pub const STATE_DIR_ENV: &str = "LLMENV_STATE_DIR";
pub const RESERVED_STATE_ENV_VARS: &[&str] = &[STATE_DIR_ENV, "CLAUDE_CONFIG_DIR"];
pub const MEMORY_MCP_NAME: &str = "icm";
/// Marketplace registration name for the built-in context-mode plugin.
pub const CONTEXT_MODE_MARKETPLACE: &str = "context-mode";
/// Canonical git source for the built-in context-mode plugin, pinned to a
/// fixed release tag (#496) — an unpinned floating `HEAD` ref would make
/// `llmenv regenerate` non-reproducible across time (whatever the upstream
/// repo currently has). Bump this deliberately as part of a llmenv release,
/// not automatically. `#<ref>` is llmenv's own marketplace-source pin syntax
/// (see `split_source_ref` in `src/plugins/cache.rs`), not a URL fragment.
pub const CONTEXT_MODE_SOURCE: &str = "https://github.com/mksglu/context-mode#v1.0.169";
/// Plugin name inside the context-mode marketplace.
pub const CONTEXT_MODE_PLUGIN: &str = "context-mode";
/// MCP tool-name prefix Claude Code assigns the context-mode plugin's server.
pub const CONTEXT_MODE_MCP_PREFIX: &str = "mcp__plugin_context-mode_context-mode__";
/// Env var context-mode honors to relocate its FTS5 store (#175 durable dir).
pub const CONTEXT_MODE_DATA_ENV: &str = "CONTEXT_MODE_DATA_DIR";
/// Durable-state subdir name for context-mode's store.
pub const CONTEXT_MODE_STATE_SUBDIR: &str = "context-mode";

pub use schema::{
    Bundle, Cache, Capabilities, Config, ConsolidationBackend, ConsolidationConfig, ContentMatch,
    ContentScope, ContextMode, EnvVar, Features, FileSinkConfig, HashingMode, Hook, HookHandler,
    HookHandlerKind, HostEntry, HostMatch, HostScope, ImportanceLevel, InitConfig, LogLevel,
    LspServer, Marketplace, MarketplaceSource, McpServer, McpTransport, Memory, MemoryType,
    ModelCost, ModelProvider, ModelRef, ModelSource, NativePermissionRules, NetworkMatch,
    NetworkScope, OFFICIAL_MARKETPLACE_OWNER, PermissionMode, PermissionRule, Permissions,
    PluginCollection, RESERVED_OFFICIAL_MARKETPLACES, ReadOnce, ReadOnceMode, Scopes, SessionLog,
    SkillSource, SlippageControl, StateConfig, StateTool, Throttle, TranscriptSinkConfig,
    UpgradeConfig, UpgradeTrack, UserMatch, UserScope, classify_source, github_owner_repo,
    is_reserved_official_marketplace, split_plugin_ref,
};
pub use template::generate_template;
pub use validate::{
    ValidateError, validate_capabilities_env_key, validate_permission_rule,
    validate_permission_string,
};

use anyhow::Context;
use std::path::Path;

impl Config {
    /// Returns `true` when `features.context_mode.enabled` is set.
    pub fn context_mode_enabled(&self) -> bool {
        self.features
            .as_ref()
            .and_then(|f| f.context_mode.as_ref())
            .is_some_and(|c| c.enabled)
    }

    /// Effective session-logging config: an absent block means ICM transcript
    /// on, file + verbose off.
    #[must_use]
    pub fn session_log_resolved(&self) -> SessionLog {
        self.session_log.clone().unwrap_or_default()
    }

    /// Load and validate a config from an **already-expanded** path.
    ///
    /// `load` does not perform tilde (`~`) expansion — the caller is
    /// responsible for expanding `~`/`~user` (e.g. via `llmenv_paths`) before
    /// calling. A `debug_assert` guards this contract in debug builds.
    ///
    /// # Errors
    /// Returns an error if the file can't be read, isn't valid YAML, or fails
    /// schema validation.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        debug_assert!(
            !path.starts_with("~"),
            "Config::load expects an expanded path; got tilde-prefixed {}",
            path.display()
        );
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let cfg: Self = serde_yaml::from_str(&s)
            .with_context(|| format!("failed to parse config file: {}", path.display()))?;
        cfg.validate()
            .with_context(|| format!("config validation failed: {}", path.display()))?;
        Ok(cfg)
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn context_mode_source_is_pinned_not_floating() {
        // #496: the built-in context-mode marketplace source must not be an
        // unpinned floating HEAD ref — every regenerate would otherwise pull
        // whatever the upstream repo currently has, breaking reproducibility.
        assert!(
            CONTEXT_MODE_SOURCE
                .split_once('#')
                .is_some_and(|(_, r#ref)| !r#ref.is_empty()),
            "CONTEXT_MODE_SOURCE must carry a non-empty pinned #<tag> suffix: {CONTEXT_MODE_SOURCE}"
        );
    }

    #[test]
    fn load_accepts_expanded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "cache: {}\n").unwrap();
        assert!(Config::load(&p).is_ok());
    }

    #[test]
    fn session_log_absent_resolves_to_transcript_on() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "cache: {}\n").unwrap();
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.session_log.is_none());
        let resolved = cfg.session_log_resolved();
        assert!(!resolved.any_sink_wants(LogLevel::Debug));
        let t = resolved.transcript.as_ref().unwrap();
        assert!(t.enabled);
        assert_eq!(t.level, LogLevel::Info);
    }

    #[test]
    fn session_log_old_shape_translates_to_new() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(
            &p,
            "session_log:\n  file: true\n  transcript: false\n  verbose: true\n",
        )
        .unwrap();
        let r = Config::load(&p).unwrap().session_log_resolved();
        let f = r.file.as_ref().unwrap();
        assert!(f.enabled);
        assert_eq!(f.level, LogLevel::Debug);
        let t = r.transcript.as_ref().unwrap();
        assert!(!t.enabled);
        assert_eq!(t.level, LogLevel::Debug);
    }

    #[test]
    fn session_log_new_shape_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(
            &p,
            "session_log:\n  file:\n    enabled: true\n    level: trace\n  transcript:\n    enabled: true\n    level: info\n",
        )
        .unwrap();
        let r = Config::load(&p).unwrap().session_log_resolved();
        let f = r.file.as_ref().unwrap();
        assert!(f.enabled);
        assert_eq!(f.level, LogLevel::Trace);
        let t = r.transcript.as_ref().unwrap();
        assert!(t.enabled);
        assert_eq!(t.level, LogLevel::Info);
    }

    #[test]
    fn session_log_bare_string_is_rejected_with_migration_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "session_log: /tmp/session.jsonl\n").unwrap();
        let err = Config::load(&p).unwrap_err().to_string();
        // The full chain mentions the field path; the source carries the hint.
        let chain = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(chain.contains("session_log") || err.contains("session_log"));
        assert!(
            chain.contains("file: true"),
            "error shows the migration: {chain}"
        );
    }

    #[test]
    #[should_panic(expected = "expanded path")]
    fn load_rejects_tilde_path_in_debug() {
        let _ = Config::load(Path::new("~/.config/llmenv/config.yaml"));
    }
}
