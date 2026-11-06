mod advisor;
mod effort;
mod proxy_path;
mod schema;
mod template;
mod unknown_keys;
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

pub use advisor::validate_advisor;
pub use effort::validate_effort;
pub use proxy_path::{PathParseError, PathSegment, get_path, parse_path, remove_path, set_path};
pub use schema::{
    Bundle, Cache, Capabilities, CdGuard, CodebaseMemory, Config, ConsolidationBackend,
    ConsolidationConfig, ContentMatch, ContentScope, ContextMode, EnvVar, Features, FileSinkConfig,
    HashingMode, Hook, HookHandler, HookHandlerKind, HookOnFailure, HostEntry, HostMatch,
    HostScope, IconSet, ImportanceLevel, InitConfig, LaunchProxy, LogLevel, LspServer, Marketplace,
    MarketplaceSource, McpPermissionAction, McpPermissions, McpServer, McpTransport, Memory,
    MemoryType, ModelCost, ModelEffort, ModelProvider, ModelRef, ModelSource,
    NativePermissionRules, NetworkMatch, NetworkScope, OFFICIAL_MARKETPLACE_OWNER, OutputStyle,
    PermissionMode, PermissionPreset, PermissionRule, Permissions, PluginCollection, ProxyCheck,
    ProxyCondition, ProxyConditionTarget, ProxyOp, ProxyRule, ProxyTarget,
    RESERVED_OFFICIAL_MARKETPLACES, ReadOnce, ReadOnceMode, RepeatDetect, Sandbox, SandboxRuntime,
    Scopes, SessionLog, SkillSource, SlippageControl, StateConfig, StateTool, StatuslineConfig,
    StatuslineStyle, TaskTracker, Throttle, TranscriptSinkConfig, UpgradeConfig, UpgradeTrack,
    UserMatch, UserScope, WAKEUP_MAX_TOKENS_RANGE, WidgetConfig, classify_source, dedup_hooks,
    github_owner_repo, is_reserved_official_marketplace, split_plugin_ref,
};
pub use template::generate_template;
pub use unknown_keys::{unknown_key_message, unknown_top_level_keys};
pub use validate::{
    ValidateError, validate_capabilities_env_key, validate_hooks, validate_permission_rule,
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

    /// Load and validate a config, expanding a leading `~`/`~user` first.
    ///
    /// # Errors
    /// Returns an error if the file can't be read, isn't valid YAML, or fails
    /// schema validation.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        // Only `~` itself goes through string handling ($HOME is always
        // valid Unicode when set) — the remainder of the path joins back on
        // as raw OS bytes, so a non-UTF-8 byte after `~/` is never mangled
        // by a lossy round trip. A non-tilde path is untouched either way.
        let path: std::borrow::Cow<'_, Path> = match path.strip_prefix("~") {
            Ok(rest) => {
                let home = llmenv_paths::expand_tilde("~")?;
                std::borrow::Cow::Owned(std::path::PathBuf::from(home).join(rest))
            }
            Err(_) => std::borrow::Cow::Borrowed(path),
        };
        let path = path.as_ref();
        let s = read_config_text(path)?;
        let mut cfg: Self = serde_yaml::from_str(&s)
            .with_context(|| format!("failed to parse config file: {}", path.display()))?;
        if let Some(features) = cfg.features.as_mut() {
            for cm in &mut features.codebase_memory {
                cm.expand_index_path()
                    .with_context(|| format!("config file: {}", path.display()))?;
            }
        }
        cfg.validate()
            .with_context(|| format!("config validation failed: {}", path.display()))?;
        Ok(cfg)
    }
}

/// The largest config file that llmenv reads. A hand-written config is a few KiB, so this cap
/// leaves room for a generated config and still stops a runaway file before it fills memory.
const MAX_CONFIG_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Read the config file at `path` through a bounded reader, so a FIFO or a huge file fails at
/// once (#2942). A missing file is a `NotFound` error, as from `std::fs::read_to_string`.
fn read_config_text(path: &Path) -> anyhow::Result<String> {
    llmenv_paths::read_text_bounded(path, MAX_CONFIG_FILE_BYTES)
        .and_then(|text| {
            text.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "there is no file there; run `llmenv init` to create one",
                )
            })
        })
        .with_context(|| format!("failed to read config file: {}", path.display()))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests;
