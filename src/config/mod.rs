mod schema;
mod validate;

pub use schema::*;
pub use validate::ValidateError;

/// Env var llmenv always emits pointing at the durable state directory.
pub const STATE_DIR_ENV: &str = "LLMENV_STATE_DIR";

/// Env vars reserved by llmenv that a [`StateTool`]'s `env` field must not claim.
/// Validation rejects any `StateTool` that tries to redirect one of these.
pub const RESERVED_STATE_ENV_VARS: &[&str] = &[STATE_DIR_ENV, "CLAUDE_CONFIG_DIR"];

/// Registration name of the memory (ICM) MCP server in the resolved MCP list.
pub const MEMORY_MCP_NAME: &str = "icm";

use anyhow::Context;
use std::path::Path;

impl Config {
    /// Load and validate a config from an **already-expanded** path.
    ///
    /// `load` does not perform tilde (`~`) expansion — the caller is
    /// responsible for expanding `~`/`~user` (e.g. via [`crate::paths`]) before
    /// calling. Passing a `~`-prefixed path will fail at `read_to_string`
    /// because the literal `~` is not resolved by the OS. A `debug_assert`
    /// guards this contract in debug builds.
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn load_accepts_expanded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        std::fs::write(&p, "cache: {}\n").unwrap();
        // An absolute (already-expanded) path must not trip the debug_assert.
        assert!(Config::load(&p).is_ok());
    }

    #[test]
    #[should_panic(expected = "expanded path")]
    fn load_rejects_tilde_path_in_debug() {
        // The contract: callers expand `~` first. In debug builds this trips
        // the guard rather than failing obscurely at read_to_string.
        let _ = Config::load(Path::new("~/.config/llmenv/config.yaml"));
    }
}
