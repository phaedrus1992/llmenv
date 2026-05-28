use crate::config::{HostScope, NetworkScope, ProjectScope, UserScope};
use crate::paths::{cwd_under_prefix, expand_tilde};
use serde::Deserialize;

/// Resolved project scope match: where the marker/prefix landed, plus any
/// tags or bundles declared in the marker file's YAML body. Empty when the
/// marker is missing/empty/malformed (parse failures are reported via
/// `tracing::warn` so the scope still activates).
#[derive(Debug, Clone)]
pub struct MatchedProject {
    pub root: std::path::PathBuf,
    pub extra_tags: Vec<String>,
    /// Bundle names this marker manually enables. Names must already be
    /// defined in `config.yaml` — the marker only opts existing bundles in,
    /// it doesn't define new ones.
    pub enable_bundles: Vec<String>,
}

/// Schema for the body of a project marker file (e.g. `.llmenv-dev`).
/// All fields optional; an empty file is valid.
///
/// `enable_bundles` lists bundles (defined in config.yaml) to manually
/// activate when this marker is matched — useful when you don't want to
/// invent a tag just to bind a bundle to one project.
#[derive(Debug, Default, Deserialize)]
struct MarkerFile {
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    enable_bundles: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Env {
    pub hostname: String,
    pub user: String,
    pub cwd: String,
    pub gateway_mac: Option<String>,
}

impl Env {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            hostname: String::new(),
            user: String::new(),
            cwd: String::new(),
            gateway_mac: None,
        }
    }

    #[must_use]
    pub fn detect() -> Self {
        let hostname = detect_hostname().unwrap_or_else(|| {
            tracing::warn!("hostname detection failed; host-scope matching disabled");
            String::new()
        });
        let user = std::env::var("USER").unwrap_or_else(|_| {
            tracing::warn!("$USER unset; user-scope matching disabled");
            String::new()
        });
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| {
                tracing::warn!("current_dir() unavailable; project-scope matching disabled");
                String::new()
            });
        Self {
            // Hostname comparison is case-insensitive — `hostname(1)` and
            // /etc/hostname may differ in case across hosts.
            hostname: hostname.to_ascii_lowercase(),
            user,
            cwd,
            gateway_mac: super::network::detect_gateway_mac(),
        }
    }
}

fn detect_hostname() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}

#[must_use]
pub fn matches_network(s: &NetworkScope, env: &Env) -> bool {
    let Some(want) = s.r#match.gateway_mac.as_deref() else {
        // ssid/cidr are not yet supported for matching; without gateway_mac we cannot match.
        return false;
    };
    env.gateway_mac
        .as_deref()
        .is_some_and(|got| got.eq_ignore_ascii_case(want))
}

#[must_use]
pub fn matches_host(s: &HostScope, env: &Env) -> bool {
    s.r#match
        .hostname
        .as_deref()
        .is_some_and(|h| h.eq_ignore_ascii_case(&env.hostname))
}

#[must_use]
pub fn matches_user(s: &UserScope, env: &Env) -> bool {
    s.r#match.user.as_deref().is_some_and(|u| u == env.user)
}

/// Resolves a project scope against the environment. For `path_prefix` the
/// root is the expanded prefix; for `marker` it's the deepest ancestor of
/// cwd containing the marker file (and the marker file's YAML body
/// contributes extra tags). A scope matches iff this returns `Some`.
#[must_use]
pub fn match_project(s: &ProjectScope, env: &Env) -> Option<MatchedProject> {
    if let Some(p) = s.r#match.path_prefix.as_deref() {
        let expanded = expand_tilde(p);
        if cwd_under_prefix(&env.cwd, &expanded) {
            return Some(empty_match(std::path::PathBuf::from(expanded)));
        }
    }
    if let Some(marker) = s.r#match.marker.as_deref() {
        let mut cur = std::path::PathBuf::from(&env.cwd);
        loop {
            let marker_path = cur.join(marker);
            if marker_path.exists() {
                let (extra_tags, enable_bundles) = read_marker(&marker_path);
                return Some(MatchedProject {
                    root: cur,
                    extra_tags,
                    enable_bundles,
                });
            }
            if !cur.pop() {
                break;
            }
        }
    }
    if let Some(glob_pattern) = s.r#match.glob.as_deref() {
        let cwd_path = std::path::PathBuf::from(&env.cwd);
        if glob_matches(&cwd_path, glob_pattern) {
            return Some(empty_match(cwd_path));
        }
    }
    None
}

/// Check if any file matching the glob pattern exists in the directory.
fn glob_matches(dir: &std::path::Path, pattern: &str) -> bool {
    let pattern_path = if pattern.starts_with('/') || pattern.starts_with("./") {
        std::path::PathBuf::from(pattern)
    } else {
        dir.join(pattern)
    };

    match glob::glob(pattern_path.to_string_lossy().as_ref()) {
        Ok(mut paths) => paths.next().is_some(),
        Err(_) => false,
    }
}

/// Returns either an empty `MatchedProject` for `path_prefix` matches or a
/// helper to build one with empty tags.
fn empty_match(root: std::path::PathBuf) -> MatchedProject {
    MatchedProject {
        root,
        extra_tags: Vec::new(),
        enable_bundles: Vec::new(),
    }
}

/// Parse the marker file as YAML and return `(tags, enable_bundles)`. Empty
/// file → both empty (no warning). Malformed YAML → warn and return both
/// empty so the scope still activates.
fn read_marker(path: &std::path::Path) -> (Vec<String>, Vec<String>) {
    let Ok(body) = std::fs::read_to_string(path) else {
        return (Vec::new(), Vec::new());
    };
    if body.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }
    match serde_yaml::from_str::<MarkerFile>(&body) {
        Ok(m) => (m.tags, m.enable_bundles),
        Err(e) => {
            tracing::warn!(
                "marker file {} is not valid YAML, ignoring tags/enable_bundles: {e}",
                path.display()
            );
            (Vec::new(), Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::read_marker;
    use proptest::prelude::*;
    use std::io::Write;

    fn write_marker(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("create temp marker");
        f.write_all(body.as_bytes()).expect("write temp marker");
        f
    }

    #[test]
    fn reads_tags_and_bundles_from_valid_yaml() {
        let f = write_marker("tags: [a, b]\nenable_bundles: [base]\n");
        let (tags, bundles) = read_marker(f.path());
        assert_eq!(tags, vec!["a", "b"]);
        assert_eq!(bundles, vec!["base"]);
    }

    #[test]
    fn empty_file_yields_empty() {
        let f = write_marker("");
        assert_eq!(read_marker(f.path()), (Vec::new(), Vec::new()));
    }

    proptest! {
        // Whitespace-only bodies are treated as empty, never error.
        #[test]
        fn whitespace_only_yields_empty(ws in r"[ \t\r\n]*") {
            let f = write_marker(&ws);
            prop_assert_eq!(read_marker(f.path()), (Vec::new(), Vec::new()));
        }

        // Arbitrary bytes never panic; malformed YAML degrades to empty.
        // A fuzz input could coincidentally be valid YAML, so only assert the
        // no-panic contract here (the empty-on-malformed path is covered above).
        #[test]
        fn arbitrary_input_never_panics(body in r"\PC*") {
            let f = write_marker(&body);
            let _ = read_marker(f.path());
        }
    }

    #[test]
    fn glob_pattern_matches_existing_file() {
        use super::glob_matches;
        let temp_dir = tempfile::TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create test files
        std::fs::write(temp_path.join("Cargo.toml"), "").unwrap();
        std::fs::write(temp_path.join(".gitignore"), "").unwrap();

        // Pattern matching file that exists
        assert!(glob_matches(temp_path, "Cargo.toml"));
        assert!(glob_matches(temp_path, "*.toml"));

        // Pattern not matching
        assert!(!glob_matches(temp_path, "nonexistent.txt"));
        assert!(!glob_matches(temp_path, "*.rs"));
    }
}
