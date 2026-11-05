//! Reads of a bundle's `AGENTS.md` and `bundle.yaml` (#2933).

use std::path::Path;

use crate::config::Capabilities;

/// The largest bundle text file that a merge reads. The text goes into every session, so a file
/// this large is a mistake, not content.
const MAX_BUNDLE_FILE_BYTES: u64 = 1024 * 1024;

/// Read the bundle text file at `path`, or `None` when there is none, through
/// [`llmenv_paths::read_text_bounded`] with a cap of [`MAX_BUNDLE_FILE_BYTES`]. A link is
/// followed, because a linked `AGENTS.md` can be deliberate.
///
/// # Errors
/// The file cannot be opened or read, is not a regular file, is too large, or is not UTF-8.
pub(crate) fn read_bundle_text(path: &Path) -> std::io::Result<Option<String>> {
    llmenv_paths::read_text_bounded(path, MAX_BUNDLE_FILE_BYTES)
}

/// Keys that a `bundle.yaml` fragment is allowed to declare. Any other top-level
/// key is rejected with a hard error rather than silently dropped.
pub(super) const BUNDLE_YAML_KNOWN_KEYS: &[&str] = &[
    "permissions",
    "hooks",
    "plugins",
    "mcp",
    "lsp",
    "skills",
    "output_styles",
    "env",
    "auto_memory_enabled",
    "effort_level",
    "model_effort",
    "advisor_model",
    // Parsed so validation can name the replacement (#2409); never merged.
    "advisor_size",
    "native_permissions",
    "native_hooks",
    "native_plugins",
    "native_mcp",
    "native_model_providers",
    "native_default_models",
    "native",
    "features",
    "host",
    // Found during #1031: these two were missing despite Capabilities' own doc
    // comments documenting bundle-level support ("contributed by this
    // capability source (bundle or top-level)") — any bundle.yaml declaring
    // either was hard-rejected as an unknown key, with no test ever
    // exercising the documented claim.
    "model_providers",
    "default_models",
];

/// Read an optional `bundle.yaml` capability fragment from a bundle directory.
/// Returns `None` when the file is absent — bundles carry capabilities only if
/// they choose to.
///
/// `pub(crate)`: also called by `hook_run`'s memory diagnostics, which need one
/// suppressed bundle's declarations without merging it into the manifest.
///
/// # Errors
/// An unreadable file, invalid YAML, or an unknown top-level key.
pub(crate) fn read_bundle_yaml(
    bundle_root: &Path,
    name: &str,
) -> anyhow::Result<Option<Capabilities>> {
    let path = bundle_root.join("bundle.yaml");
    let s = match read_bundle_text(&path) {
        Ok(Some(s)) => s,
        Ok(None) => return Ok(None),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "bundle '{name}': reading {}: {e}",
                path.display()
            ));
        }
    };
    let raw: serde_yaml::Value = serde_yaml::from_str(&s)
        .map_err(|e| anyhow::anyhow!("bundle '{name}': parsing {}: {e}", path.display()))?;
    if let Some(mapping) = raw.as_mapping() {
        for key in mapping.keys() {
            if let Some(k) = key.as_str()
                && !BUNDLE_YAML_KNOWN_KEYS.contains(&k)
            {
                anyhow::bail!(
                    "bundle '{name}': unknown key '{k}' in bundle.yaml — \
                     known keys: {}",
                    BUNDLE_YAML_KNOWN_KEYS.join(", ")
                );
            }
        }
    }
    let mut caps: Capabilities = serde_yaml::from_value(raw)
        .map_err(|e| anyhow::anyhow!("bundle '{name}': parsing {}: {e}", path.display()))?;

    // Track which bundle each hook came from, so the adapter can resolve relative paths later.
    // We don't resolve paths here because duplicate hooks (e.g., "hooks/guard.sh" from two bundles)
    // must dedup correctly before being adapted into settings.json.
    for hook in &mut caps.hooks {
        hook.bundle_origin = Some(bundle_root.to_path_buf());
    }

    let context = format!("bundle '{name}'");
    for key in caps.env.keys() {
        crate::config::validate_capabilities_env_key(&context, key)?;
    }
    llmenv_config::validate_effort(&context, &caps)?;
    // A bundle hook never reaches `Config::validate`, which reads config.yaml only (#3101).
    llmenv_config::validate_hooks(&caps.hooks).map_err(|e| anyhow::anyhow!("{context}: {e}"))?;
    llmenv_config::validate_advisor(&context, &caps)?;
    let permission_rules = caps
        .permissions
        .allow
        .iter()
        .chain(caps.permissions.ask.iter())
        .chain(caps.permissions.deny.iter());
    for rule in permission_rules {
        crate::config::validate_permission_rule(&context, rule)?;
    }
    for (engine, nr) in &caps.native_permissions {
        let ctx = format!("bundle '{name}': native_permissions['{engine}']");
        for s in nr.allow.iter().chain(nr.ask.iter()).chain(nr.deny.iter()) {
            crate::config::validate_permission_string(&ctx, s)?;
        }
    }

    // Validate bundle-contributed memory entries with the same checks that
    // Config::validate() applies to top-level features.memory entries.
    if let Some(features) = &caps.features {
        for mem in &features.memory {
            if mem.when.is_empty() {
                anyhow::bail!(
                    "{context}: features.memory entry for '{}' has no 'when' tags — every memory entry must declare at least one activation tag",
                    mem.server_host
                );
            }
            if mem.listen_host.parse::<std::net::IpAddr>().is_err() {
                anyhow::bail!(
                    "{context}: features.memory entry for '{}': listen_host '{}' is not a valid \
                     IP address literal (hostnames not supported)",
                    mem.server_host,
                    mem.listen_host
                );
            }
            if let Some(n) = mem.wakeup_max_tokens
                && !crate::config::WAKEUP_MAX_TOKENS_RANGE.contains(&n)
            {
                anyhow::bail!(
                    "{context}: features.memory entry for '{}': wakeup_max_tokens ({n}) must be \
                     between {} and {} (icm's own MCP handler clamps to this range; llmenv \
                     fails fast instead of silently truncating)",
                    mem.server_host,
                    crate::config::WAKEUP_MAX_TOKENS_RANGE.start(),
                    crate::config::WAKEUP_MAX_TOKENS_RANGE.end()
                );
            }
        }
        for th in &features.throttle {
            if th.when.is_empty() {
                anyhow::bail!(
                    "{context}: features.throttle entry for '{}' has no 'when' tags — \
                     every throttle entry must declare at least one activation tag",
                    th.backend
                );
            }
            if th.backend.is_empty() {
                anyhow::bail!("{context}: features.throttle entry has an empty 'backend' field");
            }
        }
        for cm in &features.codebase_memory {
            if cm.when.is_empty() {
                anyhow::bail!(
                    "{context}: features.codebase_memory entry has no 'when' tags — \
                     every codebase_memory entry must declare at least one activation tag"
                );
            }
        }
    }

    Ok(Some(caps))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_none_and_a_text_file_is_read() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_bundle_text(&dir.path().join("AGENTS.md")).unwrap(),
            None
        );
        std::fs::write(dir.path().join("AGENTS.md"), "rules").unwrap();
        assert_eq!(
            read_bundle_text(&dir.path().join("AGENTS.md"))
                .unwrap()
                .as_deref(),
            Some("rules")
        );
    }

    #[test]
    fn a_fifo_fails_at_once_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("AGENTS.md");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let err = read_bundle_text(&fifo).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    // #3101: a bundle hook is checked as a config hook is, with the bundle named.
    #[test]
    fn a_bundle_hook_with_on_failure_on_an_mcp_tool_names_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "hooks:\n  - event: PreToolUse\n    handler: { type: mcp_tool, tool: srv:check, on_failure: block }\n";
        std::fs::write(dir.path().join("bundle.yaml"), yaml).unwrap();
        let err = read_bundle_yaml(dir.path(), "guards")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("bundle 'guards':"), "{err}");
        assert!(err.contains("only a command hook can block"), "{err}");
        let ok = "hooks:\n  - event: PreToolUse\n    handler: { type: command, command: g.sh, on_failure: block }\n";
        std::fs::write(dir.path().join("bundle.yaml"), ok).unwrap();
        assert!(read_bundle_yaml(dir.path(), "guards").unwrap().is_some());
    }

    #[test]
    fn the_bundle_cap_is_the_documented_1_mib() {
        assert_eq!(MAX_BUNDLE_FILE_BYTES, 1_048_576);
    }

    #[test]
    fn an_oversized_file_fails_with_its_size_rule() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("bundle.yaml");
        let size = usize::try_from(MAX_BUNDLE_FILE_BYTES).unwrap() + 1;
        std::fs::write(&big, vec![b'a'; size]).unwrap();
        let err = read_bundle_text(&big).unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
        std::fs::write(&big, vec![b'a'; size - 1]).unwrap();
        assert!(
            read_bundle_text(&big).unwrap().is_some(),
            "the limit itself is allowed"
        );
    }
}
