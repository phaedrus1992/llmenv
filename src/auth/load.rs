//! Loads the usable [`AuthEntry`] files from the auth cache folder, and counts the rest.

use std::path::Path;

use anyhow::Context;

use crate::auth::credentials::CACHE_FILE;
use crate::auth::{AuthEntry, auth_cache_dir};
use crate::util::time::RecordId;

/// The usable entries of the cache, newest-first, with the skipped files.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) entries: Vec<AuthEntry>,
    /// Cache files left out because they could not be read or parsed.
    pub(crate) skipped: usize,
    /// The first problem among the skipped files. It names one file in the session notice.
    pub(crate) first_problem: Option<String>,
}

impl Loaded {
    /// The most recently seen entry, which a new folder inherits. `None` when the cache holds no
    /// usable entry.
    pub(crate) fn newest(&self) -> Option<&AuthEntry> {
        self.entries.first()
    }
}

/// Load the usable entries from the cache, sorted newest-first.
///
/// An unreadable or unparseable cache file is logged, counted, and left out. A stale file from
/// a crashed write must not block normal operation.
///
/// # Errors
/// Returns an error when the cache directory cannot be read. Also returns an error when files
/// exist but none of them is usable. The error states how many files were skipped, so the
/// caller can report the cause instead of treating the cache as empty.
pub(super) fn load_counted(adapter_root: &Path) -> anyhow::Result<Loaded> {
    let dir = auth_cache_dir(adapter_root);
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Loaded {
                entries: Vec::new(),
                skipped: 0,
                first_problem: None,
            });
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading auth cache dir {}: {e}. Fix: make the directory readable, or remove \
                 it so llmenv recreates it.",
                dir.display()
            ));
        }
    };
    // A fixed order, so the first problem is the same on every run.
    let mut listed: Vec<_> = read_dir.collect();
    listed.sort_by_key(|e| e.as_ref().ok().map(std::fs::DirEntry::path));
    let mut entries: Vec<AuthEntry> = Vec::new();
    let mut first_problem: Option<String> = None;
    let mut skipped = 0_usize;
    for res in listed {
        let outcome = res
            .map_err(|e| anyhow::anyhow!("an entry in {} cannot be read: {e}", dir.display()))
            .and_then(|entry| read_auth_entry(&entry.path()));
        match outcome {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {}
            Err(e) => {
                tracing::error!("{e:#}");
                skipped += 1;
                first_problem.get_or_insert(format!("{e:#}"));
            }
        }
    }
    if let (true, Some(problem)) = (entries.is_empty(), first_problem.as_ref()) {
        anyhow::bail!(
            "every auth cache entry in {} is unreadable ({skipped} skipped), so no account can \
             be inherited. First problem: {problem}. Fix: repair or remove the unreadable file. \
             If a newer llmenv wrote it, upgrade llmenv instead, then run `llmenv login`.",
            dir.display()
        );
    }
    entries.sort_by(|a, b| {
        crate::util::time::cmp_instants(
            &b.last_seen,
            RecordId(&b.uuid),
            &a.last_seen,
            RecordId(&a.uuid),
            "auth entry last_seen",
        )
    });
    Ok(Loaded {
        entries,
        skipped,
        first_problem,
    })
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
    // An entry whose last_seen does not parse cannot be ordered. Every such entry ties, so the
    // first file listed would be inherited (#3857). Treat it as unusable instead.
    crate::util::time::validate_stored(&entry.last_seen).with_context(|| {
        format!(
            "auth entry {} has an unreadable last_seen. Fix: rewrite it as RFC 3339, such as \
             2026-01-01T00:00:00Z, or remove the file",
            path.display()
        )
    })?;
    Ok(Some(entry))
}
