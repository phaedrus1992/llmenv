pub mod matcher;
pub mod network;

pub use matcher::Env;

use crate::config::Config;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Run a scope-detection command and capture its stdout.
///
/// Returns `None` on spawn error, non-zero exit, or non-UTF-8 output. These
/// detectors run on every shell prompt, so a failure is intentionally silent at
/// the return value (→ the scope simply doesn't match); the `tracing::debug!`
/// keeps the cause recoverable under `RUST_LOG=debug` without spamming normal
/// runs. `Command::output()` detaches stdin and captures stderr, so this never
/// blocks on a prompt nor leaks to the terminal (#307).
pub(crate) fn capture_stdout(label: &str, program: &str, args: &[&str]) -> Option<String> {
    let out = match std::process::Command::new(program).args(args).output() {
        Ok(out) => out,
        Err(e) => {
            tracing::debug!("{label}: spawning {program} failed: {e}");
            return None;
        }
    };
    if !out.status.success() {
        tracing::debug!(
            "{label}: {program} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[derive(Debug, Clone)]
pub struct ActiveScope {
    pub id: String,
    pub kind: &'static str,
    pub tags: Vec<String>,
    /// On-disk root the scope matched against. Populated only for
    /// `kind == "project"` (the directory containing `.llmenv.yaml`).
    /// `None` for all other scope kinds.
    pub project_root: Option<PathBuf>,
    /// Bundles this scope manually enables (from `.llmenv.yaml`'s
    /// `enable_bundles` list). Only populated for project scopes.
    pub enable_bundles: Vec<String>,
    /// Bundles this scope removes from the firing set (from `.llmenv.yaml`'s
    /// `disable_bundles` list, #194). Only populated for project scopes.
    /// Disable always wins over any scope's tag-firing or `enable_bundles`,
    /// including this same scope's own `enable_bundles`.
    pub disable_bundles: Vec<String>,
    /// Project display name (from `.llmenv.yaml` `name` field or folder
    /// basename). Only present for `kind == "project"`.
    pub name: Option<String>,
    /// Project description (from `.llmenv.yaml` `description` field).
    /// Only present for `kind == "project"`.
    pub description: Option<String>,
    /// Unknown fields from `.llmenv.yaml` (for warnings in `llmenv doctor`).
    /// Only populated for project scopes.
    pub unknown_fields: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ActiveScopes {
    pub scopes: Vec<ActiveScope>,
    pub tags: BTreeSet<String>,
}

impl ActiveScopes {
    /// Tags from all scopes whose `kind` is not `"project"`.
    ///
    /// Project-scoped tags describe the *project's* domain (e.g.
    /// `lang-typescript`, `agent-coding`) and must not leak into host-level
    /// plugin collection selection — a project's `.llmenv.yaml` should not
    /// cause plugins like `fullstack-dev-skills` to appear in the generated
    /// host `settings.json` (#696).
    ///
    /// Non-project tags (network, host, user, content) describe the
    /// *environment*, which is the correct scope for host plugin decisions.
    pub fn non_project_tags(&self) -> BTreeSet<String> {
        let project_tags: BTreeSet<String> = self
            .scopes
            .iter()
            .filter(|s| s.kind == "project")
            .flat_map(|s| s.tags.iter().cloned())
            .collect();
        self.tags.difference(&project_tags).cloned().collect()
    }
}

pub fn evaluate(cfg: &Config, env: &Env) -> ActiveScopes {
    let mut scopes = Vec::new();
    for s in &cfg.scope.network {
        if matcher::matches_network(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "network",
                tags: s.tags.clone(),
                project_root: None,
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: None,
                description: None,
                unknown_fields: Vec::new(),
            });
        }
    }
    for s in &cfg.scope.host {
        if matcher::matches_host(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "host",
                tags: s.tags.clone(),
                project_root: None,
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: None,
                description: None,
                unknown_fields: Vec::new(),
            });
        }
    }
    for s in &cfg.scope.user {
        if matcher::matches_user(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "user",
                tags: s.tags.clone(),
                project_root: None,
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: None,
                description: None,
                unknown_fields: Vec::new(),
            });
        }
    }
    let cwd = std::path::Path::new(&env.cwd);
    for s in &cfg.scope.content {
        tracing::trace!(id = %s.id, glob = %s.r#match.glob, "evaluating content scope");
        if cwd.exists() && matcher::matches_content(s, cwd) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "content",
                tags: s.tags.clone(),
                project_root: None,
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: None,
                description: None,
                unknown_fields: Vec::new(),
            });
        }
    }
    if let Some(p) = matcher::discover_project(env) {
        scopes.push(ActiveScope {
            id: p.id,
            kind: "project",
            tags: p.tags,
            project_root: Some(p.root),
            enable_bundles: p.enable_bundles,
            disable_bundles: p.disable_bundles,
            name: Some(p.name),
            description: p.description,
            unknown_fields: p.unknown_fields,
        });
    }
    let mut tags: BTreeSet<String> = scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
    if !env.os.is_empty() {
        tags.insert(env.os.clone());
    }
    ActiveScopes { scopes, tags }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_project_tags_excludes_project_scope_tags() {
        let active = ActiveScopes {
            scopes: vec![
                ActiveScope {
                    id: "my-host".into(),
                    kind: "host",
                    tags: vec!["infra".into(), "web".into()],
                    project_root: None,
                    enable_bundles: Vec::new(),
                    disable_bundles: Vec::new(),
                    name: None,
                    description: None,
                    unknown_fields: Vec::new(),
                },
                ActiveScope {
                    id: "my-project".into(),
                    kind: "project",
                    tags: vec!["lang-typescript".into(), "agent-coding".into()],
                    project_root: Some("/project".into()),
                    enable_bundles: Vec::new(),
                    disable_bundles: Vec::new(),
                    name: Some("project".into()),
                    description: None,
                    unknown_fields: Vec::new(),
                },
            ],
            // Full union: infra, web, lang-typescript, agent-coding
            tags: BTreeSet::from([
                "infra".into(),
                "web".into(),
                "lang-typescript".into(),
                "agent-coding".into(),
            ]),
        };

        let host_tags = active.non_project_tags();
        assert!(host_tags.contains("infra"));
        assert!(host_tags.contains("web"));
        assert!(!host_tags.contains("lang-typescript"));
        assert!(!host_tags.contains("agent-coding"));
    }

    #[test]
    fn non_project_tags_no_project_returns_all_tags() {
        let active = ActiveScopes {
            scopes: vec![ActiveScope {
                id: "my-host".into(),
                kind: "host",
                tags: vec!["infra".into(), "web".into()],
                project_root: None,
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: None,
                description: None,
                unknown_fields: Vec::new(),
            }],
            tags: BTreeSet::from(["infra".into(), "web".into()]),
        };

        let host_tags = active.non_project_tags();
        assert!(host_tags.contains("infra"));
        assert!(host_tags.contains("web"));
        assert_eq!(host_tags.len(), 2);
    }

    #[test]
    fn non_project_tags_preserves_os_tag() {
        let active = ActiveScopes {
            scopes: vec![ActiveScope {
                id: "my-project".into(),
                kind: "project",
                tags: vec!["lang-typescript".into()],
                project_root: Some("/project".into()),
                enable_bundles: Vec::new(),
                disable_bundles: Vec::new(),
                name: Some("p".into()),
                description: None,
                unknown_fields: Vec::new(),
            }],
            // OS tag added by evaluate(), not from any scope
            tags: BTreeSet::from(["lang-typescript".into(), "macos".into()]),
        };

        let host_tags = active.non_project_tags();
        assert!(!host_tags.contains("lang-typescript"));
        assert!(host_tags.contains("macos"));
    }

    #[test]
    fn non_project_tags_multiple_projects_excludes_all() {
        let active = ActiveScopes {
            scopes: vec![
                ActiveScope {
                    id: "p1".into(),
                    kind: "project",
                    tags: vec!["tag-a".into()],
                    project_root: Some("/p1".into()),
                    enable_bundles: Vec::new(),
                    disable_bundles: Vec::new(),
                    name: Some("p1".into()),
                    description: None,
                    unknown_fields: Vec::new(),
                },
                ActiveScope {
                    id: "p2".into(),
                    kind: "project",
                    tags: vec!["tag-b".into()],
                    project_root: Some("/p2".into()),
                    enable_bundles: Vec::new(),
                    disable_bundles: Vec::new(),
                    name: Some("p2".into()),
                    description: None,
                    unknown_fields: Vec::new(),
                },
                ActiveScope {
                    id: "h1".into(),
                    kind: "host",
                    tags: vec!["shared-tag".into()],
                    project_root: None,
                    enable_bundles: Vec::new(),
                    disable_bundles: Vec::new(),
                    name: None,
                    description: None,
                    unknown_fields: Vec::new(),
                },
            ],
            tags: BTreeSet::from(["tag-a".into(), "tag-b".into(), "shared-tag".into()]),
        };

        let host_tags = active.non_project_tags();
        assert!(!host_tags.contains("tag-a"));
        assert!(!host_tags.contains("tag-b"));
        assert!(host_tags.contains("shared-tag"));
    }
}
