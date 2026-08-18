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

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Subdirectory of the durable state dir holding Claude Code's transcripts.
const PROJECTS_DIR: &str = "projects";
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

/// Subdirectory of the durable state dir holding Codex's transcripts —
/// `sessions/`, the direct analogue of Claude Code's `projects/` (#1105).
const CODEX_SESSIONS_DIR: &str = "sessions";
/// Subdirectory holding Codex's archived sessions, alongside `sessions/`.
const CODEX_ARCHIVED_SESSIONS_DIR: &str = "archived_sessions";
/// Codex's prompt-recall history file — same name and role as Claude Code's.
const CODEX_HISTORY_FILE: &str = "history.jsonl";
/// Codex's combined identity + OAuth token store (`$CODEX_HOME/auth.json`).
///
/// Unlike the [`COPIED_FILES`]/[`CODEX_COPIED_FILES`] contract — copy in only
/// when absent, never overwrite — this credential legitimately changes over
/// time (a user re-runs `codex login`, rotates a token), so pinning the first
/// captured copy forever would serve a stale or revoked credential to every
/// new folder indefinitely. [`inherit_codex_auth`]/[`capture_codex_auth`]
/// give it its own newest-wins contract instead (security-audit, #1421).
const CODEX_AUTH_FILE: &str = "auth.json";
/// Codex's [`COPIED_FILES`] equivalent — `auth.json` is deliberately excluded:
/// it gets its own newest-wins capture via [`capture_codex_auth`] rather than
/// this list's plain "copy in only when absent" contract (security-audit,
/// #1421).
const CODEX_COPIED_FILES: &[&str] = &[CODEX_HISTORY_FILE];

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

/// Point `<config_dir>/sessions` at `<state_dir>/sessions` (#1105) — the Codex
/// analogue of [`link_projects_dir`].
///
/// # Errors
/// Returns an error when the durable dir cannot be created, an existing tree
/// cannot be folded in, or the link cannot be created.
pub(crate) fn link_codex_sessions_dir(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    link_durable_dir(CODEX_SESSIONS_DIR, state_dir, config_dir)
}

/// Point `<config_dir>/archived_sessions` at `<state_dir>/archived_sessions`
/// (#1105), alongside [`link_codex_sessions_dir`].
///
/// # Errors
/// Returns an error when the durable dir cannot be created, an existing tree
/// cannot be folded in, or the link cannot be created.
pub(crate) fn link_codex_archived_sessions_dir(
    state_dir: &Path,
    config_dir: &Path,
) -> anyhow::Result<()> {
    link_durable_dir(CODEX_ARCHIVED_SESSIONS_DIR, state_dir, config_dir)
}

/// Copy each [`COPIED_FILES`] entry from the durable store into a folder that
/// has none of its own.
///
/// # Errors
/// Returns an error when a copy fails. A file absent from the store is a no-op.
pub(crate) fn inherit_copied_files(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    inherit_copied_files_named(state_dir, config_dir, COPIED_FILES)
}

/// [`inherit_copied_files`] over Codex's [`CODEX_COPIED_FILES`].
///
/// # Errors
/// Returns an error when a copy fails. A file absent from the store is a no-op.
pub(crate) fn inherit_codex_copied_files(
    state_dir: &Path,
    config_dir: &Path,
) -> anyhow::Result<()> {
    inherit_copied_files_named(state_dir, config_dir, CODEX_COPIED_FILES)
}

fn inherit_copied_files_named(
    state_dir: &Path,
    config_dir: &Path,
    files: &[&str],
) -> anyhow::Result<()> {
    for name in files {
        let src = state_dir.join(name);
        let dst = config_dir.join(name);
        if dest_already_present(&dst) || !is_real_file(&src) {
            continue;
        }
        copy_owner_only(&src, &dst)?;
    }
    Ok(())
}

/// True when `path` is a regular file (`symlink_metadata`, not `is_file()`
/// — #1341: `is_file()` follows a symlink, so a symlinked entry under
/// `state_dir`/`config_dir` would otherwise be read/copied through
/// silently). Skips (warns) rather than errors: a stray symlink here
/// shouldn't block a whole `materialize`/inherit pass over one file. A
/// missing source is the documented, common no-op case (nothing cached
/// yet) and stays silent; any other stat failure (permission denied, ...)
/// is warned rather than folded into the same "absent" outcome
/// (security-audit, #1341).
fn is_real_file(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            tracing::warn!(path = %path.display(), "inherit: skipping symlinked file");
            false
        }
        Ok(meta) => meta.is_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "inherit: could not stat file, skipping");
            false
        }
    }
}

/// True when something already exists at `path`, including a dangling
/// symlink (`symlink_metadata`, not `exists()` — #1341 security-audit:
/// `exists()` follows a symlink and reports `false` for a dangling one,
/// which would fall through to `std::fs::copy` opening
/// `O_CREAT|O_TRUNC` through the link and writing the copied content at
/// the link's target instead of replacing the link itself).
fn dest_already_present(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                tracing::warn!(
                    path = %path.display(),
                    "inherit: skipping copy — destination is a symlink"
                );
            }
            true
        }
        Err(_) => false,
    }
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
    capture_copied_files_named(state_dir, config_dir, COPIED_FILES)
}

/// [`capture_copied_files`] over Codex's [`CODEX_COPIED_FILES`].
///
/// # Errors
/// Returns an error when a copy fails.
pub(crate) fn capture_codex_copied_files(
    state_dir: &Path,
    config_dir: &Path,
) -> anyhow::Result<()> {
    capture_copied_files_named(state_dir, config_dir, CODEX_COPIED_FILES)
}

fn capture_copied_files_named(
    state_dir: &Path,
    config_dir: &Path,
    files: &[&str],
) -> anyhow::Result<()> {
    for name in files {
        let src = config_dir.join(name);
        let dst = state_dir.join(name);
        if dest_already_present(&dst) || !is_real_file(&src) {
            continue;
        }
        copy_owner_only(&src, &dst)?;
    }
    Ok(())
}

/// Copy `src` to `dst` and force the result to owner-only (0o600) permissions.
///
/// `std::fs::copy` propagates the *source's* mode to the destination — a
/// history/auth file created under a looser umask would otherwise carry that
/// looser mode into the durable store or a fresh hashed folder alike
/// (security-audit, #1421). Every file this module copies is either a
/// credential or prompt history, so owner-only is the right floor
/// unconditionally, matching how everything else llmenv writes into the
/// durable store and materialized folders is owner-only.
///
/// # Errors
/// Returns an error when the copy or the permission change fails.
fn copy_owner_only(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::copy(src, dst)
        .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting owner-only permissions on {}", dst.display()))?;
    }
    Ok(())
}

/// Copy `auth.json` into a folder that has none of its own — same "copy in
/// only when absent" contract as [`inherit_codex_copied_files`]. A fresh
/// folder never has a stale copy to protect, so freshness only matters on the
/// capture side; see [`capture_codex_auth`].
///
/// # Errors
/// Returns an error when a copy fails. A file absent from the store is a no-op.
pub(crate) fn inherit_codex_auth(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    let src = state_dir.join(CODEX_AUTH_FILE);
    let dst = config_dir.join(CODEX_AUTH_FILE);
    if dest_already_present(&dst) || !is_real_file(&src) {
        return Ok(());
    }
    copy_owner_only(&src, &dst)
}

/// Capture a folder's `auth.json` into the durable store, newest `mtime`
/// wins — unlike [`capture_copied_files`]'s "only when the store has none"
/// contract, an existing store copy is replaced when the folder's is newer
/// (security-audit, #1421). Codex's login is a single global credential, but
/// it does change over time (re-running `codex login`, rotating a token), and
/// pinning the first-ever capture forever would serve a stale or revoked
/// credential to every new folder indefinitely.
///
/// A destination that can't be read counts as older, so a corrupt or missing
/// store entry never blocks the folder's copy from winning — mirrors
/// [`is_newer_at`]'s same tie-break for the reverse reason (a missing
/// *source* there vs. a missing *destination* here).
///
/// # Errors
/// Returns an error when a copy fails. A file absent from the folder is a
/// no-op.
pub(crate) fn capture_codex_auth(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    let src = config_dir.join(CODEX_AUTH_FILE);
    let dst = state_dir.join(CODEX_AUTH_FILE);
    if !is_real_file(&src) {
        return Ok(());
    }
    if dest_already_present(&dst) && !src_is_newer(&src, &dst) {
        return Ok(());
    }
    copy_owner_only(&src, &dst)
}

/// Whether `src`'s mtime is strictly newer than `dst`'s. A destination whose
/// mtime can't be read counts as older, so the source wins.
fn src_is_newer(src: &Path, dst: &Path) -> bool {
    let Ok(src_modified) = std::fs::metadata(src).and_then(|m| m.modified()) else {
        return false;
    };
    match std::fs::metadata(dst).and_then(|m| m.modified()) {
        Ok(dst_modified) => src_modified > dst_modified,
        Err(_) => true,
    }
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
pub(crate) fn migrate_stranded_projects(
    adapter_root: &Path,
    state_dir: &Path,
) -> anyhow::Result<usize> {
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
/// Symlinked *entries* are skipped rather than followed — there's no TOCTOU-safe
/// way to follow one into a bounded tree, matching the discipline in
/// `adapter::claude_code::copy_dir_owner_only`.
///
/// That guarantee is per-entry, not whole-tree (#1066). `src` itself is a path
/// the kernel re-resolves on every syscall, so swapping it for a symlink between
/// the check and the `read_dir` would still redirect the walk. Closing that
/// needs `openat`-relative traversal — opening the parent once and working from
/// its file descriptor — applied across every tree walk in the repo, which is a
/// dependency decision (`cap-std`/`rustix`) plus a cross-cutting refactor rather
/// than a fix to this function. Severity is low: it requires winning a race as
/// the same user, who can already read these files.
fn move_dir_newest_wins(src: &Path, dst: &Path) -> anyhow::Result<()> {
    crate::adapter::skills::create_dir_owner_only(dst)
        .with_context(|| format!("creating {}", dst.display()))?;
    // `create_dir_owner_only`/`create_dir_all` is a no-op (success) when `dst`
    // already resolves through a symlink to an existing directory — it never
    // lstats first. Without this check every following `rename`/`copy` in
    // this call would land inside the symlink's target instead of the
    // intended tree (#1065).
    anyhow::ensure!(
        std::fs::symlink_metadata(dst).is_ok_and(|md| md.file_type().is_dir()),
        "durable transcript dir {} is not a real directory",
        dst.display()
    );
    // #1066: both ends are opened once and every entry below is reached
    // through those descriptors. `src`/`dst` continue downward for error
    // messages only — the kernel never re-resolves them, so a directory
    // swapped for a symlink after the checks above cannot redirect the move.
    use std::os::fd::AsFd as _;
    let src_dir = crate::paths::dirfd::open_dir_nofollow(src)
        .with_context(|| format!("reading {}", src.display()))?;
    let dst_dir = crate::paths::dirfd::open_dir_nofollow(dst)
        .with_context(|| format!("opening {}", dst.display()))?;
    let entries = crate::paths::dirfd::read_dir_entries(src_dir.as_fd())
        .with_context(|| format!("reading entry in {}", src.display()))?;
    for entry in entries {
        let from = src.join(&entry.name);
        let to = dst.join(&entry.name);
        if entry.is_symlink() {
            tracing::debug!("inherit: skipping symlink {}", from.display());
        } else if entry.is_dir() {
            move_dir_newest_wins(&from, &to)?;
        } else if entry.is_file() && is_newer_at(src_dir.as_fd(), dst_dir.as_fd(), &entry.name) {
            move_file_at(src_dir.as_fd(), dst_dir.as_fd(), &entry.name, &from, &to)?;
        }
    }
    Ok(())
}

/// Rename `from` onto `to`, falling back to copy-then-delete across devices.
/// Move `name` from one open directory to another, newest-wins already decided.
///
/// `from`/`to` are for error messages only. `renameat` is tried first and the
/// copy fallback exists for the cross-filesystem (`EXDEV`) case, which is why
/// the durable store being on a different mount doesn't break inheritance.
fn move_file_at(
    src_dir: std::os::fd::BorrowedFd<'_>,
    dst_dir: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    from: &Path,
    to: &Path,
) -> anyhow::Result<()> {
    use crate::paths::dirfd;

    if dirfd::rename_at(src_dir, name, dst_dir, name).is_ok() {
        return Ok(());
    }
    let bytes = dirfd::read_file_at(src_dir, name)
        .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    // Owner-only, matching everything else llmenv writes into the durable
    // store; the replace semantics (unlink, then create) are what keep a
    // symlink at the destination from being written through (#1065).
    dirfd::write_file_at(dst_dir, name, &bytes, rustix::fs::Mode::from(0o600))
        .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    if let Err(e) = dirfd::remove_file_at(src_dir, name) {
        tracing::debug!("inherit: could not drop {} after copy: {e}", from.display());
    }
    Ok(())
}

/// Whether `name` in `src_dir` is newer than the same name in `dst_dir`.
/// A destination that can't be read counts as older, so the source wins.
fn is_newer_at(
    src_dir: std::os::fd::BorrowedFd<'_>,
    dst_dir: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
) -> bool {
    let Some(src) = crate::paths::dirfd::mtime_at(src_dir, name) else {
        return false;
    };
    match crate::paths::dirfd::mtime_at(dst_dir, name) {
        Some(dst) => src > dst,
        None => true,
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

    // ---- Codex durable-state inheritance (#1105) ----

    /// First run: a fresh folder's `sessions/` becomes a symlink into the
    /// state dir, same as Claude Code's `projects/` (#1105).
    #[cfg(unix)]
    #[test]
    fn link_codex_sessions_creates_symlink_into_state_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&cfg).unwrap();

        link_codex_sessions_dir(&state, &cfg).unwrap();

        let link = cfg.join(CODEX_SESSIONS_DIR);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            state.join(CODEX_SESSIONS_DIR)
        );
    }

    /// A pre-existing real `sessions/` dir is folded into the state dir, then
    /// replaced by the symlink — its transcripts must survive.
    #[cfg(unix)]
    #[test]
    fn link_codex_sessions_folds_existing_real_dir_then_links() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        write(
            &cfg.join(CODEX_SESSIONS_DIR).join("-proj").join("a.jsonl"),
            "old",
        );

        link_codex_sessions_dir(&state, &cfg).unwrap();

        let link = cfg.join(CODEX_SESSIONS_DIR);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let moved = state.join(CODEX_SESSIONS_DIR).join("-proj").join("a.jsonl");
        assert_eq!(std::fs::read_to_string(moved).unwrap(), "old");
    }

    /// Re-render: a correct `sessions/` symlink is left alone.
    #[cfg(unix)]
    #[test]
    fn link_codex_sessions_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&cfg).unwrap();

        link_codex_sessions_dir(&state, &cfg).unwrap();
        link_codex_sessions_dir(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_link(cfg.join(CODEX_SESSIONS_DIR)).unwrap(),
            state.join(CODEX_SESSIONS_DIR)
        );
    }

    /// `archived_sessions/` gets the same fold-then-link treatment as
    /// `sessions/`.
    #[cfg(unix)]
    #[test]
    fn link_codex_archived_sessions_creates_symlink_into_state_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&cfg).unwrap();

        link_codex_archived_sessions_dir(&state, &cfg).unwrap();

        let link = cfg.join(CODEX_ARCHIVED_SESSIONS_DIR);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            state.join(CODEX_ARCHIVED_SESSIONS_DIR)
        );
    }

    /// First run: `history.jsonl` is copied in from the store when the folder
    /// has none.
    #[test]
    fn codex_copied_files_are_inherited_on_first_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&cfg).unwrap();
        write(&state.join(CODEX_HISTORY_FILE), "prompt-one\n");

        inherit_codex_copied_files(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(cfg.join(CODEX_HISTORY_FILE)).unwrap(),
            "prompt-one\n"
        );
    }

    /// A copied file's permissions are forced to owner-only regardless of the
    /// source's mode — `std::fs::copy` propagates the source's permission
    /// bits, which would otherwise carry a looser umask into the durable
    /// store (security-audit, #1421).
    #[cfg(unix)]
    #[test]
    fn copied_files_are_forced_owner_only_regardless_of_source_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&cfg).unwrap();
        let src = state.join(CODEX_HISTORY_FILE);
        write(&src, "prompt-one\n");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

        inherit_codex_copied_files(&state, &cfg).unwrap();

        let mode = std::fs::metadata(cfg.join(CODEX_HISTORY_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "a 0o644 source must not produce a 0o644 destination"
        );
    }

    /// First run: `auth.json` is copied in from the store when the folder has
    /// none.
    #[test]
    fn codex_auth_is_inherited_on_first_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&cfg).unwrap();
        write(&state.join(CODEX_AUTH_FILE), r#"{"OPENAI_API_KEY":"sk-x"}"#);

        inherit_codex_auth(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(cfg.join(CODEX_AUTH_FILE)).unwrap(),
            r#"{"OPENAI_API_KEY":"sk-x"}"#
        );
    }

    /// Re-render: an existing folder's own `auth.json` is never clobbered by
    /// the store's copy.
    #[test]
    fn codex_auth_is_never_overwritten_by_the_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&cfg).unwrap();
        write(&state.join(CODEX_AUTH_FILE), "stale-store-copy");
        write(&cfg.join(CODEX_AUTH_FILE), "folder-own-current-auth");

        inherit_codex_auth(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(cfg.join(CODEX_AUTH_FILE)).unwrap(),
            "folder-own-current-auth",
            "must not clobber the folder's own auth.json"
        );
    }

    /// Existing state: capture seeds the store from a folder that logged in
    /// directly, so the *next* folder can inherit it — without this the store
    /// never gains a copy, since Codex only writes `auth.json` into
    /// `$CODEX_HOME`.
    #[test]
    fn codex_auth_capture_seeds_an_empty_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&cfg.join(CODEX_AUTH_FILE), "from-folder");

        capture_codex_auth(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_AUTH_FILE)).unwrap(),
            "from-folder"
        );
    }

    /// Capture replaces the store's copy once the folder's `auth.json` is
    /// newer — a re-login or token rotation must propagate, unlike
    /// `history.jsonl`'s "first copy wins forever" contract (security-audit,
    /// #1421: pinning the first captured credential forever would serve a
    /// stale or revoked token to every new folder indefinitely).
    #[test]
    fn codex_auth_capture_replaces_the_store_when_the_folder_copy_is_newer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&state.join(CODEX_AUTH_FILE), "old-account");
        write(&cfg.join(CODEX_AUTH_FILE), "re-logged-in-account");
        let now = filetime::FileTime::now();
        filetime::set_file_mtime(
            cfg.join(CODEX_AUTH_FILE),
            filetime::FileTime::from_unix_time(now.unix_seconds() + 60, 0),
        )
        .unwrap();

        capture_codex_auth(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_AUTH_FILE)).unwrap(),
            "re-logged-in-account",
            "a newer folder credential must replace the store's stale one"
        );
    }

    /// The reverse of the above: an *older* folder copy must not roll the
    /// store's newer credential back — e.g. a stale hashed folder that never
    /// got cleaned up must not stomp the account a fresher folder captured.
    #[test]
    fn codex_auth_capture_does_not_roll_back_a_newer_store_copy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&cfg.join(CODEX_AUTH_FILE), "stale-folder-account");
        write(&state.join(CODEX_AUTH_FILE), "current-account");
        let now = filetime::FileTime::now();
        filetime::set_file_mtime(
            state.join(CODEX_AUTH_FILE),
            filetime::FileTime::from_unix_time(now.unix_seconds() + 60, 0),
        )
        .unwrap();

        capture_codex_auth(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_AUTH_FILE)).unwrap(),
            "current-account",
            "an older folder copy must not roll back a newer store credential"
        );
    }

    #[test]
    fn projects_and_history_names_are_stable() {
        assert_eq!(PathBuf::from(PROJECTS_DIR), PathBuf::from("projects"));
        assert_eq!(HISTORY_FILE, "history.jsonl");
    }

    // #1065: `copy_replacing`'s whole contract is "never write through a
    // symlink at `to`" — a plain `std::fs::copy` would follow the symlink
    // and overwrite whatever it points at instead of replacing the symlink
    // itself.
    //
    // #1066: both properties now belong to the fd-relative helpers — the
    // path-based `copy_replacing`/`is_newer` they were written against are
    // gone, replaced by `write_file_at`/`is_newer_at`, so the tests move with
    // them rather than being deleted alongside the code.
    #[cfg(unix)]
    #[test]
    fn moving_a_file_replaces_a_symlink_at_the_destination() {
        use std::os::fd::AsFd as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        write(&src.join("f"), "new-content");
        let victim = tmp.path().join("victim");
        write(&victim, "must-not-be-touched");
        std::os::unix::fs::symlink(&victim, dst.join("f")).unwrap();

        let src_dir = crate::paths::dirfd::open_dir_nofollow(&src).unwrap();
        let dst_dir = crate::paths::dirfd::open_dir_nofollow(&dst).unwrap();
        move_file_at(
            src_dir.as_fd(),
            dst_dir.as_fd(),
            std::ffi::OsStr::new("f"),
            &src.join("f"),
            &dst.join("f"),
        )
        .unwrap();

        assert!(
            !std::fs::symlink_metadata(dst.join("f"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink at the destination must be replaced, not written through"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("f")).unwrap(),
            "new-content"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "must-not-be-touched",
            "the symlink's target must be untouched"
        );
    }

    // #1065: following a symlink at the destination for the mtime comparison
    // would read the *target's* mtime — an old target makes a planted symlink
    // look "safe to clobber" and selects the vulnerable move path.
    #[cfg(unix)]
    #[test]
    fn is_newer_at_does_not_follow_a_symlink_at_destination() {
        use std::os::fd::AsFd as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        write(&src.join("f"), "content");
        let old_target = tmp.path().join("old-target");
        write(&old_target, "old");
        filetime::set_file_mtime(&old_target, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        std::os::unix::fs::symlink(&old_target, dst.join("f")).unwrap();

        let src_dir = crate::paths::dirfd::open_dir_nofollow(&src).unwrap();
        let dst_dir = crate::paths::dirfd::open_dir_nofollow(&dst).unwrap();
        assert!(
            !is_newer_at(src_dir.as_fd(), dst_dir.as_fd(), std::ffi::OsStr::new("f")),
            "a symlink at the destination is compared by its own metadata, not its target's"
        );
    }

    // #1065: `create_dir_owner_only`/`create_dir_all` succeeds (no-op) when
    // `dst` already resolves through a symlink to an existing directory —
    // it never lstats first. `move_dir_newest_wins` must refuse to proceed
    // rather than silently folding into the symlink's target.
    #[cfg(unix)]
    #[test]
    fn move_dir_newest_wins_rejects_a_symlinked_destination() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        write(&src.join("a.jsonl"), "content");
        let real_other_dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&real_other_dir).unwrap();
        let dst = tmp.path().join("dst-symlink");
        std::os::unix::fs::symlink(&real_other_dir, &dst).unwrap();

        let err = move_dir_newest_wins(&src, &dst).expect_err(
            "a destination that resolves through a symlink must be rejected, not folded into",
        );
        assert!(err.to_string().contains("not a real directory"));
        assert!(
            std::fs::read_dir(&real_other_dir).unwrap().next().is_none(),
            "nothing must have been written into the symlink's target"
        );
    }
}
