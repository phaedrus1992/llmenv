//! Memory-backend endpoint resolution for hook events: which backend the active scope names,
//! and the cached client that talks to it.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Mutex, OnceLock};

use super::bundle_notice;
use super::mcp_client::McpHttpClient;
use super::memory_call::{MemoryCallContext, skipped_bundles};
use super::{HOOK_TIMEOUT, memory_url};
use crate::mcp::resolve::MemoryHookSettings;

/// A resolved memory-backend client plus the active `features.memory`
/// entry's hook settings (#1216, #2249) — the two travel together since both
/// come from the same resolved endpoint.
pub(super) struct ResolvedMemoryClient {
    pub(super) client: McpHttpClient,
    pub(super) settings: MemoryHookSettings,
}

/// Apply default memory type/importance markers from the resolved memory
/// entry's `settings` (R1, R3). `None` (no backend resolved) leaves the chunk
/// unchanged.
///
/// If the chunk already contains an `<!-- llmenv-type: -->` or
/// `<!-- llmenv-importance: -->` marker, the inline value takes precedence and
/// no default is appended. Otherwise the entry's `default_type` /
/// `default_importance` are appended as markers at the end of the chunk.
///
/// ponytail: `type_importance` per-type overrides are not yet applied here —
/// they will be resolved when the Store action runs against the ICM backend.
pub(super) fn apply_memory_config_defaults(
    mut chunk: String,
    settings: Option<&MemoryHookSettings>,
) -> String {
    let Some(settings) = settings else {
        return chunk;
    };

    if !chunk.contains("<!-- llmenv-type:")
        && let Some(ty) = &settings.default_type
    {
        chunk.push_str(&format!("\n<!-- llmenv-type: {} -->", ty.as_marker_str()));
    }

    if !chunk.contains("<!-- llmenv-importance:")
        && let Some(imp) = &settings.default_importance
    {
        chunk.push_str(&format!(
            "\n<!-- llmenv-importance: {} -->",
            imp.as_marker_str()
        ));
    }

    chunk
}

/// Resolve (or reuse from `cache`) the MCP client for the active memory
/// backend, for lifecycle-hook events. Returns `Option`, never `Result`: no
/// cause of an unresolved backend — including a bundle-merge failure
/// (#1132) — is fatal to the hook event. Memory actions are simply skipped;
/// session logging (independent of the memory backend) still proceeds. A
/// caller that instead wrote `memory_url(...)?.into_url()` would propagate
/// `memory_url`'s own `Err` via that leading `?` before `.into_url()` ever
/// ran, silently reintroducing the abort this function exists to prevent
/// (#1139).
pub(super) fn resolve_memory_client<E: std::fmt::Display>(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
    call: MemoryCallContext<'_, E>,
    cache: &'static OnceLock<Mutex<HashMap<String, McpHttpClient>>>,
) -> Option<ResolvedMemoryClient> {
    let MemoryCallContext {
        event,
        state_dir,
        session_id,
    } = call;
    let endpoint = match memory_url(config, config_dir, active) {
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
    let skipped = skipped_bundles(&endpoint);
    let (url, settings) = match endpoint.into_url_and_settings() {
        Ok(pair) => pair,
        Err(e) => {
            // A content-less bundle may declare memory, and its declaration cannot be read. The
            // agent is told once per session for each such bundle, so the cost stays low (#3218).
            let new = bundle_notice::unnoticed(state_dir, session_id, &skipped);
            if !new.is_empty() {
                crate::agent_notice::record_logged(format!(
                    "llmenv: memory {event} skipped: bundle(s) {} fired, but each has no content \
                     folder under the config dir's bundles/, or its name was rejected. llmenv \
                     cannot read whether it declares a memory backend. This notice shows once per \
                     session for each bundle. Fix: add the bundle content folder, or remove the \
                     bundle from config.yaml.",
                    new.join(", ")
                ));
            }
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::hook_run::tests::unreadable_bundle_fixture;

    // #1139: `memory_url(...)?.into_url()` at the call site put `?` directly
    // after `memory_url`, propagating *its* Err (a bundle-merge failure, #1132)
    // out of the enclosing function before `.into_url()` ever ran — bypassing
    // the fail-soft handling and aborting the whole hook event (session
    // logging included) on a merge failure. `resolve_memory_client` returns
    // `Option`, not `Result`, so it cannot repeat that mistake by construction.
    #[test]
    fn resolve_memory_client_does_not_propagate_a_bundle_merge_failure() {
        let (config_root, _cache, config, active) = unreadable_bundle_fixture();
        static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

        let client = resolve_memory_client(
            &config,
            config_root.path(),
            &active,
            MemoryCallContext {
                event: "test-event",
                state_dir: None,
                session_id: None,
            },
            &CACHE,
        );

        assert!(
            client.is_none(),
            "a bundle merge failure must degrade to no client, never propagate"
        );
    }

    /// #3202: a configured backend that fails to load reaches the agent, with a `Fix:` sentence.
    /// The marker is unique to this test, because the notice list is shared by the process.
    #[test]
    fn resolve_memory_client_notifies_agent_when_configured_backend_fails() {
        let (config_root, _cache, config, active) = unreadable_bundle_fixture();
        static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

        let client = resolve_memory_client(
            &config,
            config_root.path(),
            &active,
            MemoryCallContext {
                event: "agent-notice-3202-loud",
                state_dir: None,
                session_id: None,
            },
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

    /// #3202: a scope with no memory backend adds no agent notice, so the notice does not repeat
    /// on every hook event for a config that never had one.
    #[test]
    fn resolve_memory_client_adds_no_notice_when_no_backend_is_configured() {
        let config_root = tempfile::tempdir().expect("test");
        let config = crate::config::Config::default();
        let active = crate::scope::ActiveScopes::default();
        static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

        let client = resolve_memory_client(
            &config,
            config_root.path(),
            &active,
            MemoryCallContext {
                event: "agent-notice-3202-quiet",
                state_dir: None,
                session_id: None,
            },
            &CACHE,
        );

        assert!(client.is_none());
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(
            !notices.contains("agent-notice-3202-quiet"),
            "an unconfigured scope must not reach the agent: {notices}"
        );
    }

    /// #3202: a fired bundle with no content directory adds no agent notice, because its
    /// `features.memory` cannot be read. Only a bundle that loads and fails reaches the agent.
    #[test]
    fn resolve_memory_client_notices_a_bundle_without_content_once_per_session() {
        let config_root = tempfile::tempdir().expect("test");
        let cache_dir = tempfile::tempdir().expect("test");
        let state_dir = tempfile::tempdir().expect("test");
        let config = crate::config::Config {
            bundle: vec![crate::config::Bundle {
                name: "contentless-3218".into(),
                when: vec!["mytag".into()],
            }],
            cache: crate::config::Cache {
                cache_dir: cache_dir.path().to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            scopes: vec![],
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            extra_tags: std::collections::BTreeSet::new(),
        };
        static CACHE: OnceLock<Mutex<HashMap<String, McpHttpClient>>> = OnceLock::new();

        let call = || MemoryCallContext {
            event: "agent-notice-3218",
            state_dir: Some(state_dir.path()),
            session_id: Some("s-3218"),
        };
        let mut results = Vec::new();
        let first_logs = crate::test_log_capture::capture_logs(|| {
            results.push(
                resolve_memory_client(&config, config_root.path(), &active, call(), &CACHE)
                    .is_none(),
            );
        });
        let second_logs = crate::test_log_capture::capture_logs(|| {
            results.push(
                resolve_memory_client(&config, config_root.path(), &active, call(), &CACHE)
                    .is_none(),
            );
        });

        assert_eq!(
            results,
            vec![true, true],
            "both calls must return no client"
        );
        let notice = "This notice shows once per session";
        assert_eq!(
            first_logs.matches(notice).count(),
            1,
            "the first event must notice the bundle: {first_logs}"
        );
        assert_eq!(
            second_logs.matches(notice).count(),
            0,
            "the second event in the same session must not repeat it: {second_logs}"
        );
    }
}
