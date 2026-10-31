//! The memory-backend client that a hook event uses, resolved from the active scope.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Mutex, OnceLock};

use super::{HOOK_TIMEOUT, McpHttpClient, MemoryHookSettings};

pub(super) struct ResolvedMemoryClient {
    pub(super) client: McpHttpClient,
    pub(super) settings: MemoryHookSettings,
}

/// Resolve (or reuse from `cache`) the MCP client for the active memory
/// backend, for lifecycle-hook events. Returns `Option`, never `Result`: no
/// cause of an unresolved backend — including a bundle-merge failure
/// (#1132) — is fatal to the hook event. Memory actions are simply skipped;
/// session logging (independent of the memory backend) still proceeds. A
/// caller that instead wrote `crate::memory::memory_url(...)?.into_url()` would propagate
/// `memory_url`'s own `Err` via that leading `?` before `.into_url()` ever
/// ran, silently reintroducing the abort this function exists to prevent
/// (#1139).
pub(super) fn resolve_memory_client(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
    event: impl std::fmt::Display,
    cache: &'static OnceLock<Mutex<HashMap<String, McpHttpClient>>>,
) -> Option<ResolvedMemoryClient> {
    let endpoint = match crate::memory::memory_url(config, config_dir, active) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            // A failed bundle merge means a backend may be configured but unreadable. Hook stderr
            // never reaches the agent, so the reason is also a notice (#3202).
            crate::agent_notice::record_logged(format!(
                "llmenv: memory {event} skipped: the memory backend failed to load: {e}. Fix: \
                 correct the bundle or config that declares features.memory."
            ));
            return None;
        }
    };
    let (url, settings) = match endpoint.into_url_and_settings() {
        Ok(pair) => pair,
        Err(e) => {
            // Silent for the agent on purpose. The scope has no backend, a bundle turns the
            // backend off, or a fired bundle has no content, so its declaration cannot be read.
            // A notice here would repeat on every event for such a config. A content-less bundle
            // that does declare memory is therefore not reported to the agent.
            let _ = writeln!(std::io::stderr(), "llmenv: memory {event} skipped: {e}");
            return None;
        }
    };
    let clients = cache.get_or_init(|| Mutex::new(HashMap::new()));
    let mut clients = clients.lock().unwrap_or_else(|e| e.into_inner());
    let client = match clients.entry(url) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.get().clone(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            match McpHttpClient::new(entry.key().clone(), HOOK_TIMEOUT) {
                Ok(client) => entry.insert(client).clone(),
                Err(e) => {
                    // Hook stderr never reaches the agent, so the reason is also a notice (#3129).
                    crate::agent_notice::record_logged(format!(
                        "llmenv: memory {event} skipped: invalid memory backend URL: {e}. Fix: \
                         correct the memory URL in the config."
                    ));
                    return None;
                }
            }
        }
    };
    Some(ResolvedMemoryClient { client, settings })
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    /// A config with one bundle and a cache dir, and the active tag `mytag`.
    fn scope_with_bundle(
        bundle: &str,
        cache_dir: &std::path::Path,
    ) -> (crate::config::Config, crate::scope::ActiveScopes) {
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: bundle.into(),
                when: vec!["mytag".into()],
            }],
            cache: crate::config::Cache {
                cache_dir: cache_dir.to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            scopes: vec![],
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            extra_tags: std::collections::BTreeSet::new(),
        };
        (config, active)
    }

    /// #3202: a configured backend that fails to load reaches the agent, with a `Fix:` sentence.
    /// A bundle whose `bundle.yaml` is a directory makes the merge fail. The marker is unique to
    /// this test, because the notice list is shared by the process.
    #[test]
    fn notifies_agent_when_configured_backend_fails() {
        let config_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config_root.path().join("bundles/broken/bundle.yaml")).unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let (config, active) = scope_with_bundle("broken", cache_dir.path());
        static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

        let client = resolve_memory_client(
            &config,
            config_root.path(),
            &active,
            "agent-notice-3202-loud",
            &CACHE,
        );

        assert!(client.is_none());
        let notices = crate::agent_notice::current().unwrap_or_default();
        let line = notices
            .lines()
            .find(|l| l.contains("agent-notice-3202-loud"))
            .expect("a configured backend that fails must reach the agent");
        assert!(line.contains("Fix:"), "the notice must name a fix: {line}");
    }

    /// #3202: a scope with no memory backend adds no agent notice.
    #[test]
    fn adds_no_notice_when_no_backend_is_configured() {
        let config_root = tempfile::tempdir().unwrap();
        let config = crate::config::Config::default();
        let active = crate::scope::ActiveScopes::default();
        static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

        let client = resolve_memory_client(
            &config,
            config_root.path(),
            &active,
            "agent-notice-3202-quiet",
            &CACHE,
        );

        assert!(client.is_none());
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(!notices.contains("agent-notice-3202-quiet"), "{notices}");
    }

    /// #3202: a fired bundle with no content directory adds no agent notice, because its
    /// `features.memory` cannot be read.
    #[test]
    fn adds_no_notice_for_a_bundle_without_content() {
        let config_root = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let (config, active) = scope_with_bundle("contentless", cache_dir.path());
        static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

        let client = resolve_memory_client(
            &config,
            config_root.path(),
            &active,
            "agent-notice-3202-contentless",
            &CACHE,
        );

        assert!(client.is_none());
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(
            !notices.contains("agent-notice-3202-contentless"),
            "{notices}"
        );
    }
}
