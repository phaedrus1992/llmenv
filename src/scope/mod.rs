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
    /// `kind == "project"` (path_prefix → expanded prefix; marker → deepest
    /// ancestor of cwd containing the marker file). `None` for all other
    /// scope kinds.
    pub project_root: Option<PathBuf>,
    /// Bundles this scope manually enables (from the marker file's
    /// `enable_bundles` list). Only populated for project scopes matched
    /// via marker.
    pub enable_bundles: Vec<String>,
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
            });
        }
    }
    for s in &cfg.scope.project {
        if let Some(matched) = matcher::match_project(s, env) {
            // Static scope tags + tags declared in the marker file. Dedupe
            // while preserving "static first, then marker" order so output
            // is stable.
            let mut tags = s.tags.clone();
            for t in matched.extra_tags {
                if !tags.contains(&t) {
                    tags.push(t);
                }
            }
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "project",
                tags,
                project_root: Some(matched.root),
                enable_bundles: matched.enable_bundles,
            });
        }
    }
    let tags: BTreeSet<String> = scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
    ActiveScopes { scopes, tags }
}
