//! Durable per-tool state directory (#175).
//!
//! llmenv materializes each agent config into a content-hashed cache folder
//! (`<adapter_root>/<TAG>-<hash>/`) and points `CLAUDE_CONFIG_DIR` at it. Every
//! hash change (version bump, config edit, different directory) yields a *new*
//! folder, so any tool that persists runtime state under the config dir loses it.
//!
//! This module provides a stable sibling directory whose name carries no content
//! hash — `<adapter_root>/state/` — and the env vars that relocate tool state
//! into it: `LLMENV_STATE_DIR` (always) plus one var per configured
//! [`StateTool`], each pointed at a per-tool subdirectory.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::config::StateConfig;

/// Folder name of the durable state directory, a sibling of the hashed config
/// folders under an adapter's cache root. Has no content hash, so it is stable
/// across every materialization.
pub const STATE_DIR_NAME: &str = "state";

pub use llmenv_config::{RESERVED_STATE_ENV_VARS, STATE_DIR_ENV};
/// The durable state directory for an adapter, given its cache root
/// (`<cache_dir>/<adapter>`). Sibling to the hashed config folders.
#[must_use]
pub fn state_dir(adapter_root: &Path) -> PathBuf {
    adapter_root.join(STATE_DIR_NAME)
}

/// The env vars that relocate tool state into the durable directory.
///
/// Always includes `LLMENV_STATE_DIR=<state_dir>`. Each configured tool adds
/// `<env>=<state_dir>/<subdir>`. Pure: computes paths only, performs no I/O.
/// Directory creation is [`ensure_state_dirs`].
#[must_use]
pub fn state_env_vars(cfg: &StateConfig, state_dir: &Path) -> Vec<(String, String)> {
    let mut vars = Vec::with_capacity(cfg.tools.len() + 1);
    vars.push((STATE_DIR_ENV.to_string(), state_dir.display().to_string()));
    for tool in &cfg.tools {
        let path = state_dir.join(&tool.subdir);
        vars.push((tool.env.clone(), path.display().to_string()));
    }
    vars
}

/// Create the durable state directory and every configured tool's subdirectory.
///
/// Idempotent (`create_dir_all`). Tools expect their relocated dir to exist
/// before they start, so materialization creates them up front.
///
/// # Errors
/// Returns an error if any directory cannot be created.
pub fn ensure_state_dirs(cfg: &StateConfig, state_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    for tool in &cfg.tools {
        std::fs::create_dir_all(state_dir.join(&tool.subdir))?;
    }
    Ok(())
}

/// Compute the effective state config, injecting context-mode's durable-dir
/// relocation (#490) when the built-in feature is enabled.
///
/// Returns `cfg` borrowed unchanged when the feature is off or the user already
/// declared a `CONTEXT_MODE_DATA_DIR` tool (user config wins); otherwise returns
/// a clone with the synthetic tool appended.
#[must_use]
pub fn effective_state_config(
    cfg: &StateConfig,
    context_mode_enabled: bool,
) -> Cow<'_, StateConfig> {
    use crate::config::{CONTEXT_MODE_DATA_ENV, CONTEXT_MODE_STATE_SUBDIR, StateTool};
    if !context_mode_enabled || cfg.tools.iter().any(|t| t.env == CONTEXT_MODE_DATA_ENV) {
        return Cow::Borrowed(cfg);
    }
    let mut owned = cfg.clone();
    owned.tools.push(StateTool {
        env: CONTEXT_MODE_DATA_ENV.to_string(),
        subdir: CONTEXT_MODE_STATE_SUBDIR.to_string(),
    });
    Cow::Owned(owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::StateTool;

    fn cfg(tools: &[(&str, &str)]) -> StateConfig {
        StateConfig {
            tools: tools
                .iter()
                .map(|(env, subdir)| StateTool {
                    env: (*env).into(),
                    subdir: (*subdir).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn state_dir_is_unhashed_sibling() {
        let root = Path::new("/cache/llmenv/claude-code");
        assert_eq!(
            state_dir(root),
            Path::new("/cache/llmenv/claude-code/state")
        );
    }

    #[test]
    fn always_emits_llmenv_state_dir() {
        let dir = Path::new("/cache/llmenv/claude-code/state");
        let vars = state_env_vars(&StateConfig::default(), dir);
        assert_eq!(
            vars,
            vec![(
                "LLMENV_STATE_DIR".to_string(),
                "/cache/llmenv/claude-code/state".to_string()
            )]
        );
    }

    #[test]
    fn emits_per_tool_var_pointed_at_subdir() {
        let dir = Path::new("/cache/llmenv/claude-code/state");
        let vars = state_env_vars(&cfg(&[("CONTEXT_MODE_DATA_DIR", "context-mode")]), dir);
        assert!(vars.contains(&(
            "CONTEXT_MODE_DATA_DIR".to_string(),
            "/cache/llmenv/claude-code/state/context-mode".to_string()
        )));
        // LLMENV_STATE_DIR still present alongside the per-tool var.
        assert!(vars.iter().any(|(k, _)| k == "LLMENV_STATE_DIR"));
    }

    #[test]
    fn ensure_creates_base_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        ensure_state_dirs(&cfg(&[("A_DIR", "a"), ("B_DIR", "b")]), &dir).unwrap();
        assert!(dir.is_dir());
        assert!(dir.join("a").is_dir());
        assert!(dir.join("b").is_dir());
    }

    #[test]
    fn ensure_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let c = cfg(&[("A_DIR", "a")]);
        ensure_state_dirs(&c, &dir).unwrap();
        // Second call over existing dirs must not error.
        ensure_state_dirs(&c, &dir).unwrap();
        assert!(dir.join("a").is_dir());
    }

    #[test]
    fn context_mode_injects_state_tool() {
        let base = StateConfig::default();
        let eff = effective_state_config(&base, true);
        assert!(
            eff.tools
                .iter()
                .any(|t| t.env == "CONTEXT_MODE_DATA_DIR" && t.subdir == "context-mode"),
            "expected CONTEXT_MODE_DATA_DIR tool to be injected"
        );
    }

    #[test]
    fn context_mode_disabled_no_injection() {
        let base = StateConfig::default();
        let eff = effective_state_config(&base, false);
        assert!(eff.tools.is_empty(), "no tools when feature disabled");
    }

    #[test]
    fn context_mode_dedups_user_state_tool() {
        let base = StateConfig {
            tools: vec![StateTool {
                env: "CONTEXT_MODE_DATA_DIR".into(),
                subdir: "my-custom-dir".into(),
            }],
        };
        let eff = effective_state_config(&base, true);
        let cm: Vec<_> = eff
            .tools
            .iter()
            .filter(|t| t.env == "CONTEXT_MODE_DATA_DIR")
            .collect();
        assert_eq!(
            cm.len(),
            1,
            "no duplicate entries for CONTEXT_MODE_DATA_DIR"
        );
        assert_eq!(
            cm[0].subdir, "my-custom-dir",
            "user entry preserved, not overwritten"
        );
    }
}
