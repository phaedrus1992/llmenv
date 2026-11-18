//! Reads the cached [`AuthEntry`] files from the auth cache folder.

use std::path::Path;

use anyhow::Context;

use crate::auth::credentials::CACHE_FILE;
use crate::auth::{AuthEntry, auth_cache_dir};

/// Load all valid [`AuthEntry`] files from the cache, sorted newest-first.
///
/// An unreadable or unparseable cache file is logged and left out. A stale file from a
/// crashed write must not block normal operation.
///
/// # Errors
/// Returns an error when the cache directory cannot be read. Also returns an error when
/// files exist but none of them is usable. The caller then reports the cause, instead of
/// treating the cache as empty.
pub(super) fn load_all_auth_entries(adapter_root: &Path) -> anyhow::Result<Vec<AuthEntry>> {
    let dir = auth_cache_dir(adapter_root);
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading auth cache dir {}: {e}",
                dir.display()
            ));
        }
    };
    let mut entries: Vec<AuthEntry> = Vec::new();
    let mut first_problem: Option<String> = None;
    for res in read_dir {
        let path = match res {
            Ok(entry) => entry.path(),
            Err(e) => {
                let problem = format!("an entry in {} cannot be read: {e}", dir.display());
                tracing::error!("{problem}");
                first_problem.get_or_insert(problem);
                continue;
            }
        };
        match read_auth_entry(&path) {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {}
            Err(e) => {
                tracing::error!("{e:#}");
                first_problem.get_or_insert(format!("{e:#}"));
            }
        }
    }
    if let (true, Some(problem)) = (entries.is_empty(), first_problem) {
        anyhow::bail!(
            "every auth cache entry in {} is unreadable, so no account can be inherited. \
             First problem: {problem}. Fix: remove or repair the unreadable file, then run \
             `llmenv login`.",
            dir.display()
        );
    }
    entries.sort_by(|a, b| {
        crate::util::time::cmp_instants(&b.last_seen, &a.last_seen, "auth entry last_seen")
    });
    Ok(entries)
}

/// Read one file of the cache. `Ok(None)` marks a file that is not an auth entry.
fn read_auth_entry(path: &Path) -> anyhow::Result<Option<AuthEntry>> {
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
        return Ok(None);
    }
    // The credential cache (#1057) shares this directory but is not an
    // AuthEntry; without this it would warn on every load.
    if path.file_name().and_then(|n| n.to_str()) == Some(CACHE_FILE) {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read auth entry {}", path.display()))?;
    let entry = serde_json::from_slice::<AuthEntry>(&bytes)
        .with_context(|| format!("failed to parse auth entry {}", path.display()))?;
    Ok(Some(entry))
}
