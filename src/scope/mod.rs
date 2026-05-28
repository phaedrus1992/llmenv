pub mod matcher;
pub mod network;

pub use matcher::Env;

use crate::config::Config;
use std::collections::BTreeSet;
use std::path::PathBuf;

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
            name: Some(p.name),
            description: p.description,
            unknown_fields: p.unknown_fields,
        });
    }
    let tags: BTreeSet<String> = scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
    ActiveScopes { scopes, tags }
}
