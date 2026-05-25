pub mod matcher;
pub mod network;

pub use matcher::Env;

use crate::config::Config;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct ActiveScope {
    pub id: String,
    pub kind: &'static str,
    pub tags: Vec<String>,
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
            });
        }
    }
    for s in &cfg.scope.host {
        if matcher::matches_host(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "host",
                tags: s.tags.clone(),
            });
        }
    }
    for s in &cfg.scope.user {
        if matcher::matches_user(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "user",
                tags: s.tags.clone(),
            });
        }
    }
    for s in &cfg.scope.project {
        if matcher::matches_project(s, env) {
            scopes.push(ActiveScope {
                id: s.id.clone(),
                kind: "project",
                tags: s.tags.clone(),
            });
        }
    }
    let tags: BTreeSet<String> = scopes.iter().flat_map(|s| s.tags.iter().cloned()).collect();
    ActiveScopes { scopes, tags }
}
