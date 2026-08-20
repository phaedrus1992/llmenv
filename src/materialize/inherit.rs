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
/// No liveness guard runs before this fold, unlike
/// [`sqlite_fold_would_race_a_live_db`] — deliberately, not by omission
/// (security-audit, #1451). [`move_dir_newest_wins`] relocates each file via
/// `rename`, which repoints a directory entry without touching the
/// underlying inode: a writer with the file already open keeps writing to
/// the same inode at its new location, so there is no SQLite-style split
/// between a moved "base" and an independently-opened sidecar for `rename`
/// to strand. The remaining risk is a *new* file created after this fold's
/// directory listing was taken; [`clear_link_site`] already covers that by
/// refusing to remove a directory the fold couldn't empty, deferring the
/// swap to the next `export` instead of destroying what landed mid-fold.
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

/// No liveness/atomicity guard runs before the copy, deliberately, not by
/// omission (security-audit, #1451). [`COPIED_FILES`]/[`CODEX_COPIED_FILES`]
/// entries are copied out of the store only when the destination folder has
/// none yet ([`dest_already_present`]), and the store's own copy is written
/// exactly once, by [`capture_copied_files_named`], which likewise never
/// overwrites an existing store entry. So by the time this function's source
/// read can race anything, that source has already stopped changing — the
/// narrow window is the very first capture into an empty store, and a torn
/// read there costs at most a slightly-off `history.jsonl`/needs-auth cache
/// for one folder, not lost or corrupted state (unlike `auth.json`'s
/// newest-mtime-wins contract in [`capture_codex_auth`], which does need a
/// guard because it *keeps* overwriting).
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
/// Uses [`crate::paths::copy_replacing_symlink`] rather than `std::fs::copy`
/// for two reasons: `std::fs::copy` propagates the *source's* mode to the
/// destination — a history/auth file created under a looser umask would
/// otherwise carry that looser mode into the durable store or a fresh hashed
/// folder alike (security-audit, #1421) — and it opens the destination
/// through `O_CREAT|O_TRUNC`, which writes *through* a symlink at `dst`
/// rather than replacing it; `copy_replacing_symlink`'s write-temp-then-rename
/// closes that gap the same way it already does for `cli::upgrade`'s binary
/// swap (security-audit, #1420). Every file this module copies is either a credential or prompt history, so
/// owner-only is the right floor unconditionally, matching how everything
/// else llmenv writes into the durable store and materialized folders is
/// owner-only.
///
/// # Errors
/// Returns an error when the copy or the permission change fails.
fn copy_owner_only(src: &Path, dst: &Path) -> anyhow::Result<()> {
    crate::paths::copy_replacing_symlink(src, dst)
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
/// Unlike the SQLite fold, this path never symlinks the source, so it has no
/// orphaned-inode failure mode — but it does have a torn-read one. Codex's
/// own `save()` (`codex-rs/login/src/auth/storage.rs`, `rust-v0.148.0`)
/// truncates and writes `auth.json` in place rather than temp-then-rename, so
/// a read racing a token refresh can observe a torn or empty file. There is
/// no sidecar artifact like SQLite's `-shm` to check beforehand, so the read
/// is bracketed by an mtime stat immediately before and after it, taken from
/// a single open handle rather than two path-based stats (security-audit,
/// #1451 — see [`read_auth_stable`]); a change in between, or content that
/// doesn't even parse as JSON despite a stable mtime (a coarse-grained
/// filesystem's mtime resolution can be coarser than the write it's meant to
/// catch), means the bytes are discarded rather than risked in the durable
/// store. The next capture retries once the file has settled.
///
/// # Errors
/// Returns an error when the write fails. A file absent from the folder, or
/// one that can't be read/stat'd stably, is a no-op — see
/// [`read_auth_stable`].
pub(crate) fn capture_codex_auth(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    capture_codex_auth_with_hook(state_dir, config_dir, || {})
}

/// [`capture_codex_auth`] with a hook run between the pre-read mtime stat and
/// the read itself, so a concurrent modification racing the read can be
/// simulated deterministically in tests without a real writer thread.
fn capture_codex_auth_with_hook(
    state_dir: &Path,
    config_dir: &Path,
    after_initial_stat: impl FnOnce(),
) -> anyhow::Result<()> {
    let src = config_dir.join(CODEX_AUTH_FILE);
    let dst = state_dir.join(CODEX_AUTH_FILE);
    if !is_real_file(&src) {
        return Ok(());
    }
    if dest_already_present(&dst) && !src_is_newer(&src, &dst) {
        return Ok(());
    }
    let Some(bytes) = read_auth_stable(&src, after_initial_stat) else {
        return Ok(());
    };
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        tracing::warn!(
            path = %src.display(),
            "inherit: auth.json read is not valid JSON despite a stable mtime \
             — likely a torn read a coarse-grained filesystem's timestamp \
             resolution didn't catch; skipping this capture, will retry on \
             the next export"
        );
        return Ok(());
    }
    crate::paths::write_owner_only(&dst, &bytes)
        .with_context(|| format!("writing {}", dst.display()))
}

/// Open `path` for reading without following a final-component symlink —
/// the file-level counterpart to
/// [`crate::paths::dirfd::open_dir_nofollow`]'s directory variant — then
/// read its full contents, but only if its mtime is unchanged immediately
/// before and after the read, both stat'd from that one handle.
///
/// A single fd instead of two path-based `std::fs::metadata` calls closes a
/// gap security-audit flagged (#1451): a path-based before/after bracket
/// only proves *some* object at `path` had a matching mtime at two points,
/// not that the bytes came from one stable inode — a symlink swapped in
/// between with a matching mtime would pass unnoticed, and `O_NOFOLLOW`
/// refuses one planted after [`is_real_file`]'s check outright rather than
/// following it.
///
/// `None` (with the reason logged) covers every way this can fail to
/// produce a trustworthy read — open/read/stat errors, and an mtime that
/// moved between the two stats — so the caller always treats it as "skip
/// this capture, the next `export` retries."
fn read_auth_stable(path: &Path, after_initial_stat: impl FnOnce()) -> Option<Vec<u8>> {
    let file = match open_file_nofollow(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "inherit: could not open auth.json for capture, skipping");
            return None;
        }
    };
    let before = file.metadata().and_then(|m| m.modified());
    after_initial_stat();
    let mut bytes = Vec::new();
    if let Err(e) = std::io::Read::read_to_end(&mut &file, &mut bytes) {
        tracing::warn!(path = %path.display(), error = %e, "inherit: could not read auth.json for capture, skipping");
        return None;
    }
    let after = file.metadata().and_then(|m| m.modified());
    match (before, after) {
        (Ok(b), Ok(a)) if b == a => Some(bytes),
        (Ok(_), Ok(_)) => {
            tracing::warn!(
                path = %path.display(),
                "inherit: auth.json changed mid-read — likely a concurrent Codex \
                 token refresh; skipping this capture, will retry on the next export"
            );
            None
        }
        (before, after) => {
            let error = before.err().or_else(|| after.err());
            tracing::warn!(path = %path.display(), error = ?error, "inherit: could not stat auth.json around the capture read, skipping");
            None
        }
    }
}

/// Unix: refuses a final-component symlink via `O_NOFOLLOW`, same protection
/// [`crate::paths::dirfd::open_dir_nofollow`] gives directories. Non-unix:
/// plain open — this module's symlink defenses are unix-only throughout
/// (see [`attach_store`]'s non-unix stub), so there is no narrower guarantee
/// to preserve here than on any other path in this file.
#[cfg(unix)]
fn open_file_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let fd = openat(CWD, path, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty())?;
    Ok(std::fs::File::from(fd))
}

#[cfg(not(unix))]
fn open_file_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
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

/// Codex's durable, non-rebuildable SQLite state DBs (#1420) — three of the
/// six databases Codex writes into `$CODEX_HOME`
/// (`codex-rs/state/src/sqlite.rs`) that hold data with no other durable
/// source:
///
/// - `goals_1.sqlite` — per-thread objectives/status/token budgets a user or
///   agent set; nothing else records them.
/// - `memories_1.sqlite` — generated memory content plus its extraction/
///   consolidation job state; unlike the state DB below, Codex has no
///   automatic backfill for it, so losing the file silently empties memory
///   and makes the extraction jobs redo expensive work rather than just
///   reindex.
/// - `queue_1.sqlite` — the durable user-message queue; a message queued but
///   not yet dequeued has no other record, so losing this file loses real,
///   user-visible pending work.
///
/// The other three DBs are deliberately **not** linked, each confirmed
/// against upstream Codex source (`codex-rs`, pinned `rust-v0.148.0`) rather
/// than assumed from speculation:
///
/// - `state_5.sqlite` — a rebuildable index over the `sessions/` rollout
///   files. Codex actively backfills it from scratch at startup, blocking on
///   `BackfillStatus::Complete` for up to 30s
///   (`codex-rs/rollout/src/state_db.rs`'s `wait_for_backfill_gate`).
/// - `thread_history_1.sqlite` — a lazy projection over the same rollout
///   files, keyed by a resumable byte offset that defaults to `0` when its
///   row is missing
///   (`codex-rs/thread-store/src/local/thread_history.rs`), so a missing
///   file just reprojects from the start on next use.
/// - `logs_2.sqlite` — a 10-day-retention diagnostic/feedback log
///   (`codex-rs/state/src/runtime/logs.rs`'s `LOG_RETENTION_DAYS`), not data
///   worth preserving across a hash change.
///
/// Linking either of the first two would only add WAL-consistency risk (see
/// [`SQLITE_SIDECAR_SUFFIXES`]) for zero durability gain.
const CODEX_GOALS_DB: &str = "goals_1.sqlite";
const CODEX_MEMORIES_DB: &str = "memories_1.sqlite";
const CODEX_QUEUE_DB: &str = "queue_1.sqlite";
const CODEX_INHERITED_SQLITE_DBS: &[&str] = &[CODEX_GOALS_DB, CODEX_MEMORIES_DB, CODEX_QUEUE_DB];

/// WAL-mode sidecar suffixes that must move in lockstep with the DB file they
/// belong to. Codex opens all six SQLite DBs in WAL mode
/// (`SqliteJournalMode::Wal`, `codex-rs/state/src/sqlite.rs`), so each of
/// [`CODEX_INHERITED_SQLITE_DBS`] carries its own `<name>-wal`/`<name>-shm`
/// files. SQLite opens these sidecars as independent paths — computed by
/// string-appending the suffix, not by resolving the base file's symlink
/// first — so symlinking only the base `.sqlite` file would leave the
/// sidecars as ordinary local files in the ephemeral config dir, silently
/// splitting a DB's committed data (main file, durable) from its uncommitted
/// WAL frames (sidecar, ephemeral). Symlinking each suffix as its own path
/// keeps `open()`'s symlink resolution working per file, closing that gap.
const SQLITE_SIDECAR_SUFFIXES: &[&str] = &["-wal", "-shm"];

/// Point each of [`CODEX_INHERITED_SQLITE_DBS`] — and its WAL sidecars — at
/// the durable state dir (#1420). Same fold-then-link contract as
/// [`link_durable_dir`], applied per file instead of per directory: a
/// pre-existing real file is folded in (newest mtime wins, since Codex is the
/// sole writer to either copy — same reasoning as [`capture_codex_auth`])
/// before being replaced by a symlink. Neither side needs to exist yet: a
/// dangling symlink is left in place so Codex's own `create_if_missing`
/// (`sqlite.rs`) writes straight into the durable store the first time it
/// opens a DB that has never existed on either side.
///
/// A family for which [`sqlite_fold_would_race_a_live_db`] returns `true` is
/// skipped entirely this pass rather than folded, to avoid racing a Codex
/// process that may still have it open (#1448); the next `export` retries
/// it.
///
/// # Errors
/// Returns an error when an existing file can't be folded in or a link can't
/// be created.
pub(crate) fn link_codex_sqlite_dbs(state_dir: &Path, config_dir: &Path) -> anyhow::Result<()> {
    for name in CODEX_INHERITED_SQLITE_DBS {
        if sqlite_fold_would_race_a_live_db(name, config_dir) {
            tracing::warn!(
                db = name,
                "inherit: {name} looks open by a running Codex process — \
                 skipping this pass's fold into the durable store, will retry \
                 on the next export"
            );
            continue;
        }
        // Decided once from the base `.sqlite` file and applied to every
        // member of the family (base + WAL sidecars) — never re-decided per
        // member. SQLite ties a WAL to the exact database it was written
        // against; letting each member pick its own winner independently
        // could fold in a base file from one point in time alongside a WAL
        // from another (security-audit, #1420).
        let prefer_local = local_sqlite_db_is_newer(name, state_dir, config_dir);
        for member in std::iter::once((*name).to_string()).chain(
            SQLITE_SIDECAR_SUFFIXES
                .iter()
                .map(|suffix| format!("{name}{suffix}")),
        ) {
            link_durable_sqlite_member(&member, state_dir, config_dir, prefer_local)?;
        }
    }
    Ok(())
}

/// True when at least one member of `name`'s family (base file or either of
/// [`SQLITE_SIDECAR_SUFFIXES`]) is still a real, not-yet-migrated file in
/// `config_dir` — i.e. [`link_durable_sqlite_member`]'s fold branch would
/// still fire on it. Once every member is already a symlink, the idempotent
/// no-op path never touches any of them, so there is nothing left to protect
/// against a live connection (#1450).
///
/// A stat failure other than "not found" counts as unmigrated — the same
/// err-toward-caution default [`sqlite_fold_would_race_a_live_db`] applies to
/// its own checks, since this helper only exists to decide whether that
/// liveness check needs to run at all.
fn family_has_an_unmigrated_member(name: &str, config_dir: &Path) -> bool {
    std::iter::once((*name).to_string())
        .chain(SQLITE_SIDECAR_SUFFIXES.iter().map(|s| format!("{name}{s}")))
        .any(
            |member| match std::fs::symlink_metadata(config_dir.join(&member)) {
                Ok(md) => md.file_type().is_file(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    tracing::warn!(db = name, member, error = %e, "inherit: could not stat Codex SQLite family member, treating as unmigrated to be safe");
                    true
                }
            },
        )
}

/// True when `name`'s family (base file + [`SQLITE_SIDECAR_SUFFIXES`]) has a
/// member still real (per [`family_has_an_unmigrated_member`]) *and* the
/// base name's `-shm` sidecar exists — SQLite creates `-shm` while a WAL-mode
/// connection is attached, and removes it once the last connection closes
/// cleanly, so its presence is evidence a running Codex process may still
/// hold the DB open (#1448). Folding a live DB risks reading a torn snapshot
/// relative to Codex's in-flight WAL frames, and after the swap Codex's open
/// file descriptor keeps writing to the orphaned inode instead of the new
/// symlink target — losing those writes once it closes the file.
///
/// Checked family-wide, not just on the base file: a prior
/// `link_codex_sqlite_dbs` run can partially fail after symlinking the base
/// but before symlinking a sidecar (e.g. `attach_store_atomic` erroring on
/// the sidecar's rename after the base's succeeded), leaving the base
/// migrated while a sidecar is still real and potentially live. Gating on the
/// base file alone would miss exactly that case (#1450) — an already fully
/// migrated family is the only case with nothing left to protect.
///
/// Not exhaustive — a `-shm` left behind by an ungraceful exit reads as
/// "live" too — but a false positive only defers this one-time migration to
/// a later `export`; it never loses or corrupts data.
///
/// A stat failure other than "not found" (permission denied, transient I/O)
/// on the `-shm` check is treated as live rather than absent — this function
/// exists purely to avoid a data-losing race, so an inconclusive answer must
/// err toward skipping the fold, not toward proceeding with one it can no
/// longer rule out (mirrors [`is_real_file`]'s NotFound-vs-other-error split,
/// #1341).
fn sqlite_fold_would_race_a_live_db(name: &str, config_dir: &Path) -> bool {
    if !family_has_an_unmigrated_member(name, config_dir) {
        return false;
    }
    match std::fs::symlink_metadata(config_dir.join(format!("{name}-shm"))) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!(db = name, error = %e, "inherit: could not stat -shm sidecar, treating as live to be safe");
            true
        }
    }
}

/// Whether `config_dir`'s copy of the base `.sqlite` file should win the fold
/// over the durable store's copy — decided once per DB family so every member
/// (base + sidecars) gets the same answer. A missing store copy always
/// prefers the folder's; otherwise mtime decides, same as
/// [`capture_codex_auth`].
fn local_sqlite_db_is_newer(name: &str, state_dir: &Path, config_dir: &Path) -> bool {
    let target = state_dir.join(name);
    let link = config_dir.join(name);
    std::fs::symlink_metadata(&target).is_err() || src_is_newer(&link, &target)
}

/// Fold-then-link a single SQLite DB family member (the base file or one of
/// its [`SQLITE_SIDECAR_SUFFIXES`]) into the durable store. See
/// [`link_codex_sqlite_dbs`] for the contract. `prefer_local` is decided once
/// per family by [`local_sqlite_db_is_newer`], not per member.
fn link_durable_sqlite_member(
    name: &str,
    state_dir: &Path,
    config_dir: &Path,
    prefer_local: bool,
) -> anyhow::Result<()> {
    let target = state_dir.join(name);
    let link = config_dir.join(name);
    let link_type = match std::fs::symlink_metadata(&link) {
        Ok(md) => Some(md.file_type()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow::anyhow!("inspecting {}: {e}", link.display())),
    };
    match link_type {
        None => attach_store_atomic(&target, &link),
        Some(ft) if ft.is_symlink() => {
            if std::fs::read_link(&link).is_ok_and(|dest| dest == target) {
                return Ok(());
            }
            attach_store_atomic(&target, &link)
        }
        Some(ft) if ft.is_file() => {
            let target_exists = std::fs::symlink_metadata(&target).is_ok();
            if !target_exists || prefer_local {
                copy_owner_only(&link, &target)?;
            }
            attach_store_atomic(&target, &link)
        }
        Some(_) => {
            tracing::warn!(
                path = %link.display(),
                "inherit: skipping Codex SQLite state file — not a regular file or symlink"
            );
            Ok(())
        }
    }
}

/// Point `link` at `target` via a `symlink`-then-`rename` swap instead of
/// `remove_file` followed by a separate `symlink` — if the swap fails
/// partway, `link` is left exactly as it was (still holding the just-copied
/// data in the fold case) rather than removed with nothing yet in its place,
/// which the next run's mtime comparison could otherwise mistake for "no
/// local copy to consider" (silent-failure-hunter, #1420).
///
/// # Errors
/// Returns an error when the temporary symlink can't be created or the
/// rename fails.
fn attach_store_atomic(target: &Path, link: &Path) -> anyhow::Result<()> {
    let parent = link
        .parent()
        .with_context(|| format!("{} has no parent", link.display()))?;
    let file_name = link
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no file name", link.display()))?;
    let tmp = parent.join(format!(".{file_name}.tmp-symlink.{}", std::process::id()));
    attach_store(target, &tmp)?;
    if let Err(e) = std::fs::rename(&tmp, link) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e)
            .with_context(|| format!("swapping {} -> {}", link.display(), target.display()));
    }
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
        write(&cfg.join(CODEX_AUTH_FILE), r#"{"account":"from-folder"}"#);

        capture_codex_auth(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_AUTH_FILE)).unwrap(),
            r#"{"account":"from-folder"}"#
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
        write(&state.join(CODEX_AUTH_FILE), r#"{"account":"old-account"}"#);
        write(
            &cfg.join(CODEX_AUTH_FILE),
            r#"{"account":"re-logged-in-account"}"#,
        );
        let now = filetime::FileTime::now();
        filetime::set_file_mtime(
            cfg.join(CODEX_AUTH_FILE),
            filetime::FileTime::from_unix_time(now.unix_seconds() + 60, 0),
        )
        .unwrap();

        capture_codex_auth(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_AUTH_FILE)).unwrap(),
            r#"{"account":"re-logged-in-account"}"#,
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
        write(
            &cfg.join(CODEX_AUTH_FILE),
            r#"{"account":"stale-folder-account"}"#,
        );
        write(
            &state.join(CODEX_AUTH_FILE),
            r#"{"account":"current-account"}"#,
        );
        let now = filetime::FileTime::now();
        filetime::set_file_mtime(
            state.join(CODEX_AUTH_FILE),
            filetime::FileTime::from_unix_time(now.unix_seconds() + 60, 0),
        )
        .unwrap();

        capture_codex_auth(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_AUTH_FILE)).unwrap(),
            r#"{"account":"current-account"}"#,
            "an older folder copy must not roll back a newer store credential"
        );
    }

    /// #1451: `auth.json` has no artifact like SQLite's `-shm` sidecar to
    /// signal a write in progress — Codex's `save()`
    /// (`codex-rs/login/src/auth/storage.rs`, `rust-v0.148.0`) truncates and
    /// writes the file in place rather than temp-then-rename, so a
    /// concurrent read can observe a torn write. The hook simulates that
    /// race landing between the pre-read stat and the read itself; the
    /// capture must discard the read rather than persist a possibly-torn
    /// snapshot into the durable store.
    #[test]
    fn capture_codex_auth_skips_a_read_that_races_a_concurrent_modification() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&cfg.join(CODEX_AUTH_FILE), "before-refresh");

        capture_codex_auth_with_hook(&state, &cfg, || {
            write(&cfg.join(CODEX_AUTH_FILE), "mid-refresh-content");
            let now = filetime::FileTime::now();
            filetime::set_file_mtime(
                cfg.join(CODEX_AUTH_FILE),
                filetime::FileTime::from_unix_time(now.unix_seconds() + 60, 0),
            )
            .unwrap();
        })
        .unwrap();

        assert!(
            std::fs::symlink_metadata(state.join(CODEX_AUTH_FILE)).is_err(),
            "a capture that raced a concurrent write must not land in the durable store"
        );
    }

    /// A read a coarse-grained filesystem's mtime resolution can't
    /// distinguish from "no write happened" is still caught by the content
    /// check: invalid JSON despite a stable mtime must not be persisted into
    /// the durable store (security-audit, #1451 — mtime equality alone is a
    /// fail-open heuristic on a filesystem whose timestamp resolution is
    /// coarser than Codex's truncate-then-rewrite window).
    #[test]
    fn capture_codex_auth_rejects_non_json_content_despite_a_stable_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        // A torn/empty read that a coarse mtime tick failed to flag.
        write(&cfg.join(CODEX_AUTH_FILE), "");

        capture_codex_auth(&state, &cfg).unwrap();

        assert!(
            std::fs::symlink_metadata(state.join(CODEX_AUTH_FILE)).is_err(),
            "content that isn't valid JSON must not land in the durable store, \
             even with a mtime bracket that reported no change"
        );
    }

    /// An open failure on the file itself (simulated with a mode-0 auth.json
    /// — `is_real_file`'s directory-level stat still succeeds, so this
    /// exercises [`read_auth_stable`]'s own open error path specifically)
    /// must be treated as unsafe and skip the capture, never fall through to
    /// writing whatever bytes happened to be read — same conservative
    /// default as [`sqlite_fold_would_race_a_live_db`]'s stat-error
    /// handling.
    #[cfg(unix)]
    #[test]
    fn capture_codex_auth_treats_an_open_error_as_unsafe_not_stable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&cfg.join(CODEX_AUTH_FILE), r#"{"OPENAI_API_KEY":"sk-x"}"#);
        // Deny read permission on the file itself — `symlink_metadata` (used
        // by `is_real_file`) needs no permission on the file, only on its
        // parent directories, so this isolates `open()`'s own EACCES rather
        // than short-circuiting earlier.
        std::fs::set_permissions(
            cfg.join(CODEX_AUTH_FILE),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        let readable_anyway = std::fs::File::open(cfg.join(CODEX_AUTH_FILE)).is_ok();
        let result = capture_codex_auth(&state, &cfg);

        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_ok(),
            "an open error must be handled, not propagated: {result:?}"
        );
        assert!(
            std::fs::symlink_metadata(state.join(CODEX_AUTH_FILE)).is_err(),
            "nothing should land in the durable store when the read can't be verified stable"
        );
    }

    #[test]
    fn projects_and_history_names_are_stable() {
        assert_eq!(PathBuf::from(PROJECTS_DIR), PathBuf::from("projects"));
        assert_eq!(HISTORY_FILE, "history.jsonl");
    }

    /// First run: neither side has the DB yet, so each family member
    /// (base file + both WAL sidecars) becomes a dangling symlink into the
    /// state dir — ready for Codex's own `create_if_missing` to fill in.
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_creates_dangling_symlinks_on_first_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();

        link_codex_sqlite_dbs(&state, &cfg).unwrap();

        for name in CODEX_INHERITED_SQLITE_DBS {
            for member in [(*name).to_string()]
                .into_iter()
                .chain(SQLITE_SIDECAR_SUFFIXES.iter().map(|s| format!("{name}{s}")))
            {
                let link = cfg.join(&member);
                let md = std::fs::symlink_metadata(&link).unwrap();
                assert!(md.file_type().is_symlink(), "{member} must be a symlink");
                assert_eq!(std::fs::read_link(&link).unwrap(), state.join(&member));
                assert!(
                    std::fs::symlink_metadata(state.join(&member)).is_err(),
                    "{member}'s target should not exist yet on a first run"
                );
            }
        }
    }

    /// A pre-existing real `goals_1.sqlite` (created before this link
    /// existed) is folded into the state dir before being replaced by the
    /// symlink — its content must survive.
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_folds_existing_real_file_then_links() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&cfg.join(CODEX_GOALS_DB), "pre-existing-goals-content");

        link_codex_sqlite_dbs(&state, &cfg).unwrap();

        let link = cfg.join(CODEX_GOALS_DB);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_GOALS_DB)).unwrap(),
            "pre-existing-goals-content",
            "pre-existing DB content must survive the fold into the durable store"
        );
    }

    /// A pre-existing real file with a live `-shm` sidecar looks like a Codex
    /// process currently has it open in WAL mode — the fold must be skipped
    /// entirely (base + both sidecars left untouched) rather than risk a torn
    /// copy or stranding the running process's writes on an orphaned inode
    /// after the swap (#1448).
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_skips_fold_when_shm_sidecar_indicates_a_live_connection() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&cfg.join(CODEX_GOALS_DB), "live-goals-content");
        write(&cfg.join(format!("{CODEX_GOALS_DB}-shm")), "shm");

        link_codex_sqlite_dbs(&state, &cfg).unwrap();

        assert!(
            std::fs::symlink_metadata(cfg.join(CODEX_GOALS_DB))
                .unwrap()
                .file_type()
                .is_file(),
            "a live DB must not be folded into a symlink this pass"
        );
        assert_eq!(
            std::fs::read_to_string(cfg.join(CODEX_GOALS_DB)).unwrap(),
            "live-goals-content",
            "the folder's copy must be left exactly as it was"
        );
        assert!(
            std::fs::symlink_metadata(state.join(CODEX_GOALS_DB)).is_err(),
            "nothing should land in the durable store while the DB looks live"
        );
    }

    /// #1450: a prior `link_codex_sqlite_dbs` run can partially fail after
    /// symlinking the base file but before symlinking a WAL sidecar (e.g.
    /// `attach_store_atomic` erroring on the sidecar's rename after the
    /// base's succeeded). The base then reads as "already migrated," but a
    /// sidecar left real and live must still block the fold — not just the
    /// base-file-is-still-real case #1448 covered.
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_skips_a_sidecar_left_real_after_a_partial_prior_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();

        // Base file already fully migrated by a prior successful pass.
        write(&state.join(CODEX_GOALS_DB), "goals-content");
        std::os::unix::fs::symlink(state.join(CODEX_GOALS_DB), cfg.join(CODEX_GOALS_DB)).unwrap();

        // The `-wal` sidecar was left real by that prior run's partial
        // failure, and Codex still has it open (`-shm` present).
        let wal = cfg.join(format!("{CODEX_GOALS_DB}-wal"));
        write(&wal, "live-wal-content");
        write(&cfg.join(format!("{CODEX_GOALS_DB}-shm")), "shm");

        link_codex_sqlite_dbs(&state, &cfg).unwrap();

        assert!(
            std::fs::symlink_metadata(&wal)
                .unwrap()
                .file_type()
                .is_file(),
            "a sidecar left real by a partial prior failure must not be folded while it looks live"
        );
        assert_eq!(
            std::fs::read_to_string(&wal).unwrap(),
            "live-wal-content",
            "the folder's copy must be left exactly as it was"
        );
        assert!(
            std::fs::symlink_metadata(state.join(format!("{CODEX_GOALS_DB}-wal"))).is_err(),
            "nothing should land in the durable store for the sidecar while the DB looks live"
        );
    }

    /// A stat failure other than "not found" (simulated here with an
    /// unreadable config dir) must be treated as live and skip the fold,
    /// never as absent — an inconclusive answer from the liveness check must
    /// never silently fall through to an unguarded fold (#1448).
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_treats_a_stat_error_as_live_not_absent() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();
        write(&cfg.join(CODEX_GOALS_DB), "goals-content");
        // Deny search permission on `cfg` so stat on its children fails with
        // EACCES rather than NotFound.
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o000)).unwrap();

        let readable_anyway = std::fs::symlink_metadata(cfg.join(CODEX_GOALS_DB)).is_ok();
        let result = link_codex_sqlite_dbs(&state, &cfg);

        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_ok(),
            "a stat error must be handled, not propagated: {result:?}"
        );
        assert!(
            std::fs::symlink_metadata(cfg.join(CODEX_GOALS_DB))
                .unwrap()
                .file_type()
                .is_file(),
            "an unstattable DB must not be folded into a symlink"
        );
        assert!(
            std::fs::symlink_metadata(state.join(CODEX_GOALS_DB)).is_err(),
            "nothing should land in the durable store when liveness can't be determined"
        );
    }

    /// Once the `-shm` sidecar is gone (the live connection closed), the next
    /// `export` retries the fold and it succeeds normally.
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_folds_on_a_later_pass_once_the_shm_sidecar_is_gone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&cfg.join(CODEX_GOALS_DB), "goals-content");
        let shm = cfg.join(format!("{CODEX_GOALS_DB}-shm"));
        write(&shm, "shm");

        link_codex_sqlite_dbs(&state, &cfg).unwrap();
        std::fs::remove_file(&shm).unwrap();
        link_codex_sqlite_dbs(&state, &cfg).unwrap();

        assert!(
            std::fs::symlink_metadata(cfg.join(CODEX_GOALS_DB))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the fold must succeed once the DB no longer looks live"
        );
        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_GOALS_DB)).unwrap(),
            "goals-content"
        );
    }

    /// Idempotent: a correct symlink is left alone, and re-running does not
    /// error even though the target now exists.
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&cfg).unwrap();

        link_codex_sqlite_dbs(&state, &cfg).unwrap();
        write(&state.join(CODEX_QUEUE_DB), "queued-message");
        link_codex_sqlite_dbs(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_link(cfg.join(CODEX_QUEUE_DB)).unwrap(),
            state.join(CODEX_QUEUE_DB)
        );
        assert_eq!(
            std::fs::read_to_string(cfg.join(CODEX_QUEUE_DB)).unwrap(),
            "queued-message",
            "the symlink must transparently read the store's content"
        );
    }

    /// A folder's real file that is *newer* than the store's copy wins the
    /// fold — mirrors [`codex_auth_capture_replaces_the_store_when_the_folder_copy_is_newer`],
    /// since Codex is the sole writer to either copy.
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_newer_folder_copy_wins_the_fold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        write(&state.join(CODEX_MEMORIES_DB), "stale-store-memories");
        write(&cfg.join(CODEX_MEMORIES_DB), "fresh-folder-memories");
        let now = filetime::FileTime::now();
        filetime::set_file_mtime(
            cfg.join(CODEX_MEMORIES_DB),
            filetime::FileTime::from_unix_time(now.unix_seconds() + 60, 0),
        )
        .unwrap();

        link_codex_sqlite_dbs(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_MEMORIES_DB)).unwrap(),
            "fresh-folder-memories",
            "a newer folder copy must replace the store's stale one"
        );
    }

    /// The fold decision is made once per DB family, from the base file's
    /// mtime, and applied to every sidecar — never re-decided per member.
    /// Here the base file is *older* locally (store should win) but the
    /// `-wal` sidecar is *newer* locally in isolation; the sidecar must still
    /// follow the base file's decision and keep the store's copy
    /// (security-audit, #1420).
    #[cfg(unix)]
    #[test]
    fn link_codex_sqlite_dbs_sidecar_follows_the_base_files_fold_decision() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        let wal_name = format!("{CODEX_QUEUE_DB}-wal");

        write(&state.join(CODEX_QUEUE_DB), "store-base");
        write(&cfg.join(CODEX_QUEUE_DB), "folder-base");
        write(&state.join(&wal_name), "store-wal");
        write(&cfg.join(&wal_name), "folder-wal");
        let now = filetime::FileTime::now();
        // Base file: folder's copy is older, so the store should win.
        filetime::set_file_mtime(
            cfg.join(CODEX_QUEUE_DB),
            filetime::FileTime::from_unix_time(now.unix_seconds() - 60, 0),
        )
        .unwrap();
        // WAL sidecar: folder's copy is newer in isolation — must NOT be
        // allowed to win independently of the base file's decision.
        filetime::set_file_mtime(
            cfg.join(&wal_name),
            filetime::FileTime::from_unix_time(now.unix_seconds() + 60, 0),
        )
        .unwrap();

        link_codex_sqlite_dbs(&state, &cfg).unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join(CODEX_QUEUE_DB)).unwrap(),
            "store-base",
            "an older base file must not win the fold"
        );
        assert_eq!(
            std::fs::read_to_string(state.join(&wal_name)).unwrap(),
            "store-wal",
            "the WAL sidecar must follow the base file's fold decision, not its own newer mtime"
        );
    }

    /// A failure partway through the symlink swap must leave `link` exactly
    /// as it was — never removed with nothing yet in its place — so a folded
    /// copy that already landed in the store isn't mistaken for absent on the
    /// next run (silent-failure-hunter, #1420).
    #[cfg(unix)]
    #[test]
    fn attach_store_atomic_leaves_link_untouched_when_the_rename_target_is_a_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let cfg = tmp.path().join("codex-hash");
        std::fs::create_dir_all(&state).unwrap();
        write(&state.join(CODEX_GOALS_DB), "already-folded");
        // A directory at `link` can never be renamed over (EISDIR), forcing
        // attach_store_atomic's swap to fail after the temp symlink is made.
        std::fs::create_dir_all(cfg.join(CODEX_GOALS_DB)).unwrap();

        let target = state.join(CODEX_GOALS_DB);
        let link = cfg.join(CODEX_GOALS_DB);
        assert!(attach_store_atomic(&target, &link).is_err());

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_dir(),
            "a failed swap must leave the original entry at `link` untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "already-folded",
            "the store's copy must be unaffected by a failed swap"
        );
    }

    #[test]
    fn codex_sqlite_db_names_are_stable() {
        assert_eq!(CODEX_GOALS_DB, "goals_1.sqlite");
        assert_eq!(CODEX_MEMORIES_DB, "memories_1.sqlite");
        assert_eq!(CODEX_QUEUE_DB, "queue_1.sqlite");
        assert_eq!(SQLITE_SIDECAR_SUFFIXES, ["-wal", "-shm"]);
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
