//! Durable inheritance of Claude Code state that lives *inside* the hashed
//! config dir (#1059).
//!
//! `CLAUDE_CONFIG_DIR` carries a content hash, so anything Claude Code persists
//! under it dies on the next config edit or version bump. `/resume` reads its
//! session list from `projects/<escaped-cwd>/<session-uuid>.jsonl`, so a hash
//! change silently empties the resume list.
//!
//! `projects/` is relocated to the durable state dir (`<adapter_root>/state/`,
//! #175) and the materialized folder gets a symlink to it — one transcript
//! store, not a copy per hash.
//!
//! Single files ([`COPIED_FILES`]: `history.jsonl` for `↑` recall, and
//! `mcp-needs-auth-cache.json` recording which MCP servers still need an
//! authorization, #1058) are copied rather than linked, because a
//! write-temp-then-rename would replace a symlink with a regular file — a hazard
//! a directory link is immune to. They move both ways: captured from a folder
//! into the store when the store has none, and copied back into a new folder that
//! has none. Neither direction ever overwrites an existing copy.

use std::path::Path;

use anyhow::Context as _;

mod copy;
mod fold;
mod link_site;
mod stranded;
use copy::{copy_file_no_follow, dest_already_present, is_real_file};
use fold::move_dir_newest_wins;
use link_site::clear_link_site;
pub(crate) use stranded::migrate_stranded_projects;

/// Subdirectory of the durable state dir holding Claude Code's transcripts.
pub(crate) const PROJECTS_DIR: &str = "projects";
/// Subdirectory holding Claude Code's own internal session logs — unlike
/// `projects/`'s per-session transcripts, these accumulate as one file per
/// *calendar day* across every session, so a hash-directory change would
/// otherwise silently truncate the visible history to whatever's been
/// written since the last config edit (#1064).
const SESSION_LOGS_DIR: &str = "session-logs";
/// Prompt-history file (`↑` recall) inherited alongside the transcripts.
const HISTORY_FILE: &str = "history.jsonl";
/// Claude Code's record of which MCP servers still need an OAuth authorization.
/// Holds no tokens — losing it just makes Claude Code re-probe every server
/// after a hash change (#1058).
const MCP_NEEDS_AUTH_FILE: &str = "mcp-needs-auth-cache.json";

/// Files inherited by copying them in when the folder has none.
///
/// Copies rather than links: each is a single file that a write-temp-then-rename
/// would replace, turning a symlink back into a regular file. A folder's own copy
/// is never overwritten.
const COPIED_FILES: &[&str] = &[HISTORY_FILE, MCP_NEEDS_AUTH_FILE];

/// Point `<config_dir>/<name>` at `<state_dir>/<name>`, folding in a
/// pre-existing real directory first so its contents survive.
///
/// Shared by [`link_projects_dir`] and [`link_session_logs_dir`] — both are
/// "one durable directory per config dir" cases with identical fold/link
/// semantics, differing only in which name and which dir the caller is
/// durably relocating. Idempotent: a link already pointing at the target is
/// left untouched.
///
/// # Errors
/// Returns an error when the durable dir cannot be created, an existing tree
/// cannot be folded in, or the link cannot be created.
fn link_durable_dir(name: &str, state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    let target = state_dir.join(name);
    // 0o700, not create_dir_all's 0o777&~umask: both callers' directories hold
    // sensitive content (project paths and transcripts; session logs) that
    // shouldn't be group/world-readable.
    crate::adapter::skills::create_dir_owner_only(&target)
        .with_context(|| format!("creating durable dir {}", target.display()))?;
    let link = config_dir.join(name);
    if clear_link_site(&link, &target)? {
        attach_store(&target, &link)?;
    }
    Ok(())
}

/// Point `<config_dir>/projects` at `<state_dir>/projects`.
///
/// A pre-existing real `projects/` directory is folded into the state dir before
/// being replaced by the link, so transcripts already in the folder survive.
/// Idempotent: a link already pointing at the target is left untouched.
///
/// # Errors
/// Returns an error when the durable dir cannot be created, an existing tree
/// cannot be folded in, or the link cannot be created.
pub(crate) fn link_projects_dir(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    link_durable_dir(PROJECTS_DIR, state_dir, config_dir)
}

/// Point `<config_dir>/session-logs` at `<state_dir>/session-logs` (#1064).
///
/// Same fold-then-link treatment as [`link_projects_dir`]: a pre-existing
/// real `session-logs/` directory is folded into the state dir before being
/// replaced by the link, so history already in the folder survives.
///
/// # Errors
/// Returns an error when the durable dir cannot be created, an existing tree
/// cannot be folded in, or the link cannot be created.
pub(crate) fn link_session_logs_dir(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    link_durable_dir(SESSION_LOGS_DIR, state_dir, config_dir)
}

/// Copy each [`COPIED_FILES`] entry from the durable store into a folder that
/// has none of its own.
///
/// # Errors
/// Returns an error when a copy fails. A file absent from the store is a no-op.
pub(crate) fn inherit_copied_files(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    for name in COPIED_FILES {
        let src = state_dir.join(name);
        let dst = config_dir.join(name);
        if dest_already_present(&dst) || !is_real_file(&src) {
            continue;
        }
        copy_file_no_follow(&src, &dst)
            .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
    }
    Ok(())
}

/// Copy a folder's single-file state back into the durable store when the store
/// has no copy yet, so the next folder can inherit it.
///
/// Without this the store would never gain a `history.jsonl` or needs-auth cache
/// in the first place — Claude Code only ever writes them into the config dir.
/// Never overwrites the store's copy; the folder is not authoritative.
///
/// # Errors
/// Returns an error when a copy fails.
pub(crate) fn capture_copied_files(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    for name in COPIED_FILES {
        let src = config_dir.join(name);
        let dst = state_dir.join(name);
        if dest_already_present(&dst) || !is_real_file(&src) {
            continue;
        }
        copy_file_no_follow(&src, &dst)
            .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
    }
    Ok(())
}

/// Attach the durable store to the config dir.
#[cfg(unix)]
fn attach_store(target: &Path, link: &Path) -> anyhow::Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("linking {} -> {}", link.display(), target.display()))
}

/// No supported non-unix build exists, so fail loudly rather than ship an
/// untested copy path that would silently diverge from the linked behavior.
#[cfg(not(unix))]
fn attach_store(_target: &Path, _link: &Path) -> anyhow::Result<()> {
    anyhow::bail!("inheriting Claude Code transcripts needs symlink support (unix only)")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    /// Fresh folder: `projects/` becomes a symlink into the state dir.
    #[cfg(unix)]
    #[test]
    fn link_projects_creates_symlink_into_state_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        std::fs::create_dir_all(&cfg).unwrap();

        link_projects_dir(&state, &cfg).unwrap();

        let link = cfg.join(PROJECTS_DIR);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), state.join(PROJECTS_DIR));
    }

    /// #1064: `session-logs/` gets the same fold-then-link treatment as
    /// `projects/` — a fresh folder's `session-logs/` becomes a symlink into
    /// the state dir, and a pre-existing real directory's content survives
    /// the fold.
    #[cfg(unix)]
    #[test]
    fn link_session_logs_dir_creates_symlink_and_folds_existing_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        write(
            &cfg.join(SESSION_LOGS_DIR).join("2026-08-01.jsonl"),
            "log-line",
        );

        link_session_logs_dir(&state, &cfg).unwrap();

        let link = cfg.join(SESSION_LOGS_DIR);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            state.join(SESSION_LOGS_DIR)
        );
        assert_eq!(
            std::fs::read_to_string(state.join(SESSION_LOGS_DIR).join("2026-08-01.jsonl")).unwrap(),
            "log-line",
            "pre-existing session logs must survive the fold into the durable store"
        );
    }

    /// A pre-existing real `projects/` dir is folded into the state dir, then
    /// replaced by the symlink — its transcripts must survive.
    #[cfg(unix)]
    #[test]
    fn link_projects_folds_existing_real_dir_then_links() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        write(&cfg.join(PROJECTS_DIR).join("-proj").join("a.jsonl"), "old");

        link_projects_dir(&state, &cfg).unwrap();

        let link = cfg.join(PROJECTS_DIR);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let moved = state.join(PROJECTS_DIR).join("-proj").join("a.jsonl");
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "old");
    }

    /// Idempotent: a correct symlink is left alone.
    #[cfg(unix)]
    #[test]
    fn link_projects_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        std::fs::create_dir_all(&cfg).unwrap();

        link_projects_dir(&state, &cfg).unwrap();
        link_projects_dir(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_link(cfg.join(PROJECTS_DIR)).unwrap(),
            state.join(PROJECTS_DIR)
        );
    }

    /// A symlink pointing somewhere else gets re-pointed at the state dir.
    #[cfg(unix)]
    #[test]
    fn link_projects_repoints_wrong_symlink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, cfg.join(PROJECTS_DIR)).unwrap();

        link_projects_dir(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_link(cfg.join(PROJECTS_DIR)).unwrap(),
            state.join(PROJECTS_DIR)
        );
    }

    /// `history.jsonl` is copied in only when the folder has none.
    #[test]
    fn history_is_copied_in_when_absent_and_never_overwritten() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        std::fs::create_dir_all(&cfg).unwrap();
        write(&state.join(HISTORY_FILE), "cached");

        inherit_copied_files(&state, &cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(cfg.join(HISTORY_FILE)).unwrap(),
            "cached"
        );

        write(&cfg.join(HISTORY_FILE), "folder-own");
        inherit_copied_files(&state, &cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(cfg.join(HISTORY_FILE)).unwrap(),
            "folder-own",
            "must not clobber the folder's own history"
        );
    }

    /// No cached history is a silent no-op, not an error.
    #[test]
    fn history_absent_from_state_is_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        std::fs::create_dir_all(&cfg).unwrap();

        inherit_copied_files(&state, &cfg).unwrap();
        assert!(!cfg.join(HISTORY_FILE).exists());
    }

    /// #1341: a symlinked `history.jsonl` in the state dir must be skipped,
    /// not followed — `is_file()` alone would read through it.
    #[cfg(unix)]
    #[test]
    fn history_symlink_in_state_dir_is_not_followed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        let elsewhere = tmp.path().join("elsewhere.jsonl");
        write(&elsewhere, "secret");
        std::os::unix::fs::symlink(&elsewhere, state.join(HISTORY_FILE)).unwrap();

        inherit_copied_files(&state, &cfg).unwrap();
        assert!(!cfg.join(HISTORY_FILE).exists());
    }

    /// #1341: a dangling symlink at the destination reports `exists() ==
    /// false`, which would otherwise fall through to `std::fs::copy`
    /// writing the copied content at the symlink's target instead of
    /// replacing the link. Must be treated as "already present, skip".
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_at_destination_is_not_written_through() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        write(&state.join(HISTORY_FILE), "cached");
        let victim = tmp.path().join("does-not-exist-yet.jsonl");
        std::os::unix::fs::symlink(&victim, cfg.join(HISTORY_FILE)).unwrap();

        inherit_copied_files(&state, &cfg).unwrap();
        assert!(
            !victim.exists(),
            "must not create the dangling symlink's target"
        );
    }

    /// The stranding bug: `projects/` trees live at BOTH `<root>/<shape>/` (loose,
    /// strict) and `<root>/<version>/<shape>/` (normal). Migration must find both.
    #[test]
    fn migration_folds_flat_and_nested_layouts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let state = root.join("state");
        write(
            &root
                .join("TAG-abc")
                .join(PROJECTS_DIR)
                .join("-p")
                .join("flat.jsonl"),
            "flat",
        );
        write(
            &root
                .join("3.6")
                .join("hash1")
                .join(PROJECTS_DIR)
                .join("-p")
                .join("n1.jsonl"),
            "n1",
        );
        write(
            &root
                .join("3.7")
                .join("hash2")
                .join(PROJECTS_DIR)
                .join("-p")
                .join("n2.jsonl"),
            "n2",
        );

        let folded = migrate_stranded_projects(root, &state).unwrap();

        assert_eq!(folded, 3, "all three stranded trees must be folded in");
        let dst = state.join(PROJECTS_DIR).join("-p");
        assert_eq!(
            std::fs::read_to_string(dst.join("flat.jsonl")).unwrap(),
            "flat"
        );
        assert_eq!(std::fs::read_to_string(dst.join("n1.jsonl")).unwrap(), "n1");
        assert_eq!(std::fs::read_to_string(dst.join("n2.jsonl")).unwrap(), "n2");
    }

    /// Same session id in two folders: the newer file wins.
    #[test]
    fn migration_newest_wins_on_collision() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let state = root.join("state");
        let older = root
            .join("3.6")
            .join("h")
            .join(PROJECTS_DIR)
            .join("-p")
            .join("s.jsonl");
        let newer = root
            .join("3.7")
            .join("h")
            .join(PROJECTS_DIR)
            .join("-p")
            .join("s.jsonl");
        write(&older, "older");
        write(&newer, "newer");
        let long_ago = SystemTime::now() - Duration::from_secs(60 * 60);
        filetime_set(&older, long_ago);

        migrate_stranded_projects(root, &state).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(PROJECTS_DIR).join("-p").join("s.jsonl")).unwrap(),
            "newer"
        );
    }

    /// The state dir is not itself a migration source.
    #[test]
    fn migration_skips_the_state_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let state = root.join("state");
        write(
            &state.join(PROJECTS_DIR).join("-p").join("keep.jsonl"),
            "keep",
        );

        let folded = migrate_stranded_projects(root, &state).unwrap();

        assert_eq!(folded, 0);
        assert_eq!(
            std::fs::read_to_string(state.join(PROJECTS_DIR).join("-p").join("keep.jsonl"))
                .unwrap(),
            "keep"
        );
    }

    use std::time::{Duration, SystemTime};

    /// Backdate a file's mtime so collision ordering is deterministic.
    fn filetime_set(path: &Path, when: SystemTime) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    /// Migration moves rather than copies, so a 1 GiB transcript pile costs
    /// metadata instead of a bulk read+write on a path `export` hits constantly.
    #[test]
    fn migration_moves_files_out_of_the_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let state = root.join("state");
        let src = root
            .join("3.6")
            .join("h")
            .join(PROJECTS_DIR)
            .join("-p")
            .join("s.jsonl");
        write(&src, "body");

        migrate_stranded_projects(root, &state).unwrap();

        assert!(!src.exists(), "source file must be moved, not copied");
        assert_eq!(
            std::fs::read_to_string(state.join(PROJECTS_DIR).join("-p").join("s.jsonl")).unwrap(),
            "body"
        );
    }

    /// A file appearing mid-swap must be inherited later, never deleted. The old
    /// `remove_dir_all` would have destroyed it.
    #[cfg(unix)]
    #[test]
    fn swap_refilled_mid_flight_keeps_the_new_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        let target = state.join(PROJECTS_DIR);
        std::fs::create_dir_all(&target).unwrap();
        // A real dir whose file is NOT foldable-away: same name, newer in the
        // destination, so the fold leaves the source file in place.
        // Leave something the fold provably will not move: it skips symlinks by
        // design. That makes the "directory refilled" condition deterministic,
        // with no dependence on filesystem timestamp granularity.
        let outside = tmp.path().join("outside.jsonl");
        write(&outside, "not-ours");
        let leftover = cfg.join(PROJECTS_DIR).join("-p").join("link.jsonl");
        std::fs::create_dir_all(leftover.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &leftover).unwrap();

        link_projects_dir(&state, &cfg).unwrap();

        // The entry the fold could not move must still be there, not deleted, and
        // `projects/` must still be a real directory rather than a symlink — the
        // swap is deferred to the next export instead of destroying data.
        assert!(
            std::fs::symlink_metadata(&leftover).is_ok(),
            "an entry the fold skipped must not be destroyed"
        );
        assert!(
            !std::fs::symlink_metadata(cfg.join(PROJECTS_DIR))
                .unwrap()
                .file_type()
                .is_symlink(),
            "swap must be deferred while the directory still holds data"
        );
    }

    /// The needs-auth cache gets the same treatment as history — it's written into
    /// CLAUDE_CONFIG_DIR and dies with it, so losing it re-probes every MCP server.
    #[test]
    fn mcp_needs_auth_cache_is_inherited_like_history() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        std::fs::create_dir_all(&cfg).unwrap();
        write(
            &state.join(MCP_NEEDS_AUTH_FILE),
            r#"{"notion":{"timestamp":1}}"#,
        );

        inherit_copied_files(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(cfg.join(MCP_NEEDS_AUTH_FILE)).unwrap(),
            r#"{"notion":{"timestamp":1}}"#
        );
    }

    /// Capture seeds the store from a folder — without it the store never gains a
    /// copy, since Claude Code only writes these into the config dir.
    #[test]
    fn capture_seeds_the_store_and_never_clobbers_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("TAG-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&cfg.join(HISTORY_FILE), "from-folder");

        capture_copied_files(&state, &cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(state.join(HISTORY_FILE)).unwrap(),
            "from-folder"
        );

        write(&cfg.join(HISTORY_FILE), "newer-folder-copy");
        capture_copied_files(&state, &cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(state.join(HISTORY_FILE)).unwrap(),
            "from-folder",
            "the store's copy is authoritative once it exists"
        );
    }

    #[test]
    fn projects_and_history_names_are_stable() {
        assert_eq!(PathBuf::from(PROJECTS_DIR), PathBuf::from("projects"));
        assert_eq!(HISTORY_FILE, "history.jsonl");
    }

    #[test]
    fn a_destination_that_cannot_be_read_is_not_treated_as_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("plain");
        std::fs::write(&file, "x").expect("write a test file");
        // A file used as a folder makes `symlink_metadata` fail with a non-NotFound error.
        let under_a_file = file.join("child");
        let mut present = false;
        let logs = crate::test_log_capture::capture_logs(|| {
            present = dest_already_present(&under_a_file);
        });
        assert!(present, "an unreadable destination must not look absent");
        assert!(logs.contains("cannot read the destination"), "{logs}");
        assert!(logs.contains("Fix:"), "{logs}");
    }

    #[test]
    fn a_missing_destination_is_absent_and_logs_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut present = true;
        let logs = crate::test_log_capture::capture_logs(|| {
            present = dest_already_present(&dir.path().join("absent"));
        });
        assert!(!present);
        assert!(logs.is_empty(), "{logs}");
    }
}
