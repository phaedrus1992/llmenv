//! Remember which content-less bundles a session was already told about (#3218).
//!
//! The record is per session, like `stop_dedupe` and `read_once`. The global recall-health file
//! cannot hold a per-session record.

use std::collections::BTreeSet;
use std::path::Path;

use super::session_state;

/// The state folder that holds one record per session.
const DIR: &str = "bundle_notice";

/// Days a record stays on disk, the same window as the slippage stats.
const MAX_AGE_DAYS: u64 = 7;

/// The bundle names one session was told about.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Noticed {
    bundles: BTreeSet<String>,
}

/// The names in `names` that this session was not yet told about. Records them as told.
///
/// With no state folder, or no usable session id, nothing can be stored, so every name is
/// returned on every call. A repeated notice is better than a silent one.
pub(super) fn unnoticed(
    state_dir: Option<&Path>,
    session_id: Option<&str>,
    names: &[String],
) -> Vec<String> {
    if names.is_empty() {
        return Vec::new();
    }
    let (Some(state_dir), Some(id)) = (state_dir, session_id) else {
        return names.to_vec();
    };
    if !crate::paths::is_valid_short_name(id) {
        return names.to_vec();
    }
    let dir = state_dir.join(DIR);
    // The lock creates the folder, owner-only. It is held until this function returns.
    let Some(_lock) = session_state::lock_state_file(&dir, id, "bundle notice") else {
        return names.to_vec();
    };
    let path = dir.join(format!("{id}.json"));
    let Some(mut noticed) = read_record(&path) else {
        return names.to_vec();
    };
    let new = fresh_names(names, &noticed.bundles);
    if new.is_empty() {
        return new;
    }
    noticed.bundles.extend(new.iter().cloned());
    session_state::prune_stale_json_files(&dir, MAX_AGE_DAYS);
    session_state::prune_orphan_locks(&dir, MAX_AGE_DAYS);
    save_record(&path, &noticed);
    new
}

/// The record at `path`. A missing file is an empty record. A file that does not parse is logged
/// and replaced by the next save. Any other read error is logged, and `None` means the caller
/// cannot tell what was told, so it must not write.
fn read_record(path: &Path) -> Option<Noticed> {
    match std::fs::read(path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            crate::agent_notice::record_logged(format!(
                "llmenv: the bundle notice record {} does not parse ({e}), so its bundles may be \
                 noticed again. Fix: delete that file.",
                path.display()
            ));
            Noticed::default()
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Noticed::default()),
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the bundle notice record {} cannot be read ({e}), so a bundle notice \
                 repeats on every event. Fix: check the permissions of the state folder.",
                path.display()
            ));
            None
        }
    }
}

/// Write `noticed` to `path`. A write failure is logged, and the notice still goes out for this
/// event.
fn save_record(path: &Path, noticed: &Noticed) {
    let json = match serde_json::to_vec(noticed) {
        Ok(json) => json,
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the bundle notice record cannot be encoded ({e}), so a bundle notice \
                 repeats on every event. Fix: report this llmenv bug."
            ));
            return;
        }
    };
    if let Err(e) = crate::paths::write_owner_only_atomic(path, &json) {
        crate::agent_notice::record_logged(format!(
            "llmenv: the bundle notice record {} cannot be saved ({e}), so a bundle notice \
             repeats on every event. Fix: check the permissions of the state folder.",
            path.display()
        ));
    }
}

/// The names in `names` that are not in `told`, in input order, with no repeats.
fn fresh_names(names: &[String], told: &BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if !told.contains(name) && !out.contains(name) {
            out.push(name.clone());
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// A saved record reads back with the same bundle names, for any set of names.
        #[test]
        fn a_saved_record_reads_back_unchanged(
            names in proptest::collection::btree_set("[a-z0-9-]{1,12}", 0..8),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.json");
            save_record(&path, &Noticed { bundles: names.clone() });
            let back = read_record(&path).unwrap();
            prop_assert_eq!(back.bundles, names);
        }

        /// `fresh_names` keeps every unseen name once, in input order, and drops every told name.
        #[test]
        fn fresh_names_is_the_unseen_names_without_repeats(
            names in proptest::collection::vec("[a-c]{1,2}", 0..10),
            told in proptest::collection::btree_set("[a-c]{1,2}", 0..5),
        ) {
            let got = fresh_names(&names, &told);
            prop_assert!(got.iter().all(|g| !told.contains(g)));
            let unique: BTreeSet<&String> = got.iter().collect();
            prop_assert_eq!(unique.len(), got.len());
            for name in names.iter().filter(|n| !told.contains(*n)) {
                prop_assert!(got.contains(name), "unseen {name} must be returned");
            }
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_new_bundle_is_returned_once_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let first = unnoticed(Some(dir.path()), Some("s1"), &names(&["b1"]));
        let second = unnoticed(Some(dir.path()), Some("s1"), &names(&["b1"]));
        assert_eq!(first, names(&["b1"]));
        assert!(
            second.is_empty(),
            "a second call must not repeat: {second:?}"
        );
    }

    #[test]
    fn a_later_new_bundle_is_returned_alone() {
        let dir = tempfile::tempdir().unwrap();
        unnoticed(Some(dir.path()), Some("s1"), &names(&["b1"]));
        let got = unnoticed(Some(dir.path()), Some("s1"), &names(&["b1", "b2"]));
        assert_eq!(got, names(&["b2"]));
    }

    #[test]
    fn another_session_is_told_again() {
        let dir = tempfile::tempdir().unwrap();
        unnoticed(Some(dir.path()), Some("s1"), &names(&["b1"]));
        let got = unnoticed(Some(dir.path()), Some("s2"), &names(&["b1"]));
        assert_eq!(got, names(&["b1"]));
    }

    #[test]
    fn no_session_id_returns_every_name_every_time() {
        let dir = tempfile::tempdir().unwrap();
        let first = unnoticed(Some(dir.path()), None, &names(&["b1"]));
        let second = unnoticed(Some(dir.path()), None, &names(&["b1"]));
        assert_eq!(first, names(&["b1"]));
        assert_eq!(second, names(&["b1"]));
    }

    #[test]
    fn a_corrupt_record_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join(DIR);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("s1.json"), "not json").unwrap();
        let first = unnoticed(Some(dir.path()), Some("s1"), &names(&["b1"]));
        let second = unnoticed(Some(dir.path()), Some("s1"), &names(&["b1"]));
        assert_eq!(first, names(&["b1"]));
        assert!(
            second.is_empty(),
            "the replaced record must hold the bundle: {second:?}"
        );
    }
}
