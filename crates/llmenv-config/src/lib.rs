mod proxy_path;
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

pub use proxy_path::{PathParseError, PathSegment, get_path, parse_path, remove_path, set_path};
pub use schema::{
    Bundle, Cache, Capabilities, CdGuard, CodebaseMemory, Config, ConsolidationBackend,
    ConsolidationConfig, ContentMatch, ContentScope, ContextMode, EnvVar, Features, FileSinkConfig,
    HashingMode, Hook, HookHandler, HookHandlerKind, HostEntry, HostMatch, HostScope, IconSet,
    ImportanceLevel, InitConfig, LaunchProxy, LogLevel, LspServer, Marketplace, MarketplaceSource,
    McpPermissionAction, McpPermissions, McpServer, McpTransport, Memory, MemoryType, ModelCost,
    ModelProvider, ModelRef, ModelSource, NativePermissionRules, NetworkMatch, NetworkScope,
    OFFICIAL_MARKETPLACE_OWNER, OutputStyle, PermissionMode, PermissionPreset, PermissionRule,
    Permissions, PluginCollection, ProxyCheck, ProxyCondition, ProxyConditionTarget, ProxyOp,
    ProxyRule, ProxyTarget, RESERVED_OFFICIAL_MARKETPLACES, ReadOnce, ReadOnceMode, RepeatDetect,
    Scopes, SessionLog, SkillSource, SlippageControl, StateConfig, StateTool, StatuslineConfig,
    StatuslineStyle, TaskTracker, Throttle, TranscriptSinkConfig, UpgradeConfig, UpgradeTrack,
    UserMatch, UserScope, WAKEUP_MAX_TOKENS_RANGE, WidgetConfig, classify_source,
    github_owner_repo, is_reserved_official_marketplace, split_plugin_ref,
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
    /// on, file off.
    #[must_use]
    pub fn session_log_resolved(&self) -> SessionLog {
        self.session_log.clone().unwrap_or_default()
    }

    /// Load and validate a config, expanding a leading `~`/`~/` internally
    /// (via `llmenv_paths::expand_tilde`) so callers don't need to expand the
    /// path themselves first.
    ///
    /// # Errors
    /// Returns an error if the file can't be read, isn't valid YAML, or fails
    /// schema validation.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let expanded;
        let path: &Path = if path.starts_with("~") {
            expanded = llmenv_paths::expand_tilde(&path.to_string_lossy());
            Path::new(&expanded)
        } else {
            path
        };
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

    // #744: the pre-3.3 boolean shape used to be silently translated. Removed in
    // 4.0 — but a bare "invalid type: boolean" from serde tells a user upgrading
    // nothing about what to write instead, so the shape is still *detected*, only
    // to produce an error that names its replacement.
    #[test]
    fn session_log_old_boolean_shape_is_rejected_with_migration_guidance() {
        for body in [
            "session_log:\n  file: true\n",
            "session_log:\n  transcript: false\n",
            "session_log:\n  verbose: true\n",
            "session_log:\n  file: true\n  transcript: false\n  verbose: true\n",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join("config.yaml");
            std::fs::write(&p, body).unwrap();

            let err = format!("{:#}", Config::load(&p).unwrap_err());
            assert!(
                err.contains("no longer supported"),
                "error should say the shape is gone: {err}"
            );
            // The whole point of keeping a detection pass: the message has to
            // show the replacement, not just refuse the input.
            assert!(
                err.contains("enabled:"),
                "error should show the per-sink form: {err}"
            );
        }
    }

    // `verbose: true` meant Debug for both sinks; the replacement is a per-sink
    // `level`. Naming it keeps the migration mechanical for anyone hitting this.
    #[test]
    fn old_shape_error_maps_verbose_onto_the_level_field() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "session_log:\n  verbose: true\n").unwrap();
        let err = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(err.contains("level:"), "verbose maps onto level: {err}");
    }

    // A mapping-valued `file`/`transcript` is the *new* shape and must not be
    // caught by the old-shape guard — the detection keys on the boolean value,
    // not on the field name.
    #[test]
    fn new_shape_keys_are_not_mistaken_for_the_old_boolean_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(
            &p,
            "session_log:\n  file:\n    enabled: true\n    level: debug\n",
        )
        .unwrap();
        // The per-sink mapping form still parses.
        let r = Config::load(&p).unwrap().session_log_resolved();
        let f = r.file.as_ref().unwrap();
        assert!(f.enabled);
        assert_eq!(f.level, LogLevel::Debug);
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
    fn load_expands_tilde_prefixed_path_internally() {
        let home = std::env::var("HOME").unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("llmenv-config-tilde-test-")
            .tempdir_in(&home)
            .unwrap();
        let dir_name = tmp.path().file_name().unwrap().to_str().unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "cache: {}\n").unwrap();

        let tilde_path = std::path::PathBuf::from(format!("~/{dir_name}/config.yaml"));
        assert!(Config::load(&tilde_path).is_ok());
    }

    // Invalid-UTF-8 filenames are legal on Linux (raw byte paths) but
    // rejected outright by macOS/APFS ("Illegal byte sequence"), so this can
    // only run on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn load_passes_non_tilde_path_through_byte_exact() {
        // A non-tilde path must never go through a `to_string_lossy` round
        // trip: that would mangle invalid-UTF-8 bytes into U+FFFD and read
        // the wrong file.
        use std::os::unix::ffi::OsStrExt;

        let tmp = tempfile::tempdir().unwrap();
        let name = std::ffi::OsStr::from_bytes(b"co\xffnfig.yaml");
        let p = tmp.path().join(name);
        std::fs::write(&p, "cache: {}\n").unwrap();

        assert!(Config::load(&p).is_ok());
    }

    #[test]
    fn mcp_permissions_rejects_invalid_action_value() {
        // #946: `features.<name>.mcp_permissions` values must be one of
        // allow|ask|deny — anything else is a clear config error.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(
            &p,
            "cache: {}\n\
             features:\n\
             \x20\x20context_mode:\n\
             \x20\x20\x20\x20enabled: true\n\
             \x20\x20\x20\x20mcp_permissions:\n\
             \x20\x20\x20\x20\x20\x20read_only: maybe\n",
        )
        .unwrap();
        let err = Config::load(&p).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to parse config file"),
            "expected a config-parse error, got: {msg}"
        );
    }

    #[test]
    fn mcp_permissions_accepts_valid_action_values() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(
            &p,
            "cache: {}\n\
             features:\n\
             \x20\x20context_mode:\n\
             \x20\x20\x20\x20enabled: true\n\
             \x20\x20\x20\x20mcp_permissions:\n\
             \x20\x20\x20\x20\x20\x20read_only: allow\n\
             \x20\x20\x20\x20\x20\x20mutation: allow\n\
             \x20\x20\x20\x20\x20\x20destructive: deny\n",
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        let perms = cfg
            .features
            .unwrap()
            .context_mode
            .unwrap()
            .mcp_permissions
            .unwrap();
        assert_eq!(perms.read_only, Some(McpPermissionAction::Allow));
        assert_eq!(perms.mutation, Some(McpPermissionAction::Allow));
        assert_eq!(perms.destructive, Some(McpPermissionAction::Deny));
    }
}
