//! Post-materialize helpers for Claude Code adapters.
//!
//! These functions run after `adapter.materialize()` completes: they seed
//! settings, inject cached auth, and handle Claude Code–specific setup.

use std::path::Path;

use crate::adapter::AgentAdapter;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::config::Config;
use crate::materialize::manifest::{AuthSource, AuthStatus};

/// Run Claude Code–specific post-materialize steps.
///
/// This seeds settings, injects cached auth, and handles Claude Code–specific
/// setup. Returns a default [`AuthStatus`] for non-Claude Code adapters.
pub(crate) fn run_claude_code_post_materialize(
    adapter: &dyn AgentAdapter,
    config: &Config,
    adapter_root: &Path,
    cache_path: &Path,
) -> anyhow::Result<AuthStatus> {
    if adapter.name() != ClaudeCodeAdapter.name() {
        return Ok(AuthStatus::default());
    }

    // Merge user-elected seeded settings after materialize succeeds (#172).
    // Must run post-materialize so a materialize failure leaves settings.json
    // either absent (new folder) or in its prior good state (re-render).
    crate::adapter::claude_code::apply_seeded_settings(cache_path, &config.init.seeded_settings)?;
    // Seed installMethod to suppress the "config install method is 'unknown'" warning (#346).
    crate::adapter::claude_code::seed_install_method(cache_path)?;
    // Seed the default statusLine hook so `llmenv statusline` renders out of the
    // box; never overwrites a user's own customization (#836).
    crate::adapter::claude_code::seed_status_line(cache_path)?;

    // Auth inheritance (#172): inject cached credentials after the adapter has
    // finished its own .claude.json writes (mcpServers upsert). Only fires
    // when the stable cache has an entry.
    Ok(inject_cached_auth_if_available(adapter_root, cache_path))
}

/// Inject cached credentials and auth into the adapter cache folder.
///
/// Reads OAuth credentials from the durable state, injects them into the
/// cache folder, then looks up the active auth entry and writes its details
/// to the manifest.
fn inject_cached_auth_if_available(adapter_root: &Path, cache_path: &Path) -> AuthStatus {
    inject_cached_credentials(adapter_root, cache_path);
    match crate::auth::choose_auth_for_inheritance(adapter_root) {
        Err(e) => {
            // Printed, not only logged: a session that starts with no account must say so.
            eprintln!("[llmenv] auth: no account inherited. {e:#}");
            AuthStatus::default()
        }
        Ok(choice) => {
            // Printed beside the account line, so a skipped file is never silent (#3842).
            if choice.skipped > 0 {
                eprintln!(
                    "[llmenv] auth: {} cache file(s) skipped as unreadable. Fix: repair or \
                     remove them, then run `llmenv login`.",
                    choice.skipped
                );
            }
            match choice.entry {
                None => AuthStatus::default(),
                Some(entry) => inherit_auth_entry(cache_path, entry),
            }
        }
    }
}

/// Write `entry` into the cache folder and return the status that names it.
fn inherit_auth_entry(cache_path: &Path, entry: crate::auth::AuthEntry) -> AuthStatus {
    match crate::auth::inject_auth_into_claude_json(cache_path, &entry) {
        Ok(()) => {
            eprintln!("[llmenv] auth: {} (inherited)", entry.email);
            AuthStatus {
                source: AuthSource::Inherited,
                id: Some(entry.uuid),
                email: Some(entry.email),
            }
        }
        Err(e) => {
            tracing::error!("auth inject failed for {} (non-fatal): {e}", entry.email);
            AuthStatus::default()
        }
    }
}

/// Inject cached OAuth credentials into the adapter cache folder.
///
/// Detects the credential backend and injects missing credentials from the
/// durable state into the adapter's cache folder.
fn inject_cached_credentials(adapter_root: &Path, cache_path: &Path) {
    let backend = crate::auth::credentials::Backend::detect();
    match crate::auth::credentials::inject_if_missing(backend, adapter_root, cache_path) {
        Ok(true) => eprintln!("[llmenv] auth: OAuth credential inherited from durable state"),
        Ok(false) => {}
        Err(e) => tracing::error!("credential inject failed (non-fatal): {e}"),
    }
}
