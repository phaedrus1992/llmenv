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
//! store, not a copy per hash. `history.jsonl` is copied in when absent rather
//! than linked: a single file rewritten via write-temp-then-rename would
//! replace the symlink with a regular file, which a directory link is immune to.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Subdirectory of the durable state dir holding Claude Code's transcripts.
pub const PROJECTS_DIR: &str = "projects";
/// Prompt-history file (`↑` recall) inherited alongside the transcripts.
pub const HISTORY_FILE: &str = "history.jsonl";

/// Point `<config_dir>/projects` at `<state_dir>/projects`.
///
/// A pre-existing real `projects/` directory is folded into the state dir before
/// being replaced by the link, so transcripts already in the folder survive.
/// Idempotent: a link already pointing at the target is left untouched.
///
/// # Errors
/// Returns an error when the durable dir cannot be created, an existing tree
/// cannot be folded in, or the link cannot be created.
pub fn link_projects_dir(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    let target = state_dir.join(PROJECTS_DIR);
    // 0o700, not create_dir_all's 0o777&~umask: this directory's listing is every
    // project the user has ever opened, and the transcripts under it are 0o600.
    crate::adapter::skills::create_dir_owner_only(&target)
        .with_context(|| format!("creating durable transcript dir {}", target.display()))?;
    let link = config_dir.join(PROJECTS_DIR);
    if clear_link_site(&link, &target)? {
        attach_store(&target, &link)?;
    }
    Ok(())
}

/// Copy the cached `history.jsonl` in when the folder has none.
///
/// Deliberately a copy, not a link: a single file rewritten via
/// write-temp-then-rename would replace a symlink with a regular file.
///
/// # Errors
/// Returns an error when the copy fails. A missing cached file is a no-op.
pub fn inherit_history_file(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    let src = state_dir.join(HISTORY_FILE);
    let dst = config_dir.join(HISTORY_FILE);
    if dst.exists() || !src.is_file() {
        return Ok(());
    }
    std::fs::copy(&src, &dst)
        .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

/// Move every `projects/` tree stranded in an old hashed folder into the durable
/// store, newest mtime winning per file. Returns how many trees were folded.
///
/// Handles both layouts: `<root>/<shape>/projects` (loose, strict) and
/// `<root>/<version>/<shape>/projects` (normal) — the old code only ever scanned
/// the flat one, which is why transcripts piled up unmigrated. The now-empty
/// source directories are left for `llmenv prune` to reclaim.
///
/// # Errors
/// Returns an error when a tree cannot be folded into the durable store.
pub fn migrate_stranded_projects(adapter_root: &Path, state_dir: &Path) -> anyhow::Result<usize> {
    let target = state_dir.join(PROJECTS_DIR);
    let mut folded = 0usize;
    for src in stranded_projects_dirs(adapter_root, state_dir) {
        move_dir_newest_wins(&src, &target)
            .with_context(|| format!("folding stranded transcripts from {}", src.display()))?;
        folded = folded.saturating_add(1);
    }
    Ok(folded)
}

/// Free the link site, returning whether a link should now be created.
///
/// `Ok(false)` means "nothing to do" — either the link is already correct, or the
/// path is something we deliberately refuse to replace.
fn clear_link_site(link: &Path, target: &Path) -> anyhow::Result<bool> {
    let file_type = match std::fs::symlink_metadata(link) {
        Ok(md) => md.file_type(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(anyhow::anyhow!("inspecting {}: {e}", link.display())),
    };
    if file_type.is_symlink() {
        if std::fs::read_link(link).is_ok_and(|dest| dest == target) {
            return Ok(false);
        }
        std::fs::remove_file(link)
            .with_context(|| format!("replacing stale link {}", link.display()))?;
        return Ok(true);
    }
    if file_type.is_dir() {
        move_dir_newest_wins(link, target)?;
        // Deliberately not remove_dir_all: Claude Code writes into this directory
        // while the session runs, so anything it created between the fold above
        // and this call would be destroyed rather than inherited. Prune the empty
        // skeleton the fold left, then require the directory itself to be empty —
        // if it isn't, new transcripts landed mid-swap, so leave everything alone
        // and let the next export fold them in.
        crate::materialize::prune_empty_dirs(link)?;
        if let Err(e) = std::fs::remove_dir(link) {
            tracing::warn!(
                "{} refilled during the swap ({e}); leaving it for the next export",
                link.display()
            );
            return Ok(false);
        }
        return Ok(true);
    }
    tracing::warn!(
        "{} is a regular file, not Claude Code's transcript dir — leaving it alone",
        link.display()
    );
    Ok(false)
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

/// Move `src` into `dst` recursively, keeping whichever file is newer.
///
/// Renames rather than copies: the stranded folders and the durable store both
/// live under the cache dir, so this is a metadata operation instead of a bulk
/// copy. That matters — a real machine had 1 GiB of transcripts stranded across
/// 27 folders, and this runs from `export`, which fires on every shell prompt.
/// Falls back to copy-then-delete when `rename` can't be used (cross-device).
///
/// Symlinks are skipped rather than followed — there's no TOCTOU-safe way to
/// follow one into a bounded tree, matching the discipline in
/// `adapter::claude_code::copy_dir_owner_only`.
fn move_dir_newest_wins(src: &Path, dst: &Path) -> anyhow::Result<()> {
    crate::adapter::skills::create_dir_owner_only(dst)
        .with_context(|| format!("creating {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry.with_context(|| format!("reading entry in {}", src.display()))?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_symlink() {
            tracing::debug!("inherit: skipping symlink {}", from.display());
        } else if file_type.is_dir() {
            move_dir_newest_wins(&from, &to)?;
        } else if file_type.is_file() && is_newer(&from, &to) {
            move_file(&from, &to)?;
        }
    }
    Ok(())
}

/// Rename `from` onto `to`, falling back to copy-then-delete across devices.
fn move_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)
        .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    if let Err(e) = std::fs::remove_file(from) {
        tracing::debug!("inherit: could not drop {} after copy: {e}", from.display());
    }
    Ok(())
}

/// True when `from` is newer than `to`, or `to` does not exist yet.
/// An unreadable source mtime is treated as "not newer" — never clobber on a stat failure.
fn is_newer(from: &Path, to: &Path) -> bool {
    let Ok(src) = from.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    match to.metadata().and_then(|m| m.modified()) {
        Ok(dst) => src > dst,
        Err(_) => true,
    }
}

/// Every stranded `projects/` directory under `adapter_root`, at both the flat
/// and version-nested depths. Excludes the durable store and already-linked
/// folders (following a link would fold the store into itself).
fn stranded_projects_dirs(adapter_root: &Path, state_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for level1 in child_dirs(adapter_root) {
        if level1 == state_dir {
            continue;
        }
        push_projects_dir(&level1, &mut out);
        for level2 in child_dirs(&level1) {
            push_projects_dir(&level2, &mut out);
        }
    }
    out
}

/// Immediate subdirectories of `dir`, skipping symlinks and staging dirs.
/// An unreadable directory yields nothing — migration is best-effort.
fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    // An absent dir is nothing to migrate; anything else (a permission error,
    // say) is warned rather than swallowed — treating it as "no children" would
    // silently skip a folder full of transcripts.
    let read_dir = match crate::paths::read_dir_optional(dir) {
        Ok(Some(rd)) => rd,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                "inherit: cannot scan {} for transcripts: {e:#}",
                dir.display()
            );
            return Vec::new();
        }
    };
    read_dir
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.ends_with(".tmp") {
                return None;
            }
            entry.file_type().ok()?.is_dir().then(|| entry.path())
        })
        .collect()
}

/// Push `<dir>/projects` onto `out` when it is a real directory (not a link).
fn push_projects_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let candidate = dir.join(PROJECTS_DIR);
    if std::fs::symlink_metadata(&candidate).is_ok_and(|md| md.file_type().is_dir()) {
        out.push(candidate);
    }
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

        inherit_history_file(&state, &cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(cfg.join(HISTORY_FILE)).unwrap(),
            "cached"
        );

        write(&cfg.join(HISTORY_FILE), "folder-own");
        inherit_history_file(&state, &cfg).unwrap();
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

        inherit_history_file(&state, &cfg).unwrap();
        assert!(!cfg.join(HISTORY_FILE).exists());
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

    #[test]
    fn projects_and_history_names_are_stable() {
        assert_eq!(PathBuf::from(PROJECTS_DIR), PathBuf::from("projects"));
        assert_eq!(HISTORY_FILE, "history.jsonl");
    }
}
