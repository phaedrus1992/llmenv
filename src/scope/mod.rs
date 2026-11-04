mod content_walk;
mod identity;
pub(crate) mod marker_file;
mod marker_link;
pub mod matcher;
pub mod network;
mod probe;
pub mod ssid;
pub(crate) mod tags;

pub use matcher::Env;

use crate::config::Config;
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct ActiveScope {
    pub id: String,
    pub kind: &'static str,
    pub tags: Vec<String>,
    /// On-disk root the scope matched against. Populated only for
    /// `kind == "project"` (the directory containing `.llmenv.yaml`).
    /// `None` for all other scope kinds.
    pub(crate) project_root: Option<PathBuf>,
    /// Bundles this scope manually enables (from `.llmenv.yaml`'s
    /// `enable_bundles` list). Only populated for project scopes.
    pub enable_bundles: Vec<String>,
    /// Bundles this scope removes from the firing set (from `.llmenv.yaml`'s
    /// `disable_bundles` list, #194). Only populated for project scopes.
    /// Disable always wins over any scope's tag-firing or `enable_bundles`,
    /// including this same scope's own `enable_bundles`.
    pub(crate) disable_bundles: Vec<String>,
    /// Project display name (from `.llmenv.yaml` `name` field or folder
    /// basename). Only present for `kind == "project"`.
    pub name: Option<String>,
    /// Project description (from `.llmenv.yaml` `description` field).
    /// Only present for `kind == "project"`.
    pub(crate) description: Option<String>,
    /// Unknown fields from `.llmenv.yaml` (for warnings in `llmenv doctor`).
    /// Only populated for project scopes.
    pub(crate) unknown_fields: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ActiveScopes {
    pub scopes: Vec<ActiveScope>,
    pub tags: BTreeSet<String>,
    /// Tags from `$LLMENV_EXTRA_TAGS`. Not attached to any scope, so tracked
    /// separately from `tags` — `non_project_tags()` excludes them from
    /// host-level plugin/MCP resolution, matching the trust level of the
    /// `.llmenv.yaml` project tags they're the escape hatch for (#696, #1020).
    pub extra_tags: BTreeSet<String>,
}

impl ActiveScopes {
    /// Tags from all scopes whose `kind` is not `"project"`.
    ///
    /// Project-scoped tags describe the *project's* domain (e.g.
    /// `lang-typescript`, `agent-coding`) and must not leak into host-level
    /// plugin collection selection — a project's `.llmenv.yaml` should not
    /// cause plugins like `fullstack-dev-skills` to appear in the generated
    /// host `settings.json` (#696). `$LLMENV_EXTRA_TAGS` tags get the same
    /// treatment: they're the documented substitute for a committed
    /// `.llmenv.yaml`, so they must not reach a more privileged surface than
    /// the project tags they stand in for.
    ///
    /// Non-project tags (network, host, user, content) describe the
    /// *environment*, which is the correct scope for host plugin decisions.
    pub(crate) fn non_project_tags(&self) -> BTreeSet<String> {
        // Tags from non-project scopes (network, host, user, content).
        let mut result: BTreeSet<String> = self
            .scopes
            .iter()
            .filter(|s| s.kind != "project")
            .flat_map(|s| s.tags.iter().cloned())
            .collect();
        // Preserve tags in the union that no scope contributed (e.g. OS
        // tag added by evaluate()) and that aren't extra_tags. These are
        // never project-scoped.
        result.extend(
            self.tags
                .iter()
                .filter(|t| {
                    !self.scopes.iter().any(|s| s.tags.contains(t)) && !self.extra_tags.contains(*t)
                })
                .cloned(),
        );
        result
    }
}

pub fn evaluate(cfg: &Config, env: &Env) -> ActiveScopes {
    let mut scopes = Vec::new();
    for s in &cfg.scope.network {
        if network::matches_network(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "network",
                tags: crate::scope::tags::sanitize_tags(s.tags.clone(), "config.yaml scope tags"),
                ..Default::default()
            });
        }
    }
    for s in &cfg.scope.host {
        if matcher::matches_host(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "host",
                tags: crate::scope::tags::sanitize_tags(s.tags.clone(), "config.yaml scope tags"),
                ..Default::default()
            });
        }
    }
    for s in &cfg.scope.user {
        if matcher::matches_user(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "user",
                tags: crate::scope::tags::sanitize_tags(s.tags.clone(), "config.yaml scope tags"),
                ..Default::default()
            });
        }
    }
    let cwd = std::path::Path::new(&env.cwd);
    if cwd.exists() && !cfg.scope.content.is_empty() {
        let matched_ids = matcher::matches_content_all(&cfg.scope.content, cwd);
        for s in &cfg.scope.content {
            if matched_ids.contains(s.id.as_str()) {
                scopes.push(ActiveScope {
                    id: s.id.clone(),
                    kind: "content",
                    tags: crate::scope::tags::sanitize_tags(
                        s.tags.clone(),
                        "config.yaml scope tags",
                    ),
                    ..Default::default()
                });
            }
        }
    }
    if let Some(p) = marker_file::discover_project(env) {
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
    let extra_tags: BTreeSet<String> = env.extra_tags.iter().cloned().collect();
    tags.extend(extra_tags.iter().cloned());
    let tags = crate::scope::tags::cap_aggregate_tags(tags);
    ActiveScopes {
        scopes,
        tags,
        extra_tags,
    }
}

#[cfg(test)]
mod tests;
