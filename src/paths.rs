//! XDG paths and path helpers.

use std::path::{Path, PathBuf};

/// Expand a leading `~` or `~/` to `$HOME`. Other input is returned unchanged.
/// Returns the input unchanged when `HOME` is unset.
#[must_use]
pub fn expand_tilde(p: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return p.to_string();
    };
    if let Some(rest) = p.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if p == "~" {
        home
    } else {
        p.to_string()
    }
}

/// True if `path` contains any parent (`..`) component, parsed
/// component-wise rather than by substring. Catches traversal that string
/// matching misses: `foo/..`, mixed separators on the host OS, and a bare
/// `..` with no trailing slash. A leading `/` (root) is fine; only `..`
/// components are rejected.
#[must_use]
pub fn has_parent_component(path: &str) -> bool {
    use std::path::Component;
    Path::new(path)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
}

/// Return true if `cwd` is at or below `prefix`, treating both as filesystem
/// paths (component-wise) rather than raw strings. This avoids the
/// `/home/alice/git/xyz` matches prefix `/home/alice/git/x` bug.
#[must_use]
pub fn cwd_under_prefix(cwd: &str, prefix: &str) -> bool {
    let cwd_p = Path::new(cwd);
    let pre_p = PathBuf::from(prefix);
    cwd_p.starts_with(&pre_p)
}

pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("LLMENV_CONFIG_DIR") {
        Ok(PathBuf::from(dir))
    } else {
        let home = std::env::var("HOME")?;
        Ok(PathBuf::from(home).join(".config/llmenv"))
    }
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("config.yaml"))
}

pub fn state_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("LLMENV_STATE_DIR") {
        Ok(PathBuf::from(dir))
    } else {
        let home = std::env::var("HOME")?;
        Ok(PathBuf::from(home).join(".local/state/llmenv"))
    }
}

/// Write `content` to `path` with owner-only permissions (mode 0o600) on Unix.
/// On Windows falls back to default permissions. Creates the file if absent,
/// truncates if present. Use for any file containing user state or
/// credentials (settings, sync state, MCP configs, ICM memory) where
/// world-readable defaults would leak data on shared systems.
pub fn write_owner_only(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(content)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_under_prefix_respects_component_boundary() {
        assert!(cwd_under_prefix("/home/alice/git/x", "/home/alice/git/x"));
        assert!(cwd_under_prefix(
            "/home/alice/git/x/sub",
            "/home/alice/git/x"
        ));
        assert!(!cwd_under_prefix(
            "/home/alice/git/xyz",
            "/home/alice/git/x"
        ));
        assert!(!cwd_under_prefix("/home/alice", "/home/alice/git"));
    }

    #[test]
    fn has_parent_component_detects_traversal_substring_misses() {
        // Trailing `..` with no slash — substring check for "../" misses this.
        assert!(has_parent_component("foo/.."));
        assert!(has_parent_component(".."));
        assert!(has_parent_component("/foo/../bar"));
        assert!(has_parent_component("a/b/../c"));
    }

    #[test]
    fn has_parent_component_allows_safe_paths() {
        assert!(!has_parent_component("/home/alice/.cache/llmenv"));
        assert!(!has_parent_component("relative/path"));
        assert!(!has_parent_component("~/.cache/llmenv"));
        // A `..` embedded in a name is not a parent component.
        assert!(!has_parent_component("/foo/..bar/baz"));
        assert!(!has_parent_component("file..txt"));
        assert!(!has_parent_component(""));
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_sets_mode_0o600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("secret");
        write_owner_only(&path, b"sensitive").expect("write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        // Group/other bits must be clear — file is owner-only.
        assert_eq!(mode & 0o077, 0, "group/other bits set: {mode:o}");
        let body = std::fs::read(&path).expect("read");
        assert_eq!(body, b"sensitive");
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_truncates_existing_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("file");
        write_owner_only(&path, b"longer content").expect("write1");
        write_owner_only(&path, b"short").expect("write2");
        let body = std::fs::read(&path).expect("read");
        assert_eq!(body, b"short");
    }

    #[test]
    fn tilde_passthrough_for_absolute_and_relative() {
        // Tests the non-HOME-dependent branches.
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("rel/path"), "rel/path");
        assert_eq!(expand_tilde(""), "");
    }
}
