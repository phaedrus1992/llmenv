mod schema;
mod template;
mod validate;

pub const STATE_DIR_ENV: &str = "LLMENV_STATE_DIR";
pub const RESERVED_STATE_ENV_VARS: &[&str] = &[STATE_DIR_ENV, "CLAUDE_CONFIG_DIR"];
pub const MEMORY_MCP_NAME: &str = "icm";

pub use schema::{
    Bundle, Cache, Capabilities, Config, EnvVar, Features, HashingMode, Hook, HookHandler,
    HookHandlerKind, HostEntry, HostMatch, HostScope, InitConfig, Marketplace, MarketplaceSource,
    McpServer, McpTransport, Memory, NativePermissionRules, NetworkMatch, NetworkScope,
    OFFICIAL_MARKETPLACE_OWNER, PermissionMode, PermissionRule, Permissions, PluginCollection,
    RESERVED_OFFICIAL_MARKETPLACES, Scopes, StateConfig, StateTool, Throttle, UserMatch, UserScope,
    classify_source, github_owner_repo, is_reserved_official_marketplace, split_plugin_ref,
};
pub use template::generate_template;
pub use validate::{ValidateError, validate_capabilities_env_key};

use anyhow::Context;
use std::path::Path;

impl Config {
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
    fn load_accepts_expanded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "cache: {}\n").unwrap();
        assert!(Config::load(&p).is_ok());
    }

    #[test]
    fn session_log_defaults_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "cache: {}\n").unwrap();
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.session_log.is_none());
    }

    #[test]
    fn session_log_parses_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "session_log: /tmp/session.jsonl\n").unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.session_log.as_deref(), Some("/tmp/session.jsonl"));
    }

    #[test]
    #[should_panic(expected = "expanded path")]
    fn load_rejects_tilde_path_in_debug() {
        let _ = Config::load(Path::new("~/.config/llmenv/config.yaml"));
    }
}
