mod advisor;
mod effort;
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
pub use schema::{
    Bundle, Cache, Capabilities, CdGuard, CodebaseMemory, Config, ConsolidationBackend,
    ConsolidationConfig, ContentMatch, ContentScope, ContextMode, EnvVar, Features, FileSinkConfig,
    HashingMode, Hook, HookHandler, HookHandlerKind, HookOnFailure, HostEntry, HostMatch,
    HostScope, IconSet, ImportanceLevel, InitConfig, LogLevel, LspServer, Marketplace,
    MarketplaceSource, McpPermissionAction, McpPermissions, McpServer, McpTransport, Memory,
    MemoryType, ModelCost, ModelEffort, ModelProvider, ModelRef, ModelSource,
    NativePermissionRules, NetworkMatch, NetworkScope, OFFICIAL_MARKETPLACE_OWNER, OutputStyle,
    PermissionMode, PermissionPreset, PermissionRule, Permissions, PluginCollection,
    RESERVED_OFFICIAL_MARKETPLACES, ReadOnce, ReadOnceMode, RepeatDetect, Scopes, SessionLog,
    SkillSource, SlippageControl, StateConfig, StateTool, StatuslineConfig, StatuslineStyle,
    TaskTracker, Throttle, TranscriptSinkConfig, UpgradeConfig, UpgradeTrack, UserMatch, UserScope,
    WAKEUP_MAX_TOKENS_RANGE, WidgetConfig, classify_source, dedup_hooks, github_owner_repo,
    is_reserved_official_marketplace, split_plugin_ref,
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
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn load_fails_at_once_on_a_fifo_and_names_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        let made = std::process::Command::new("mkfifo")
            .arg(&p)
            .status()
            .unwrap();
        assert!(made.success());
        let err = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(err.contains(&p.display().to_string()), "{err}");
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn the_config_cap_is_the_documented_4_mib() {
        assert_eq!(MAX_CONFIG_FILE_BYTES, 4_194_304);
    }

    #[test]
    fn load_rejects_a_file_over_the_cap_and_names_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.yaml");
        let size = usize::try_from(MAX_CONFIG_FILE_BYTES).unwrap() + 1;
        std::fs::write(&p, vec![b'#'; size]).unwrap();
        let err = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(err.contains(&p.display().to_string()), "{err}");
        assert!(err.contains("more than"), "{err}");
    }

    #[test]
    fn load_of_a_missing_file_is_a_not_found_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Config::load(&tmp.path().join("config.yaml")).unwrap_err();
        let io = err.downcast_ref::<std::io::Error>().unwrap();
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    }

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
    fn load_expands_tilde_path() {
        // #1910: `load` used to only debug_assert the caller pre-expanded `~`,
        // so a release build silently mis-resolved a tilde path instead of
        // erroring. It must now expand `~` itself.
        let home = std::env::var("HOME").unwrap();
        let dir = tempfile::Builder::new()
            .prefix(".llmenv-config-test-")
            .tempdir_in(&home)
            .unwrap();
        let p = dir.path().join("config.yaml");
        std::fs::write(&p, "cache: {}\n").unwrap();
        let dir_name = dir.path().file_name().unwrap().to_str().unwrap();
        let tilde_path = format!("~/{dir_name}/config.yaml");
        assert!(Config::load(Path::new(&tilde_path)).is_ok());
    }

    #[test]
    fn load_expands_tilde_in_root_codebase_memory_index_path() {
        // #2651: a root-level entry is not merged, so `load` must expand it itself.
        let home = std::env::var("HOME").unwrap();
        let dir = tempfile::Builder::new()
            .prefix(".llmenv-config-test-")
            .tempdir_in(&home)
            .unwrap();
        let p = dir.path().join("config.yaml");
        let yaml =
            "features:\n  codebase_memory:\n    - when: [proj]\n      index_path: ~/cache/cbm\n";
        std::fs::write(&p, yaml).unwrap();
        let cfg = Config::load(&p).unwrap();
        let cm = &cfg.features.unwrap().codebase_memory[0];
        let index_path = cm.index_path.as_deref().unwrap();
        assert!(
            !index_path.starts_with('~'),
            "tilde must be expanded: {index_path}"
        );
        assert!(
            index_path.ends_with("/cache/cbm"),
            "suffix kept: {index_path}"
        );
    }

    // macOS (APFS/HFS+) rejects invalid-UTF-8 filenames at the syscall level, so this
    // property is only observable on Linux, where filenames are arbitrary bytes.
    #[cfg(target_os = "linux")]
    #[test]
    fn load_does_not_mangle_a_non_tilde_non_utf8_path() {
        // #1910 follow-up: expansion must only touch tilde-prefixed paths.
        // Round-tripping every path through `to_string_lossy` would corrupt
        // a non-UTF-8 path that never needed expansion in the first place.
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let tmp = tempfile::tempdir().unwrap();
        let mut bytes = tmp.path().join("").into_os_string().into_vec();
        bytes.extend_from_slice(b"caf\xe9.yaml"); // invalid UTF-8 byte 0xe9
        let p = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&bytes));
        std::fs::write(&p, "cache: {}\n").unwrap();
        assert!(Config::load(&p).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn load_expands_tilde_without_mangling_non_utf8_bytes_after_it() {
        // A tilde path also needs the non-UTF-8 guarantee: only `~` itself
        // (always valid UTF-8 -- $HOME) goes through string handling, so a
        // non-UTF-8 byte in the rest of the path survives untouched.
        use std::os::unix::ffi::OsStrExt;

        let home = std::env::var("HOME").unwrap();
        let dir = tempfile::Builder::new()
            .prefix(".llmenv-config-test-")
            .tempdir_in(&home)
            .unwrap();

        let filename_bytes = b"caf\xe9.yaml"; // invalid UTF-8 byte 0xe9
        let filename = std::ffi::OsStr::from_bytes(filename_bytes);
        std::fs::write(dir.path().join(filename), "cache: {}\n").unwrap();

        let dir_name = dir.path().file_name().unwrap().as_bytes();
        let mut tilde_bytes = b"~/".to_vec();
        tilde_bytes.extend_from_slice(dir_name);
        tilde_bytes.push(b'/');
        tilde_bytes.extend_from_slice(filename_bytes);
        let tilde_path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&tilde_bytes));
        assert!(Config::load(&tilde_path).is_ok());
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

    use proptest::prelude::*;
    proptest! {
        /// #1910: an arbitrary non-tilde-prefixed filename must load
        /// unchanged — the tilde branch in `load` must never fire on it, and
        /// no round trip may alter which file gets read.
        #[test]
        fn prop_load_non_tilde_filename_reads_the_exact_file(name in "[a-zA-Z0-9_-]{1,20}") {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join(format!("{name}.yaml"));
            std::fs::write(&p, "cache: {}\n").unwrap();
            prop_assert!(Config::load(&p).is_ok());
        }

        /// #1910: an arbitrary tilde-prefixed path resolves under `$HOME`,
        /// for any legal file/dir name component after the `~/`.
        #[test]
        fn prop_load_tilde_path_resolves_under_home(name in "[a-zA-Z0-9_-]{1,20}") {
            let home = std::env::var("HOME").unwrap();
            let dir = tempfile::Builder::new()
                .prefix(".llmenv-config-prop-test-")
                .tempdir_in(&home)
                .unwrap();
            let p = dir.path().join(format!("{name}.yaml"));
            std::fs::write(&p, "cache: {}\n").unwrap();
            let dir_name = dir.path().file_name().unwrap().to_str().unwrap();
            let tilde_path = format!("~/{dir_name}/{name}.yaml");
            prop_assert!(Config::load(Path::new(&tilde_path)).is_ok());
        }

        /// A tilde that doesn't lead the path (e.g. mid-string) is a literal
        /// character, not an expansion trigger — `strip_prefix("~")` on the
        /// whole path only matches a *leading* `~` component.
        #[test]
        fn prop_load_mid_path_tilde_is_not_expanded(name in "[a-zA-Z0-9_-]{1,20}") {
            let tmp = tempfile::tempdir().unwrap();
            let p = tmp.path().join(format!("{name}~extra.yaml"));
            std::fs::write(&p, "cache: {}\n").unwrap();
            prop_assert!(Config::load(&p).is_ok());
        }
    }
}
