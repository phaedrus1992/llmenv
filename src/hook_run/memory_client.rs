//! The memory-backend client that a hook event uses, resolved from the active scope.

use std::collections::HashMap;
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
    let (url, settings) = match crate::memory::memory_url(config, config_dir, active)
        .and_then(crate::memory::MemoryEndpoint::into_url_and_settings)
    {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("llmenv: memory {event} skipped: {e}");
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
                    // Hook stderr never reaches the agent, so the same reason is also a notice (#3129).
                    let notice = format!(
                        "llmenv: memory {event} skipped: invalid memory backend URL: {e}. Fix: \
                         correct the memory URL in the config."
                    );
                    eprintln!("{notice}");
                    crate::agent_notice::record(notice);
                    return None;
                }
            }
        }
    };
    Some(ResolvedMemoryClient { client, settings })
}
