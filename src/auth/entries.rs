//! Chooses the cached [`AuthEntry`] to inherit into a new folder.

use std::path::Path;

use crate::auth::AuthEntry;
use crate::auth::load::load_counted;

/// The cache entry chosen for a new folder, and what was left out to choose it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Inheritance {
    /// The chosen entry, or `None` when the cache holds no entry.
    pub(crate) entry: Option<AuthEntry>,
    /// Cache files left out because they could not be read or parsed.
    pub(crate) skipped: usize,
    /// The first problem among the skipped files. It names one file in the session notice.
    pub(crate) first_problem: Option<String>,
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
    let loaded = load_counted(adapter_root)?;
    // `load_counted` drops every entry whose last_seen does not parse, so the winner always has
    // a readable instant and no tie among bad values can reach this point (#3857).
    let entry = loaded.entries.first().cloned();
    for other in loaded.entries.iter().skip(1) {
        tracing::debug!(
            "auth cache: also have {} ({}); use `llmenv login` to switch",
            other.email,
            other.uuid
        );
    }
    Ok(Inheritance {
        entry,
        skipped: loaded.skipped,
        first_problem: loaded.first_problem,
    })
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "a test fails loudly when its fixture is wrong"
)]
mod tests {
    use jiff::Timestamp;
    use proptest::prelude::*;

    use super::choose_auth_for_inheritance;
    use crate::auth::credentials::CACHE_FILE;
    use crate::auth::load::load_counted;
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

    fn write_file(adapter_root: &std::path::Path, name: &str, bytes: &[u8]) {
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
            let loaded = load_counted(tmp.path()).unwrap().entries;
            prop_assert_eq!(loaded.len(), 1);
            prop_assert_eq!(&loaded[0].uuid, &saved.uuid);
            prop_assert_eq!(&loaded[0].email, &saved.email);
            prop_assert_eq!(&loaded[0].last_seen, &saved.last_seen);
            prop_assert_eq!(&loaded[0].raw, &saved.raw);
        }

        // The load fails only when no file is usable and at least one file is bad. A bad file
        // beside a good one must not block the good account. The skipped count equals the bad
        // files, so the session notice reports every one that was left out.
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
            match load_counted(tmp.path()) {
                Ok(loaded) => {
                    prop_assert_eq!(loaded.entries.len(), valid);
                    prop_assert_eq!(loaded.skipped, bad);
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
            let loaded = load_counted(tmp.path()).unwrap().entries;
            prop_assert_eq!(loaded.len(), stamps.len());
            for pair in loaded.windows(2) {
                let earlier: Timestamp = pair[0].last_seen.parse().unwrap();
                let later: Timestamp = pair[1].last_seen.parse().unwrap();
                prop_assert!(earlier >= later, "{} sorts before {}", pair[0].last_seen, pair[1].last_seen);
            }
        }

        // A bad last_seen sorts as the oldest. When any entry parses, the chosen entry must be
        // the newest parseable one, never a bad value that happens to sort first.
        #[test]
        fn choice_is_the_newest_parseable_last_seen(
            stamps in proptest::collection::vec(
                prop_oneof![
                    (0u32..60).prop_map(|s| format!("2025-01-01T00:00:{s:02}Z")),
                    "[a-z]{1,6}",
                ],
                1..6,
            ),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            for (index, stamp) in (0u32..).zip(stamps.iter()) {
                save_auth_entry(tmp.path(), &entry_with(index, "p@test.com", stamp)).unwrap();
            }
            let chosen = choose_auth_for_inheritance(tmp.path());
            let newest = stamps.iter().filter_map(|s| s.parse::<Timestamp>().ok()).max();
            match newest {
                Some(newest) => {
                    let entry = chosen.unwrap().entry.unwrap();
                    let chosen_at = entry.last_seen.parse::<Timestamp>().ok();
                    prop_assert_eq!(chosen_at, Some(newest));
                }
                // No stamp parses, so no entry can be ordered, and none may be inherited (#3857).
                None => prop_assert!(
                    chosen.is_err(),
                    "an entry with no readable last_seen was chosen"
                ),
            }
        }
    }

    #[test]
    fn an_unreadable_last_seen_is_skipped_and_the_readable_entry_is_inherited() {
        let tmp = tempfile::tempdir().unwrap();
        save_auth_entry(
            tmp.path(),
            &entry_with(1, "good@test.com", "2025-01-01T00:00:00Z"),
        )
        .unwrap();
        save_auth_entry(tmp.path(), &entry_with(2, "bad@test.com", "yesterday")).unwrap();
        let chosen = choose_auth_for_inheritance(tmp.path()).unwrap();
        assert_eq!(chosen.entry.unwrap().email, "good@test.com");
        assert_eq!(chosen.skipped, 1);
        assert!(chosen.first_problem.unwrap().contains("last_seen"));
    }

    #[test]
    fn the_first_problem_of_an_unusable_cache_is_the_file_that_sorts_first() {
        let tmp = tempfile::tempdir().unwrap();
        for index in [3, 1, 2] {
            save_auth_entry(tmp.path(), &entry_with(index, "x@test.com", "yesterday")).unwrap();
        }
        let mut files: Vec<_> = std::fs::read_dir(crate::auth::auth_cache_dir(tmp.path()))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        let err = choose_auth_for_inheritance(tmp.path()).err().unwrap();
        let text = format!("{err:#}");
        assert!(text.contains(&files[0].display().to_string()), "{text}");
    }

    #[test]
    fn a_cache_whose_last_seen_values_all_fail_to_parse_inherits_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        save_auth_entry(tmp.path(), &entry_with(1, "a@test.com", "yesterday")).unwrap();
        save_auth_entry(tmp.path(), &entry_with(2, "b@test.com", "last week")).unwrap();
        let err = choose_auth_for_inheritance(tmp.path()).err().unwrap();
        assert!(format!("{err:#}").contains("unreadable"), "{err:#}");
    }
}
