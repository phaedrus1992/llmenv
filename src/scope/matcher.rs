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

/// Discover project by walking cwd upward looking for `.llmenv.yaml`.
/// When found, parse and return a `ResolvedProject` with all fields resolved
/// (defaults applied, unknown fields collected). If YAML is malformed, log a
/// warning and return a minimal `ResolvedProject` with id/name from the
/// folder basename.
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
                        "id" | "name" | "description" | "tags" | "enable_bundles"
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
                unknown_fields,
            });
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Parse `.llmenv.yaml` file into a `ProjectFile`. Empty file → all defaults.
/// Malformed YAML → log warning and return defaults.
fn read_project_file(path: &std::path::Path) -> ProjectFile {
    let Ok(body) = std::fs::read_to_string(path) else {
        return ProjectFile::default();
    };
    if body.trim().is_empty() {
        return ProjectFile::default();
    }
    match serde_yaml::from_str::<ProjectFile>(&body) {
        Ok(pf) => pf,
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
    use super::{Env, discover_project};
    use proptest::prelude::*;
    use std::path::Path;

    fn write_project_file(temp_dir: &Path, body: &str) {
        let path = temp_dir.join(".llmenv.yaml");
        std::fs::write(&path, body).expect("write .llmenv.yaml");
    }

    #[test]
    fn discovers_project_with_all_fields() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let yaml =
            "id: myapp\nname: MyApp\ndescription: Test app\ntags: [a, b]\nenable_bundles: [base]\n";
        write_project_file(temp_dir.path(), yaml);

        let env = Env {
            hostname: String::new(),
            user: String::new(),
            cwd: temp_dir.path().to_string_lossy().to_string(),
            gateway_mac: None,
        };

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.id, "myapp");
        assert_eq!(project.name, "MyApp");
        assert_eq!(project.description, Some("Test app".to_string()));
        assert_eq!(project.tags, vec!["a", "b"]);
        assert_eq!(project.enable_bundles, vec!["base"]);
        assert!(project.unknown_fields.is_empty());
    }

    #[test]
    fn empty_file_uses_defaults() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(temp_dir.path(), "");

        let env = Env {
            hostname: String::new(),
            user: String::new(),
            cwd: temp_dir.path().to_string_lossy().to_string(),
            gateway_mac: None,
        };

        let project = discover_project(&env).expect("discover");
        let basename = temp_dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(project.id, basename.as_ref());
        assert_eq!(project.name, basename.as_ref());
        assert_eq!(project.description, None);
        assert!(project.tags.is_empty());
        assert!(project.enable_bundles.is_empty());
    }

    #[test]
    fn walks_upward_to_find_marker() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let root = temp_dir.path();
        let subdir = root.join("a").join("b");
        std::fs::create_dir_all(&subdir).expect("mkdir");
        write_project_file(root, "id: found\n");

        let env = Env {
            hostname: String::new(),
            user: String::new(),
            cwd: subdir.to_string_lossy().to_string(),
            gateway_mac: None,
        };

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.id, "found");
        assert_eq!(project.root, root);
    }

    #[test]
    fn returns_none_when_no_marker_found() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let env = Env {
            hostname: String::new(),
            user: String::new(),
            cwd: temp_dir.path().to_string_lossy().to_string(),
            gateway_mac: None,
        };

        let project = discover_project(&env);
        assert!(project.is_none());
    }

    #[test]
    fn malformed_yaml_uses_defaults() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(temp_dir.path(), "not: [valid: yaml");

        let env = Env {
            hostname: String::new(),
            user: String::new(),
            cwd: temp_dir.path().to_string_lossy().to_string(),
            gateway_mac: None,
        };

        let project = discover_project(&env).expect("discover");
        let basename = temp_dir.path().file_name().unwrap().to_string_lossy();
        assert_eq!(project.id, basename.as_ref());
        assert_eq!(project.name, basename.as_ref());
    }

    #[test]
    fn captures_unknown_fields() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        write_project_file(
            temp_dir.path(),
            "id: test\nunknown_field: value\nanother: 42\n",
        );

        let env = Env {
            hostname: String::new(),
            user: String::new(),
            cwd: temp_dir.path().to_string_lossy().to_string(),
            gateway_mac: None,
        };

        let project = discover_project(&env).expect("discover");
        assert_eq!(project.unknown_fields.len(), 2);
        assert!(
            project
                .unknown_fields
                .contains(&"unknown_field".to_string())
        );
        assert!(project.unknown_fields.contains(&"another".to_string()));
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
            };
            let _ = discover_project(&env);
        }

        // Malformed YAML never panics; always degrades to defaults.
        #[test]
        fn malformed_yaml_never_panics(body in r"\PC*") {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");
            write_project_file(temp_dir.path(), &body);
            let env = Env {
                hostname: String::new(),
                user: String::new(),
                cwd: temp_dir.path().to_string_lossy().to_string(),
                gateway_mac: None,
            };
            let _ = discover_project(&env);
        }
    }
}
