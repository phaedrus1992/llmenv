//! The link state of one session in the map, and the pending marker that guards a transcript
//! start (#2834).
//!
//! A transcript start writes a pending marker first. When ICM starts the transcript but the link
//! cannot be recorded, the marker stays. Later events then see a start in progress and do not
//! start a second transcript, which would be a second orphan. The marker expires, so a start
//! that failed is tried again later.

use std::path::Path;

use llmenv_paths::write_owner_only_atomic;

use super::{insert_capped, load_at, load_for_write, lock_map};

/// The map value prefix for a transcript start in progress. A real ICM id never has it.
const PENDING_PREFIX: &str = "pending:";

/// How long a pending marker blocks a new start. A start takes seconds, so ten minutes passes
/// only when a start failed after its marker was written. The value is fixed, because only the
/// retry delay after such a failure depends on it.
const DEFAULT_PENDING_TTL_SECS: i64 = 600;

/// The link of one session in the map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Link {
    /// No entry, or a pending marker that expired.
    Missing,
    /// A transcript start is in progress, or its link could not be recorded. `since` is the Unix
    /// second the marker was written.
    Pending { since: i64 },
    /// The session is linked to this ICM transcript id.
    Linked(String),
}

/// The link that a map `value` gives at `now` (Unix seconds).
///
/// A marker that does not parse cannot say when it was written, so it counts as expired. A
/// marker dated in the future also counts as expired: after the clock moves backward, it would
/// otherwise block every start until the clock catches up.
fn classify(value: Option<&str>, now: i64) -> Link {
    let Some(value) = value else {
        return Link::Missing;
    };
    let Some(since) = value.strip_prefix(PENDING_PREFIX) else {
        return Link::Linked(value.to_owned());
    };
    match since.parse::<i64>() {
        Ok(since) if (0..DEFAULT_PENDING_TTL_SECS).contains(&now.saturating_sub(since)) => {
            Link::Pending { since }
        }
        _ => Link::Missing,
    }
}

/// Whether a map `value` is a pending marker that has expired at `now`.
pub(super) fn is_expired_marker(value: &str, now: i64) -> bool {
    value.starts_with(PENDING_PREFIX) && classify(Some(value), now) == Link::Missing
}

/// The link of `claude_session_id` in the map at `path`.
///
/// # Errors
/// The map file exists but cannot be read or parsed.
pub(crate) fn link_at(path: &Path, claude_session_id: &str, now: i64) -> anyhow::Result<Link> {
    let map = load_at(path)?;
    Ok(classify(
        map.get(claude_session_id).map(String::as_str),
        now,
    ))
}

/// Write a pending marker for `claude_session_id`, if its link is still `seen`. `false` means
/// another hook changed the link first, so this one must not start a transcript. The write also
/// proves that the map can be written before ICM starts a transcript (#2667).
///
/// # Errors
/// The lock cannot be taken, or the map cannot be read or written.
pub(crate) fn claim_pending_at(
    path: &Path,
    claude_session_id: &str,
    seen: &Link,
    now: i64,
) -> anyhow::Result<bool> {
    let _lock = lock_map(path)?;
    let mut map = load_for_write(path)?;
    let current = classify(map.get(claude_session_id).map(String::as_str), now);
    if current != *seen || matches!(current, Link::Pending { .. }) {
        return Ok(false);
    }
    insert_capped(
        &mut map,
        claude_session_id,
        &format!("{PENDING_PREFIX}{now}"),
        now,
    );
    write_owner_only_atomic(path, serde_json::to_string(&map)?.as_bytes())?;
    Ok(true)
}

/// Remove the pending marker of `claude_session_id`, after a start that failed. A real link is
/// kept.
///
/// # Errors
/// The lock cannot be taken, or the map cannot be read or written.
pub(crate) fn clear_pending_at(path: &Path, claude_session_id: &str) -> anyhow::Result<()> {
    let _lock = lock_map(path)?;
    let mut map = load_for_write(path)?;
    let is_pending = map
        .get(claude_session_id)
        .is_some_and(|value| value.starts_with(PENDING_PREFIX));
    if is_pending {
        map.remove(claude_session_id);
        write_owner_only_atomic(path, serde_json::to_string(&map)?.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session_log::state::{lookup_session_at, record_session_at};

    fn map_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript-sessions.json");
        (dir, path)
    }

    #[test]
    fn a_fresh_marker_is_pending_and_an_old_or_bad_one_is_missing() {
        assert_eq!(classify(None, 1000), Link::Missing);
        assert_eq!(
            classify(Some("pending:1000"), 1000),
            Link::Pending { since: 1000 }
        );
        assert_eq!(
            classify(Some("pending:1000"), 1599),
            Link::Pending { since: 1000 }
        );
        // A marker from the future, after the clock moved backward, does not block a start.
        assert_eq!(classify(Some("pending:2000"), 1000), Link::Missing);
        assert_eq!(classify(Some("pending:1000"), 1600), Link::Missing);
        assert_eq!(classify(Some("pending:x"), 1000), Link::Missing);
        assert_eq!(
            classify(Some("icm-1"), 1000),
            Link::Linked("icm-1".to_owned())
        );
    }

    #[test]
    fn a_claim_blocks_a_second_claim_until_it_expires() {
        let (_dir, path) = map_path();
        assert!(claim_pending_at(&path, "c1", &Link::Missing, 1000).unwrap());
        assert_eq!(
            link_at(&path, "c1", 1001).unwrap(),
            Link::Pending { since: 1000 }
        );
        assert!(!claim_pending_at(&path, "c1", &Link::Missing, 1001).unwrap());
        // A lookup for logging sees no transcript while the start is pending.
        assert_eq!(lookup_session_at(&path, "c1").unwrap(), None);
        assert!(
            claim_pending_at(&path, "c1", &Link::Missing, 1000 + DEFAULT_PENDING_TTL_SECS).unwrap()
        );
    }

    #[test]
    fn a_claim_fails_when_another_hook_linked_the_session_first() {
        let (_dir, path) = map_path();
        record_session_at(&path, "c1", "icm-other").unwrap();
        assert!(!claim_pending_at(&path, "c1", &Link::Missing, 1000).unwrap());
        let seen = Link::Linked("icm-other".to_owned());
        assert!(claim_pending_at(&path, "c1", &seen, 1000).unwrap());
    }

    #[test]
    fn an_insert_drops_expired_markers_and_keeps_live_ones() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("old".to_owned(), "pending:0".to_owned());
        map.insert("fresh".to_owned(), "pending:1000".to_owned());
        map.insert("linked".to_owned(), "icm-a".to_owned());
        insert_capped(&mut map, "new", "icm-b", 1000);
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(keys, ["fresh", "linked", "new"]);
    }

    #[test]
    fn clear_removes_only_a_pending_marker() {
        let (_dir, path) = map_path();
        record_session_at(&path, "linked", "icm-a").unwrap();
        assert!(claim_pending_at(&path, "pending", &Link::Missing, 1000).unwrap());
        clear_pending_at(&path, "pending").unwrap();
        clear_pending_at(&path, "linked").unwrap();
        assert_eq!(link_at(&path, "pending", 1000).unwrap(), Link::Missing);
        assert_eq!(
            lookup_session_at(&path, "linked").unwrap().as_deref(),
            Some("icm-a")
        );
    }

    #[test]
    fn a_claim_fails_when_the_lock_file_cannot_be_opened() {
        // A directory where the lock file belongs stops the lock step. The map is not written.
        let (dir, path) = map_path();
        std::fs::create_dir(dir.path().join("transcript-sessions.lock")).unwrap();
        assert!(claim_pending_at(&path, "c1", &Link::Missing, 1000).is_err());
        assert!(!path.exists(), "no map may be written without the lock");
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn classify_follows_the_marker_rules(value in "\\PC{0,24}", now in -10_000i64..10_000) {
            let got = classify(Some(&value), now);
            match value.strip_prefix(PENDING_PREFIX) {
                None => prop_assert_eq!(got, Link::Linked(value.clone())),
                Some(rest) => {
                    let fresh = rest
                        .parse::<i64>()
                        .is_ok_and(|since| (0..DEFAULT_PENDING_TTL_SECS).contains(&now.saturating_sub(since)));
                    prop_assert_eq!(matches!(got, Link::Pending { .. }), fresh);
                    prop_assert_eq!(got == Link::Missing, !fresh);
                }
            }
        }

        #[test]
        fn a_marker_is_pending_only_inside_its_window(since in -10_000i64..10_000, age in -2_000i64..2_000) {
            let now = since + age;
            let got = classify(Some(&format!("{PENDING_PREFIX}{since}")), now);
            let pending = (0..DEFAULT_PENDING_TTL_SECS).contains(&age);
            prop_assert_eq!(got == (Link::Pending { since }), pending);
        }

        #[test]
        fn a_claim_never_changes_the_other_pairs(
            pairs in prop::collection::vec(("[a-z]{1,8}", "icm-[a-z0-9]{1,8}"), 0..8),
        ) {
            let (_dir, path) = map_path();
            for (claude, icm) in &pairs {
                record_session_at(&path, claude, icm).unwrap();
            }
            let mut before = load_at(&path).unwrap();
            before.remove("new-session-0");
            prop_assert!(claim_pending_at(&path, "new-session-0", &Link::Missing, 1000).unwrap());
            let mut after = load_at(&path).unwrap();
            after.remove("new-session-0");
            prop_assert_eq!(after, before);
        }
    }
}
