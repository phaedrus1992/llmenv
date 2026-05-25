//! XDG paths and path helpers. Cache/state-dir resolution wired in later tasks (M5).

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

/// Return true if `cwd` is at or below `prefix`, treating both as filesystem
/// paths (component-wise) rather than raw strings. This avoids the
/// `/home/breed/git/xyz` matches prefix `/home/breed/git/x` bug.
#[must_use]
pub fn cwd_under_prefix(cwd: &str, prefix: &str) -> bool {
    let cwd_p = Path::new(cwd);
    let pre_p = PathBuf::from(prefix);
    cwd_p.starts_with(&pre_p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_under_prefix_respects_component_boundary() {
        assert!(cwd_under_prefix("/home/breed/git/x", "/home/breed/git/x"));
        assert!(cwd_under_prefix(
            "/home/breed/git/x/sub",
            "/home/breed/git/x"
        ));
        assert!(!cwd_under_prefix(
            "/home/breed/git/xyz",
            "/home/breed/git/x"
        ));
        assert!(!cwd_under_prefix("/home/breed", "/home/breed/git"));
    }

    #[test]
    fn tilde_passthrough_for_absolute_and_relative() {
        // Tests the non-HOME-dependent branches.
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("rel/path"), "rel/path");
        assert_eq!(expand_tilde(""), "");
    }
}
