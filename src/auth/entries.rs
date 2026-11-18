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
/// Load the usable entries, and count the files that were skipped as unreadable.
///
/// The count lets the caller tell the user that an account was left out. A session
/// can otherwise inherit a different account with no sign that a file was skipped.
pub(super) fn load_counted(adapter_root: &Path) -> anyhow::Result<(Vec<AuthEntry>, usize)> {
    let dir = auth_cache_dir(adapter_root);
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading auth cache dir {}: {e}. Fix: make the directory readable, or remove \
                 it so llmenv recreates it.",
                dir.display()
            ));
        }
    };
    let mut entries: Vec<AuthEntry> = Vec::new();
    let mut first_problem: Option<String> = None;
    let mut skipped = 0_usize;
    for res in read_dir {
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
    if let (true, Some(problem)) = (entries.is_empty(), first_problem) {
        anyhow::bail!(
            "every auth cache entry in {} is unreadable, so no account can be inherited. \
             First problem: {problem}. Fix: repair or remove the unreadable file. If a newer \
             llmenv wrote it, upgrade llmenv instead, then run `llmenv login`.",
            dir.display()
        );
    }
    entries.sort_by(|a, b| {
        crate::util::time::cmp_instants(&b.last_seen, &a.last_seen, "auth entry last_seen")
    });
    Ok((entries, skipped))
}

/// The cache entry chosen for a new folder, and the number of cache files skipped.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Inheritance {
    /// The chosen entry, or `None` when the cache holds no entry.
    pub(crate) entry: Option<AuthEntry>,
    /// Cache files left out because they could not be read or parsed.
    pub(crate) skipped: usize,
}

/// Choose the best cached auth entry to inherit into a new folder.
///
/// Selects the most-recently-seen entry. When multiple entries exist, logs
/// the others so users know alternate accounts are cached.
///
/// # Errors
/// Returns an error on I/O failure reading the cache directory, or when every
/// cache file is unusable.
pub(crate) fn choose_auth_for_inheritance(adapter_root: &Path) -> anyhow::Result<Inheritance> {
    let (entries, skipped) = load_counted(adapter_root)?;
    let entry = match entries.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        [best, rest @ ..] => {
            for other in rest {
                tracing::debug!(
                    "auth cache: also have {} ({}); use `llmenv login` to switch",
                    other.email,
                    other.uuid
                );
            }
            Some(best.clone())
        }
    };
    Ok(Inheritance { entry, skipped })
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

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use std::path::Path;

    use jiff::Timestamp;
    use proptest::prelude::*;

    use super::{CACHE_FILE, load_counted};
    use crate::auth::{AuthEntry, auth_cache_dir, save_auth_entry};

    fn entry_with(index: u32, email: &str, last_seen: &str) -> AuthEntry {
        let uuid = format!("{index:08x}-0000-0000-0000-000000000000");
        AuthEntry {
            email: email.to_string(),
            last_seen: last_seen.to_string(),
            raw: serde_json::json!({ "id": uuid }),
            uuid,
        }
    }

    fn write_file(adapter_root: &Path, name: &str, bytes: &[u8]) {
        let dir = auth_cache_dir(adapter_root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    proptest! {
        // A saved entry must load back with every field intact. A field lost or renamed in the
        // JSON would change the account that a folder inherits.
        #[test]
        fn a_saved_entry_loads_back_with_every_field(
            index in 0u32..=u32::MAX,
            email in "[a-z]{1,8}",
            seconds in 0u32..60,
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let saved = entry_with(index, &format!("{email}@test.com"), &format!("2025-01-01T00:00:{seconds:02}Z"));
            save_auth_entry(tmp.path(), &saved).unwrap();
            let loaded = load_counted(tmp.path()).unwrap().0;
            prop_assert_eq!(loaded.len(), 1);
            prop_assert_eq!(&loaded[0].uuid, &saved.uuid);
            prop_assert_eq!(&loaded[0].email, &saved.email);
            prop_assert_eq!(&loaded[0].last_seen, &saved.last_seen);
            prop_assert_eq!(&loaded[0].raw, &saved.raw);
        }

        // The load fails only when no file is usable and at least one file is bad. A bad file
        // beside a good one must not block the good account.
        #[test]
        fn the_load_fails_only_when_no_entry_is_usable_and_a_file_is_bad(
            kinds in proptest::collection::vec(0u8..4, 0..8),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let mut valid = 0usize;
            let mut bad = 0usize;
            for (index, kind) in (0u32..).zip(kinds.iter()) {
                match kind {
                    0 => {
                        let saved = entry_with(index, "ok@test.com", "2025-01-01T00:00:00Z");
                        save_auth_entry(tmp.path(), &saved).unwrap();
                        valid += 1;
                    }
                    1 => {
                        write_file(tmp.path(), &format!("corrupt{index}.json"), b"{ not json");
                        bad += 1;
                    }
                    2 => write_file(tmp.path(), &format!("note{index}.txt"), b"ignored"),
                    _ => write_file(tmp.path(), CACHE_FILE, b"ignored"),
                }
            }
            match load_counted(tmp.path()).map(|(entries, _)| entries) {
                Ok(entries) => {
                    prop_assert_eq!(entries.len(), valid);
                    prop_assert!(valid > 0 || bad == 0);
                }
                Err(_) => {
                    prop_assert_eq!(valid, 0);
                    prop_assert!(bad > 0);
                }
            }
        }

        // The loaded list is newest-first by instant. Text order would put "Z" after "." and
        // reverse a whole second against a fraction of it (#3796).
        #[test]
        fn entries_load_newest_first_by_instant(
            stamps in proptest::collection::vec((0i64..4_000_000_000, 0i32..1_000_000_000), 1..6),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            for (index, (secs, nanos)) in (0u32..).zip(stamps.iter()) {
                let at = Timestamp::new(*secs, *nanos).unwrap();
                let saved = entry_with(index, "ord@test.com", &at.to_string());
                save_auth_entry(tmp.path(), &saved).unwrap();
            }
            let loaded = load_counted(tmp.path()).unwrap().0;
            prop_assert_eq!(loaded.len(), stamps.len());
            for pair in loaded.windows(2) {
                let earlier: Timestamp = pair[0].last_seen.parse().unwrap();
                let later: Timestamp = pair[1].last_seen.parse().unwrap();
                prop_assert!(earlier >= later, "{} sorts before {}", pair[0].last_seen, pair[1].last_seen);
            }
        }
    }
}
