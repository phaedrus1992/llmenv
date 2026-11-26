//! The dead-folder marker. A folder read that finds a dead credential is not repeated on every
//! export, because a keychain read is a `security` subprocess that can prompt (#3008).

use std::path::{Path, PathBuf};

/// How long a dead folder read suppresses the next read, in milliseconds. Claude Code access
/// tokens last for hours, so a refreshed folder credential reaches the cache within this bound.
const DEAD_FOLDER_TTL_MS: i64 = 5 * 60 * 1000;

const MARKER_FILE: &str = "credentials-dead-folder.marker";

fn marker_path(adapter_root: &Path) -> PathBuf {
    adapter_root.join(MARKER_FILE)
}

/// Record that the folder read at `now_ms` held a dead credential.
///
/// # Errors
/// Returns an error when the marker cannot be written.
pub(super) fn mark(adapter_root: &Path, now_ms: i64) -> anyhow::Result<()> {
    let path = marker_path(adapter_root);
    crate::paths::write_owner_only_atomic(&path, now_ms.to_string().as_bytes()).map_err(|e| {
        anyhow::anyhow!(
            "writing {}: {e}. Fix: make sure the adapter state directory is writable.",
            path.display()
        )
    })
}

/// True when the last dead folder read is younger than [`DEAD_FOLDER_TTL_MS`].
///
/// A timestamp in the future is not recent. It can only come from a clock that moved back.
///
/// # Errors
/// Returns an error when the marker exists but cannot be read.
pub(super) fn is_recent(adapter_root: &Path, now_ms: i64) -> anyhow::Result<bool> {
    let path = marker_path(adapter_root);
    let text = match crate::paths::bounded::state_text_io(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading {}: {e}. Fix: delete the file; the next export then reads the keychain once.",
                path.display()
            ));
        }
    };
    match text.trim().parse::<i64>() {
        Ok(at) => Ok(crate::util::ttl::is_within(at, now_ms, DEAD_FOLDER_TTL_MS)),
        Err(_) => {
            // The keychain is read on every export until the next dead read rewrites the file.
            super::cache::report_ignored(&format!(
                "{} does not hold a timestamp, so the keychain is read on each export. Fix: \
                 delete the file.",
                path.display()
            ));
            Ok(false)
        }
    }
}

/// Remove the marker. Call it when a live credential is cached, so the next read is not held back.
///
/// # Errors
/// Returns an error when the marker exists but cannot be removed.
pub(super) fn clear(adapter_root: &Path) -> anyhow::Result<()> {
    let path = marker_path(adapter_root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "removing {}: {e}. Fix: delete the file by hand, then run the export again.",
            path.display()
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::*;
    use super::*;

    const PAST_MS: i64 = 1_000_000_000_000;
    const FUTURE_MS: i64 = 4_000_000_000_000;

    fn oauth(expires_at: i64) -> serde_json::Value {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat-TESTONLY",
                "refreshToken": "sk-ant-ort-TESTONLY",
                "expiresAt": expires_at,
            }
        })
    }

    fn write_folder(config_dir: &Path, expires_at: i64) {
        std::fs::create_dir_all(config_dir).unwrap();
        let value = oauth(expires_at);
        std::fs::write(
            config_dir.join(CREDENTIALS_FILE),
            serde_json::to_string(&value).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_dead_folder_is_not_cached_and_is_not_reread_for_a_hundred_exports() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        write_folder(folder.path(), PAST_MS);
        for _ in 0..100 {
            assert!(!cache_if_needed(Backend::File, root.path(), folder.path()).unwrap());
        }
        assert!(load_cached(root.path()).unwrap().is_none());
        // The folder is now live, but the marker holds the read back until its TTL passes.
        write_folder(folder.path(), FUTURE_MS);
        assert!(!cache_if_needed(Backend::File, root.path(), folder.path()).unwrap());
        assert!(load_cached(root.path()).unwrap().is_none());
    }

    #[test]
    fn a_read_after_the_ttl_picks_up_a_refreshed_folder() {
        let root = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        write_folder(folder.path(), FUTURE_MS);
        mark(root.path(), 1).unwrap();
        assert!(cache_if_needed(Backend::File, root.path(), folder.path()).unwrap());
        assert!(load_cached(root.path()).unwrap().is_some());
    }

    #[test]
    fn caching_a_live_credential_clears_the_marker() {
        let root = tempfile::tempdir().unwrap();
        mark(root.path(), now_ms()).unwrap();
        let live = Credentials::from_json(oauth(FUTURE_MS)).unwrap();
        save_cached(root.path(), &live).unwrap();
        assert!(!is_recent(root.path(), now_ms()).unwrap());
    }

    #[test]
    fn marker_is_recent_only_inside_the_ttl_window() {
        let root = tempfile::tempdir().unwrap();
        mark(root.path(), 0).unwrap();
        assert!(is_recent(root.path(), DEAD_FOLDER_TTL_MS - 1).unwrap());
        assert!(!is_recent(root.path(), DEAD_FOLDER_TTL_MS).unwrap());
    }

    #[test]
    fn a_marker_from_the_future_is_not_recent() {
        let root = tempfile::tempdir().unwrap();
        mark(root.path(), FUTURE_MS).unwrap();
        assert!(!is_recent(root.path(), 0).unwrap());
    }

    #[test]
    fn a_corrupt_marker_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(marker_path(root.path()), "not a timestamp").unwrap();
        assert!(!is_recent(root.path(), 0).unwrap());
        let notices = crate::agent_notice::current().unwrap_or_default();
        let file = marker_path(root.path()).display().to_string();
        assert!(notices.contains(&file), "{notices}");
        assert!(notices.contains("delete the file"), "{notices}");
    }

    #[test]
    fn a_missing_marker_clears_without_error() {
        let root = tempfile::tempdir().unwrap();
        clear(root.path()).unwrap();
        assert!(!is_recent(root.path(), 0).unwrap());
    }

    #[test]
    fn the_window_holds_at_the_extremes_of_the_clock() {
        let root = tempfile::tempdir().unwrap();
        mark(root.path(), i64::MIN).unwrap();
        assert!(!is_recent(root.path(), i64::MAX).unwrap());
        mark(root.path(), i64::MAX).unwrap();
        assert!(!is_recent(root.path(), i64::MIN).unwrap());
    }

    proptest::proptest! {
        #[test]
        fn recency_is_the_window_for_any_marker_and_clock(at in proptest::prelude::any::<i64>(), now in proptest::prelude::any::<i64>()) {
            let root = tempfile::tempdir().unwrap();
            mark(root.path(), at).unwrap();
            let expected = (0..DEAD_FOLDER_TTL_MS).contains(&now.saturating_sub(at));
            proptest::prop_assert_eq!(is_recent(root.path(), now).unwrap(), expected);
        }
    }
}
