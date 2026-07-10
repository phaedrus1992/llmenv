use crate::config::{HostScope, NetworkScope, UserScope};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Resolved project (discovered from `.llmenv.yaml` walking upward from cwd).
/// All fields default permissively; malformed YAML is logged as a warning
/// and yields a minimal project with defaults (cwd folder name for id/name).
#[derive(Debug, Clone)]
pub struct ResolvedProject {
    pub root: std::path::PathBuf,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub enable_bundles: Vec<String>,
    /// Bundle names this scope removes from the firing set even if a lower-
    /// precedence scope's tag or `enable_bundles` turned them on (#194).
    /// Disable always wins, including within this same scope.
    pub disable_bundles: Vec<String>,
    /// Keys from the marker file not matching any declared field.
    pub unknown_fields: Vec<String>,
}

/// Schema for the body of `.llmenv.yaml` (project marker file).
/// All fields optional; an empty file is valid.
#[derive(Debug, Default, Deserialize)]
struct ProjectFile {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    enable_bundles: Vec<String>,
    #[serde(default)]
    disable_bundles: Vec<String>,
    /// Capture unknown fields for warning emission.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone)]
pub struct Env {
    pub hostname: String,
    pub user: String,
    pub cwd: String,
    pub gateway_mac: Option<String>,
    /// User's home directory. The `.llmenv.yaml` discovery walk stops at
    /// this boundary so a marker file dropped above $HOME (e.g. `/tmp` on a
    /// shared host) cannot be picked up.
    pub home: Option<std::path::PathBuf>,
}

impl Env {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            hostname: String::new(),
            user: String::new(),
            cwd: String::new(),
            gateway_mac: None,
            home: None,
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
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        Self {
            // Hostname comparison is case-insensitive — `hostname(1)` and
            // /etc/hostname may differ in case across hosts.
            hostname: hostname.to_ascii_lowercase(),
            user,
            cwd,
            gateway_mac: super::network::detect_gateway_mac(),
            home,
        }
    }
}

fn detect_hostname() -> Option<String> {
    super::capture_stdout("hostname detection", "hostname", &[]).map(|s| s.trim().to_string())
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

pub(crate) fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern_lower = pattern.to_ascii_lowercase();
    let text_lower = text.to_ascii_lowercase();

    // ponytail: simple `*` glob, no `?` or `[..]`. Upgrade if needed for complex patterns.
    if !pattern_lower.contains('*') {
        return pattern_lower == text_lower;
    }

    let parts: Vec<&str> = pattern_lower.split('*').collect();

    // First part must match at the start (unless empty, which means pattern started with *)
    if !parts[0].is_empty() && !text_lower.starts_with(parts[0]) {
        return false;
    }

    // Last part must match at the end (unless empty, which means pattern ended with *)
    let last_part = parts[parts.len() - 1];
    if !last_part.is_empty() && !text_lower.ends_with(last_part) {
        return false;
    }

    // Prefix and suffix must not overlap: text must be long enough for both
    if text_lower.len() < parts[0].len() + last_part.len() {
        return false;
    }

    // Middle parts must appear in order between prefix and suffix
    let mut pos = parts[0].len();
    for &part in &parts[1..parts.len() - 1] {
        if let Some(idx) = text_lower[pos..].find(part) {
            pos += idx + part.len();
        } else {
            return false;
        }
    }

    true
}

#[must_use]
pub fn matches_host(s: &HostScope, env: &Env) -> bool {
    s.r#match
        .hostname
        .as_deref()
        .is_some_and(|h| glob_matches(h, &env.hostname))
}

#[must_use]
pub fn matches_user(s: &UserScope, env: &Env) -> bool {
    s.r#match.user.as_deref().is_some_and(|u| u == env.user)
}

/// Discover project by walking cwd upward looking for `.llmenv.yaml`.
/// When found, parse and return a `ResolvedProject` with all fields resolved
/// (defaults applied, unknown fields collected). If YAML is malformed, log a
/// warning and return a minimal `ResolvedProject` with id/name from the
/// folder basename.
///
/// The walk is bounded at `$HOME`: a marker at `~/.llmenv.yaml` activates,
/// but the walk does not ascend above home. This prevents a hostile marker
/// dropped in e.g. `/tmp` (on a shared host) or `/Volumes/...` from being
/// picked up. When `$HOME` is unknown, only the cwd itself is checked.
#[must_use]
pub fn discover_project(env: &Env) -> Option<ResolvedProject> {
    let mut cur = std::path::PathBuf::from(&env.cwd);
    loop {
        let marker_path = cur.join(".llmenv.yaml");
        if marker_path.exists() {
            let pf = read_project_file(&marker_path);
            let basename = cur
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("llmenv")
                .to_string();
            let id = pf.id.unwrap_or_else(|| basename.clone());
            let name = pf.name.unwrap_or_else(|| basename.clone());
            let unknown_fields: Vec<String> = pf
                .extra
                .keys()
                .filter(|k| {
                    !matches!(
                        k.as_str(),
                        "id" | "name"
                            | "description"
                            | "tags"
                            | "enable_bundles"
                            | "disable_bundles"
                    )
                })
                .cloned()
                .collect();
            return Some(ResolvedProject {
                root: cur,
                id,
                name,
                description: pf.description,
                tags: pf.tags,
                enable_bundles: pf.enable_bundles,
                disable_bundles: pf.disable_bundles,
                unknown_fields,
            });
        }
        // Stop the walk once we've checked $HOME (or if home is unknown,
        // after checking only cwd). This blocks markers above home from
        // activating.
        match &env.home {
            Some(h) if cur == *h => break,
            None => break,
            _ => {}
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Maximum length (in bytes) for the project description. Anything longer
/// is truncated and a warning is logged. The description is surfaced into
/// LLM context chunks; a hard cap prevents a malformed or hostile marker
/// from bloating every prompt.
const MAX_DESCRIPTION_BYTES: usize = 1024;

/// Parse `.llmenv.yaml` file into a `ProjectFile`. Empty file → all defaults.
/// Malformed YAML → log warning and return defaults. The `description`
/// field is truncated to `MAX_DESCRIPTION_BYTES` if oversized.
fn read_project_file(path: &std::path::Path) -> ProjectFile {
    let Ok(body) = std::fs::read_to_string(path) else {
        return ProjectFile::default();
    };
    if body.trim().is_empty() {
        return ProjectFile::default();
    }
    match serde_yaml::from_str::<ProjectFile>(&body) {
        Ok(mut pf) => {
            if let Some(desc) = pf.description.as_mut()
                && desc.len() > MAX_DESCRIPTION_BYTES
            {
                tracing::warn!(
                    "project marker file {} has description >{} bytes; truncating",
                    path.display(),
                    MAX_DESCRIPTION_BYTES
                );
                // Truncate at a char boundary so the result remains valid UTF-8.
                let mut cut = MAX_DESCRIPTION_BYTES;
                while cut > 0 && !desc.is_char_boundary(cut) {
                    cut -= 1;
                }
                desc.truncate(cut);
            }
            pf
        }
        Err(e) => {
            tracing::warn!(
                "project marker file {} is not valid YAML: {e}; using defaults",
                path.display()
            );
            ProjectFile::default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{Env, discover_project, glob_matches};
    use proptest::prelude::*;
    use std::path::Path;

    fn write_project_file(temp_dir: &Path, body: &str) {
        let path = temp_dir.join(".llmenv.yaml");
        std::fs::write(&path, body).expect("write .llmenv.yaml");
    }

    /// Build an `Env` with cwd inside `temp_dir`, treating `temp_dir`'s
    /// parent as $HOME so the walk reaches markers at `temp_dir` (and
    /// upward as long as we're under the boundary).
    fn env_in(cwd: &Path, home: &Path) -> Env {
        Env {
            hostname: String::new(),
            user: String::new(),
            cwd: cwd.to_string_lossy().to_string(),
            gateway_mac: None,
            home: Some(home.to_path_buf()),
        }
    }

    #[test]
    fn discovers_project_with_all_fields() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let yaml =
            "id: myapp\nname: MyApp\ndescription: Test app\ntags: [a, b]\nenable_bundles: [base]\n";
        write_project_file(temp_dir.path(), yaml);

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.id, "myapp");
        assert_eq!(project.name, "MyApp");
        assert_eq!(project.description, Some("Test app".to_string()));
        assert_eq!(project.tags, vec!["a", "b"]);
        assert_eq!(project.enable_bundles, vec!["base"]);
        assert!(project.unknown_fields.is_empty());
    }

    #[test]
    fn discovers_project_with_disable_bundles() {
        // #194
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let yaml = "id: myapp\nenable_bundles: [github-issues]\ndisable_bundles: [yaks]\n";
        write_project_file(temp_dir.path(), yaml);

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.enable_bundles, vec!["github-issues"]);
        assert_eq!(project.disable_bundles, vec!["yaks"]);
        assert!(project.unknown_fields.is_empty());
    }

    #[test]
    fn empty_file_uses_defaults() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(temp_dir.path(), "");

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        let basename = temp_dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(project.id, basename.as_ref());
        assert_eq!(project.name, basename.as_ref());
        assert_eq!(project.description, None);
        assert!(project.tags.is_empty());
        assert!(project.enable_bundles.is_empty());
        assert!(project.disable_bundles.is_empty());
    }

    #[test]
    fn walks_upward_to_find_marker() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let root = temp_dir.path();
        let subdir = root.join("a").join("b");
        std::fs::create_dir_all(&subdir).expect("mkdir");
        write_project_file(root, "id: found\n");

        let env = env_in(&subdir, root);

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.id, "found");
        assert_eq!(project.root, root);
    }

    #[test]
    fn walk_stops_at_home_boundary() {
        // Marker is above $HOME (in an ancestor of home) — must not be
        // picked up even when cwd is below home.
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let above_home = temp_dir.path();
        let home = above_home.join("home");
        let workdir = home.join("project");
        std::fs::create_dir_all(&workdir).expect("mkdir");
        // Hostile marker above home.
        write_project_file(above_home, "id: hostile\n");

        let env = env_in(&workdir, &home);
        assert!(
            discover_project(&env).is_none(),
            "marker above $HOME must not activate"
        );
    }

    #[test]
    fn walk_finds_marker_at_home() {
        // Marker exactly at $HOME — must activate (boundary is inclusive).
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let home = temp_dir.path();
        let workdir = home.join("project");
        std::fs::create_dir_all(&workdir).expect("mkdir");
        write_project_file(home, "id: home-project\n");

        let env = env_in(&workdir, home);
        let project = discover_project(&env).expect("discover");
        assert_eq!(project.id, "home-project");
        assert_eq!(project.root, home);
    }

    #[test]
    fn no_walk_above_cwd_when_home_unknown() {
        // With no HOME, only cwd itself is checked — no upward walk.
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let root = temp_dir.path();
        let subdir = root.join("sub");
        std::fs::create_dir_all(&subdir).expect("mkdir");
        write_project_file(root, "id: parent\n");

        let env = Env {
            hostname: String::new(),
            user: String::new(),
            cwd: subdir.to_string_lossy().to_string(),
            gateway_mac: None,
            home: None,
        };
        assert!(
            discover_project(&env).is_none(),
            "without HOME, walk must not ascend"
        );
    }

    #[test]
    fn returns_none_when_no_marker_found() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env);
        assert!(project.is_none());
    }

    #[test]
    fn malformed_yaml_uses_defaults() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(temp_dir.path(), "not: [valid: yaml");

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        let basename = temp_dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(project.id, basename.as_ref());
        assert_eq!(project.name, basename.as_ref());
    }

    #[test]
    fn long_description_is_truncated() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let huge = "a".repeat(super::MAX_DESCRIPTION_BYTES + 500);
        write_project_file(temp_dir.path(), &format!("description: \"{huge}\"\n"));

        let env = env_in(temp_dir.path(), temp_dir.path());
        let project = discover_project(&env).expect("discover");
        let desc = project.description.expect("description");
        assert!(
            desc.len() <= super::MAX_DESCRIPTION_BYTES,
            "description must be capped"
        );
    }

    #[test]
    fn captures_unknown_fields() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(
            temp_dir.path(),
            "id: test\nunknown_field: value\nanother: 42\n",
        );

        let env = env_in(temp_dir.path(), temp_dir.path());

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.unknown_fields.len(), 2);
        assert!(
            project
                .unknown_fields
                .contains(&"unknown_field".to_string())
        );
        assert!(project.unknown_fields.contains(&"another".to_string()));
    }

    #[test]
    fn glob_matches_exact() {
        assert!(glob_matches("localhost", "localhost"));
        assert!(glob_matches("example.com", "example.com"));
        assert!(!glob_matches("example.com", "other.com"));
    }

    #[test]
    fn glob_matches_case_insensitive() {
        assert!(glob_matches("LOCALHOST", "localhost"));
        assert!(glob_matches("Example.COM", "example.com"));
        assert!(glob_matches("localhost", "LOCALHOST"));
    }

    #[test]
    fn glob_matches_leading_wildcard() {
        assert!(glob_matches("*.example.com", "dev.example.com"));
        assert!(glob_matches("*.example.com", "prod.example.com"));
        assert!(glob_matches("*.example.com", "api.staging.example.com"));
        assert!(!glob_matches("*.example.com", "example.com"));
        assert!(!glob_matches("*.example.com", "example.org"));
    }

    #[test]
    fn glob_matches_trailing_wildcard() {
        assert!(glob_matches("host-*", "host-001"));
        assert!(glob_matches("host-*", "host-prod"));
        assert!(glob_matches("host-*", "host-"));
        assert!(!glob_matches("host-*", "other-001"));
    }

    #[test]
    fn glob_matches_multiple_wildcards() {
        assert!(glob_matches("*-prod-*", "web-prod-01"));
        assert!(glob_matches("*-prod-*", "api-prod-staging"));
        assert!(glob_matches("*-prod-*", "-prod-"));
        assert!(!glob_matches("*-prod-*", "web-dev-01"));
    }

    #[test]
    fn glob_matches_only_wildcard() {
        assert!(glob_matches("*", "localhost"));
        assert!(glob_matches("*", "any.host.example.com"));
        assert!(glob_matches("*", ""));
    }

    #[test]
    fn glob_matches_preserves_ordering() {
        assert!(glob_matches("*-prod-*-01", "web-prod-east-01"));
        assert!(!glob_matches("*-prod-*-01", "web-01-prod-east"));
    }

    #[test]
    fn glob_matches_overlapping_prefix_suffix() {
        // Critical: prefix and suffix must not overlap
        assert!(!glob_matches("abc*abc", "abc"));
        assert!(!glob_matches("abc*cd", "abcd"));
        assert!(!glob_matches("abcde*cde", "abcde"));
        assert!(!glob_matches("host*host", "host"));
        // Valid matches where prefix+suffix fits
        assert!(glob_matches("abc*abc", "abcXabc"));
        assert!(glob_matches("abc*cd", "abcXcd"));
    }

    #[test]
    fn glob_matches_exact_length_match() {
        // Pattern prefix+suffix exactly matches text length (no middle content)
        assert!(glob_matches("a*b", "ab"));
        assert!(glob_matches("host*prod", "hostprod"));
        assert!(!glob_matches("host*prod", "host"));
        assert!(glob_matches("abc*def", "abcdef")); // prefix+suffix fit exactly
        // Pattern with middle parts matching exactly
        assert!(glob_matches("a*b*c", "abc")); // a + nothing + b + nothing + c
        assert!(!glob_matches("a*x*c", "abc")); // a + nothing + x (missing) + nothing + c
    }

    proptest! {
        // discover_project never panics on arbitrary cwd paths.
        #[test]
        fn discover_arbitrary_path_never_panics(cwd in r"/[a-z/]*") {
            let env = Env {
                hostname: String::new(),
                user: String::new(),
                cwd,
                gateway_mac: None,
                home: None,
            };
            let _ = discover_project(&env);
        }

        // Malformed YAML never panics; always degrades to defaults.
        #[test]
        fn malformed_yaml_never_panics(body in r"\PC*") {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");
            write_project_file(temp_dir.path(), &body);
            let env = env_in(temp_dir.path(), temp_dir.path());
            let _ = discover_project(&env);
        }

        // Property test #165: Unicode-safe basename derivation.
        // Derived project id/name must be valid UTF-8 and handle special chars.
        #[test]
        fn unicode_safe_basename_derivation(
            name_part in r"[^\x00/\.]|[^\x00/][^\x00/]*[^\x00/.]"
        ) {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");
            let root = temp_dir.path();
            let sub = root.join(&name_part);
            // Reject test cases where directory creation fails.
            prop_assume!(std::fs::create_dir_all(&sub).is_ok());

            write_project_file(&sub, "");
            let env = env_in(&sub, root);
            let project = discover_project(&env).expect("discover");

            // id and name must be valid UTF-8 (already guaranteed by String).
            // Both must be non-empty (basename fallback is "llmenv").
            prop_assert!(!project.id.is_empty());
            prop_assert!(!project.name.is_empty());
            // name_part is guaranteed non-empty, no leading/trailing dots
            prop_assert_eq!(project.id, name_part.clone());
            prop_assert_eq!(project.name, name_part);
        }

        // Property test #166: discover_project walk termination with deep nesting.
        // Walk must not descend infinitely; should terminate at home boundary or root.
        #[test]
        fn walk_terminates_at_home_boundary(
            depth in 1..32usize,
        ) {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");
            let root = temp_dir.path();
            let mut deep_path = root.to_path_buf();
            for i in 0..depth {
                deep_path.push(format!("d{i}"));
            }
            prop_assume!(std::fs::create_dir_all(&deep_path).is_ok());

            // Place marker at root; walk from deep_path should find it.
            write_project_file(root, "id: root-marker\n");

            let env = env_in(&deep_path, root);
            let project = discover_project(&env).expect("discover at depth");
            prop_assert_eq!(project.id, "root-marker");
            prop_assert_eq!(project.root, root);

            // Now test walk stops at home: place hostile marker above home.
            let temp_dir2 = tempfile::TempDir::new().expect("tempdir2");
            let above_home = temp_dir2.path();
            let home = above_home.join("home");
            let mut deep_work = home.to_path_buf();
            for i in 0..depth {
                deep_work.push(format!("w{i}"));
            }
            prop_assume!(std::fs::create_dir_all(&deep_work).is_ok());
            write_project_file(above_home, "id: hostile\n");

            let env2 = env_in(&deep_work, &home);
            let result = discover_project(&env2);
            // Hostile marker above home must not be found, even at depth.
            prop_assert!(result.is_none(), "hostile marker above home must not activate");
        }

        // Property test #167: ProjectFile unknown-fields filtering correctness.
        // Unknown fields must be captured; known fields must not appear in unknown_fields.
        #[test]
        fn project_file_unknown_fields_filtering(
            unknown_count in 0..10usize,
            known_id in "[a-z0-9]+",
        ) {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");

            // Build YAML with known fields + unknown fields.
            let mut yaml = format!("id: {}\n", known_id);
            yaml.push_str("name: TestName\n");
            yaml.push_str("tags: [a, b, c]\n");

            // Append arbitrary unknown fields.
            for i in 0..unknown_count {
                yaml.push_str(&format!("field_{}: value_{}\n", i, i));
            }

            write_project_file(temp_dir.path(), &yaml);
            let env = env_in(temp_dir.path(), temp_dir.path());
            let project = discover_project(&env).expect("discover");

            // Verify known fields were parsed.
            prop_assert_eq!(project.id, known_id);
            prop_assert_eq!(project.name, "TestName");
            prop_assert_eq!(project.tags, vec!["a", "b", "c"]);

            // Verify unknown fields were captured.
            prop_assert_eq!(
                project.unknown_fields.len(),
                unknown_count,
                "all unknown fields must be captured"
            );

            // Verify no known field names appear in unknown_fields.
            for uf in &project.unknown_fields {
                prop_assert!(!matches!(
                    uf.as_str(),
                    "id" | "name" | "description" | "tags" | "enable_bundles" | "disable_bundles"
                ));
            }
        }
    }
}
