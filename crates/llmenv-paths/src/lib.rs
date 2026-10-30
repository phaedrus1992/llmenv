//! XDG paths and path helpers.

use std::env::VarError;
use std::path::{Path, PathBuf};

mod atomic_write;
mod bounded_read;
mod names;
mod owner_only;
mod tilde;
mod unsynced_write;
mod write_stats;
pub use tilde::expand_tilde;
use tilde::expand_tilde_with_env;
pub use write_stats::{WriteStats, snapshot as write_stats_snapshot};
pub mod program;
pub use atomic_write::{resolve_user_link, write_owner_only_atomic, write_user_file_atomic};
pub use bounded_read::{check_regular_file, read_file_bounded, read_open_file, read_text_bounded};
pub use names::{
    cwd_under_prefix, has_parent_component, is_unsafe_join_target, is_valid_short_name,
};
pub use owner_only::{
    create_dir_owner_only, is_executable, open_append_owner_only, write_owner_only,
    write_owner_only_executable, write_owner_only_like,
};
pub use unsynced_write::write_owner_only_atomic_unsynced;

/// `read_dir` that treats a missing directory as "nothing to iterate":
/// returns `Ok(None)` on `NotFound` but propagates every other I/O error (e.g.
/// a permission denial), with `reading <dir>` context. Use instead of an
/// `exists()`-then-`read_dir` guard, which collapses *all* stat failures —
/// including `EACCES` — to "absent" and so silently skips a directory the
/// caller can't read (#918).
///
/// # Errors
/// Returns any `read_dir` error other than `NotFound` (e.g. permission denied,
/// or the path is not a directory).
pub fn read_dir_optional(dir: &Path) -> anyhow::Result<Option<std::fs::ReadDir>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", dir.display()))),
    }
}

pub fn config_dir() -> anyhow::Result<PathBuf> {
    ensure_utf8_override("LLMENV_CONFIG_DIR")?;
    config_dir_with_env(&|name| std::env::var(name).ok())
}

/// Fails when `name` is set to a value that is not valid UTF-8. The env closures read such a value
/// as unset, so the default path would be used with no message (#2607).
fn ensure_utf8(name: &str, value: Option<&std::ffi::OsStr>) -> anyhow::Result<()> {
    match value.map(std::ffi::OsStr::to_str) {
        Some(None) => {
            anyhow::bail!("{name} is not valid UTF-8; set {name} to a UTF-8 value and retry")
        }
        _ => Ok(()),
    }
}

/// Checks the override `name`, and `HOME` when the resolved path reads it (#2607).
fn ensure_utf8_override(name: &str) -> anyhow::Result<()> {
    let value = std::env::var_os(name);
    ensure_utf8(name, value.as_deref())?;
    if home_is_read(value.as_deref()) {
        ensure_utf8("HOME", std::env::var_os("HOME").as_deref())?;
    }
    Ok(())
}

/// Whether the resolved path reads `HOME`: when the override is unset or empty, or when it is a
/// `~` path that is expanded. An absolute override never reads it.
fn home_is_read(override_value: Option<&std::ffi::OsStr>) -> bool {
    match override_value.filter(|v| !v.is_empty()) {
        None => true,
        Some(value) => value.to_str().is_some_and(|s| s.starts_with('~')),
    }
}

/// Adapts an Option-based env provider to the Result-based one that
/// [`expand_tilde_with_env`] takes. A non-UTF-8 `HOME` never reaches this adapter in
/// production, because [`ensure_utf8_override`] rejects it first.
fn present_only(
    get_env: &impl Fn(&str) -> Option<String>,
) -> impl Fn(&str) -> Result<String, VarError> + '_ {
    move |name| get_env(name).ok_or(VarError::NotPresent)
}

/// [`config_dir`] with an injectable env-var provider so tests can exercise
/// the set-but-empty override case without mutating real process env vars.
fn config_dir_with_env(get_env: &impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    // A set-but-empty override (`LLMENV_CONFIG_DIR=`) must fall through to the
    // `$HOME` default rather than resolving to a relative `PathBuf::from("")` (#1111).
    if let Some(dir) = get_env("LLMENV_CONFIG_DIR").filter(|d| !d.is_empty()) {
        // Same bug class as #1910: a tilde-prefixed override is a directory
        // path, not a literal string, and must be expanded like any other.
        Ok(PathBuf::from(expand_tilde_with_env(
            &dir,
            &present_only(get_env),
        )?))
    } else {
        // A set-but-empty `HOME` has the identical failure mode as the override
        // above (a relative `PathBuf::from("")`-derived path) — treat it as
        // unset too rather than fixing only the override branch.
        let home = get_env("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow::anyhow!("environment variable not found: HOME"))?;
        Ok(PathBuf::from(home).join(".config/llmenv"))
    }
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("config.yaml"))
}

pub fn state_dir() -> anyhow::Result<PathBuf> {
    ensure_utf8_override("LLMENV_STATE_DIR")?;
    state_dir_with_env(&|name| std::env::var(name).ok())
}

/// [`state_dir`] with an injectable env-var provider so tests can exercise
/// the set-but-empty override case without mutating real process env vars.
fn state_dir_with_env(get_env: &impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    // A set-but-empty override (`LLMENV_STATE_DIR=`) must fall through to the
    // `$HOME` default rather than resolving to a relative `PathBuf::from("")` (#1111).
    if let Some(dir) = get_env("LLMENV_STATE_DIR").filter(|d| !d.is_empty()) {
        // Same bug class as #1910: a tilde-prefixed override is a directory
        // path, not a literal string, and must be expanded like any other.
        Ok(PathBuf::from(expand_tilde_with_env(
            &dir,
            &present_only(get_env),
        )?))
    } else {
        // Same reasoning as `config_dir_with_env`: a set-but-empty `HOME` must
        // also be treated as unset, not just the override.
        let home = get_env("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow::anyhow!("environment variable not found: HOME"))?;
        Ok(PathBuf::from(home).join(".local/state/llmenv"))
    }
}

/// # Errors
/// Propagates the read, copy, permission, or rename failure. Removes the
/// temp file on error.
pub fn copy_replacing_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::io::Seek as _;

    let parent = dst.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", dst.display()),
        )
    })?;
    let file_name = dst.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no file name: {}", dst.display()),
        )
    })?;
    let mut src_file = std::fs::File::open(src)?;
    // Read the mode from the already-open fd, not a second path-based stat.
    #[cfg(unix)]
    let src_mode = {
        use std::os::unix::fs::PermissionsExt;
        src_file.metadata()?.permissions().mode()
    };
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..COPY_REPLACING_SYMLINK_MAX_RETRIES {
        let counter = COPY_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".{pid}.{nanos}.{counter}.tmp"));
        let tmp_path = parent.join(&tmp_name);

        let mut tmp_file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };

        // A retried attempt reuses `src_file`, whose read cursor is at EOF
        // from the prior try — rewind before streaming again.
        if attempt > 0 {
            src_file.seek(std::io::SeekFrom::Start(0))?;
        }
        let copy_result = (|| -> std::io::Result<()> {
            std::io::copy(&mut src_file, &mut tmp_file)?;
            tmp_file.sync_all()?;
            // Set permissions on the held fd, not by path — immune to a
            // symlink swapped in at `tmp_path` after this point.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tmp_file.set_permissions(std::fs::Permissions::from_mode(src_mode & 0o755))?;
            }
            Ok(())
        })();
        drop(tmp_file);
        if let Err(e) = copy_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp_path, dst) {
            if let Err(cleanup_err) = std::fs::remove_file(&tmp_path) {
                tracing::warn!(
                    "copy_replacing_symlink: failed to remove temp file {} after failed \
                     rename (rename error: {e}): {cleanup_err}",
                    tmp_path.display()
                );
            }
            return Err(e);
        }
        return Ok(());
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted temp-file collision retries",
        )
    }))
}

/// Fail if `path` exists and isn't a regular file — in particular, a
/// symlink, which `symlink_metadata` (never following it) distinguishes from
/// a plain file that `metadata` alone would not.
///
/// Use before opening a path for appending, where an existing symlink can't
/// simply be replaced the way [`write_owner_only`]'s unlink-then-recreate
/// does — that would destroy the log/append history the caller is trying to
/// preserve. This is a check-then-use guard, not a TOCTOU-proof one: the same
/// residual, same-user-race risk `src/paths/dirfd.rs` documents for the
/// fd-relative primitives in the `llmenv` crate applies here too (winning it
/// requires already being the user who owns the files) (#1431).
///
/// # Errors
/// Returns an error if `path` exists and is not a regular file (a symlink,
/// FIFO, device, etc.), or if the path can't be stat'd for a reason other
/// than not existing.
pub fn reject_non_regular_file(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not a regular file ({:?}); refusing to write through it",
                path.display(),
                meta.file_type()
            ),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// [`resolve_on_path`] against an explicit `PATH` value.
///
/// Split out so the lookup is testable without mutating the process
/// environment: `std::env::set_var` is `unsafe` as of Rust 2024 and this
/// workspace sets `unsafe_code = "forbid"`, so a test cannot legally point the
/// real `PATH` somewhere else. Callers with a `PATH` value in hand (rather than
/// the process's own) use it directly.
#[must_use]
pub fn resolve_in_path_list(name: &str, path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.chars().any(char::is_whitespace) {
        return None;
    }
    std::env::split_paths(path_var)
        // Absolute entries only (#1400). A shell resolves every relative entry
        // against the working directory — a literal empty one (`PATH="$UNSET:…"`
        // leaves one behind), and equally an explicit `.` or `bin`. Resolving a
        // binary out of the working directory is a hijack vector, and nothing
        // llmenv looks up — `claude`, `crush`, `opencode`, `mcp-proxy`, `uvx`,
        // `icm` — legitimately lives at a relative path. Skipping only the empty
        // spelling, as this did until #1400, left `PATH=".:/usr/local/bin"`
        // resolving `./claude` in preference to the installed one.
        //
        // `is_absolute` covers the empty entry too: `Path::new("").is_absolute()`
        // is false.
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// Returns `true` when `name` resolves to an executable on the current `PATH`.
///
/// Walks `PATH` directly rather than shelling out to `which` (#1382). The
/// subprocess returned the same `false` for "the binary isn't installed" and
/// "`which` itself couldn't be run", so on an image without `which` — routine
/// for distroless and minimal containers — an installed engine looked missing.
/// That was harmless while every caller was advisory, but `run_launch` turns a
/// negative result into a hard error, which made the ambiguity user-visible as
/// a false "not installed". Resolving `PATH` in-process has no such failure
/// mode, and skips a fork+exec on a hot path.
///
/// Names containing `/` or ASCII whitespace are unconditionally rejected;
/// they cannot be plain binary names.
///
/// Lives here rather than in `llmenv`'s adapter module because it is a plain
/// path helper with two unrelated callers — adapter/engine detection and
/// `mcp-proxy` lookup. Each kept its own copy until #1390, and they diverged:
/// the proxy's never got the empty-`PATH`-entry guard below.
#[must_use]
pub fn binary_on_path(name: &str) -> bool {
    resolve_on_path(name).is_some()
}

/// The full path `name` resolves to on `PATH`, or `None` when it isn't there.
///
/// Callers that only need a yes/no answer should use [`binary_on_path`]; this
/// exists for the ones that need the location itself (claude-code's
/// install-method detection inspects the resolved path).
#[must_use]
pub fn resolve_on_path(name: &str) -> Option<PathBuf> {
    let Some(path_var) = std::env::var_os("PATH") else {
        // Distinguished from "PATH is set but has no match" only in the log:
        // both mean the binary is unusable, but an absent PATH points at a
        // stripped environment rather than a missing install.
        tracing::debug!("PATH is unset; cannot resolve '{name}'");
        return None;
    };
    resolve_in_path_list(name, &path_var)
}

/// Copy `src` to `dst`, replacing whatever is at `dst` — including a symlink
/// — rather than writing through it.
///
/// A plain `std::fs::copy(src, dst)` opens `dst` with `O_TRUNC`, which
/// follows a symlink there and truncates/overwrites its target instead of
/// replacing the link itself — the same class of gap `write_owner_only_atomic`
/// closes for in-memory content, applied here to copying from another file.
///
/// The temp file is opened with `create_new` (`O_EXCL`, never follows a
/// final-component symlink and fails if the name is already taken) and
/// written to **through that same file handle** — never a second path-based
/// open, which `std::fs::copy(src, &tmp_path)` would be. An `O_EXCL` open
/// followed by a second, non-exclusive open of the same path is exactly the
/// TOCTOU window this function exists to close: a planted or raced symlink
/// between those two opens would still get written through on the second one
/// (security-audit, #1426). Only after the content and permissions are fully
/// written does `rename` move the temp file over `dst`, which POSIX
/// guarantees replaces the destination directory entry atomically without
/// following it — and does not follow a symlink at the *source* of the
/// rename either, so a swapped `tmp_path` still moves as an entry, never as
/// its target's content.
///
/// Preserves `src`'s permission mode with group/other write and setuid/setgid
/// masked off (`& 0o755 & !0o022`, i.e. `& 0o755`) — full mode propagation
/// would let a bundle file sourced from a permissive local path or tarball
/// land group/world-writable inside a directory the agent can execute code
/// from. Every other file llmenv writes into that directory goes through
/// [`write_owner_only`] (0o600); this is deliberately looser only to let a
/// legitimately executable bundle asset (a hook script) keep its exec bit,
/// never group/other write or setuid/setgid.
///
/// Retried on a temp-name collision (`AlreadyExists`) up to this many times,
/// mirroring [`write_owner_only_atomic_in_dir`]'s retry bound — the
/// process-local counter below makes a same-process collision unlikely but
/// not provably impossible (`SystemTime` granularity varies by platform), and
/// the bound also covers a stale leftover from a prior crashed process at the
/// same pid+nanos slice.
const COPY_REPLACING_SYMLINK_MAX_RETRIES: u32 = 8;

/// Process-local counter disambiguating [`copy_replacing_symlink`]'s temp
/// filenames when multiple calls land in the same nanosecond — shares the
/// reasoning [`TMP_COUNTER`] documents for [`write_owner_only_atomic`], kept
/// separate since the two functions' temp-file lifetimes don't overlap.
static COPY_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// True when `path` is a regular file carrying an execute bit — what a shell's
/// `PATH` search accepts.
///
/// `metadata` follows symlinks, so a symlinked binary resolves the way a shell
/// would. Any I/O error (a `PATH` entry that doesn't exist, or one the user
/// can't stat) simply means "not usable here", which is the same conclusion a
/// shell reaches.
fn is_executable_file(path: &Path) -> bool {
    let md = match std::fs::metadata(path) {
        Ok(md) => md,
        Err(e) => {
            // A missing entry is the overwhelmingly common case and not worth
            // reporting. Anything else (an unreadable directory, a stalled
            // network mount, fd exhaustion) also resolves to "not usable
            // here", but it can make an installed binary look absent — so
            // leave a breadcrumb rather than discarding it entirely.
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("PATH probe could not stat {}: {e}", path.display());
            }
            return false;
        }
    };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        // Fail closed. There is no shipped non-unix target (release builds are
        // linux-musl and apple-darwin), and treating every regular file as
        // executable would be weaker than the `which` call this replaced.
        false
    }
}

/// Makes an open refuse a symlink at the final path component (#2599).
/// A name check before the open is not enough: a symlink swapped in after the check is followed,
/// and the write then lands on the symlink's target.
#[cfg(unix)]
pub fn refuse_symlink(opts: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt as _;
    opts.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
}

/// Non-unix targets have no `O_NOFOLLOW`; the name check remains the only guard there.
#[cfg(not(unix))]
pub fn refuse_symlink(opts: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    opts
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ensure_utf8_rejects_a_value_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(&[0xff, 0xfe]);
        let err = ensure_utf8("LLMENV_STATE_DIR", Some(raw)).unwrap_err();
        assert!(
            err.to_string()
                .contains("LLMENV_STATE_DIR is not valid UTF-8"),
            "{err}"
        );
    }

    #[test]
    fn home_is_read_only_when_the_override_does_not_resolve_the_path() {
        use std::ffi::OsStr;
        assert!(home_is_read(None));
        assert!(home_is_read(Some(OsStr::new(""))));
        assert!(home_is_read(Some(OsStr::new("~/llmenv"))));
        assert!(!home_is_read(Some(OsStr::new("/srv/llmenv"))));
    }

    #[test]
    fn ensure_utf8_accepts_unset_and_utf8_values() {
        assert!(ensure_utf8("HOME", None).is_ok());
        assert!(ensure_utf8("HOME", Some(std::ffi::OsStr::new("/home/u"))).is_ok());
    }

    #[test]
    fn state_dir_with_env_empty_override_falls_through_to_home() {
        let get_env = |name: &str| match name {
            "LLMENV_STATE_DIR" => Some(String::new()),
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        let result = state_dir_with_env(&get_env).unwrap();
        assert_eq!(result, PathBuf::from("/home/testuser/.local/state/llmenv"));
    }

    #[test]
    fn state_dir_with_env_empty_equals_unset() {
        let unset = |name: &str| match name {
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        let empty = |name: &str| match name {
            "LLMENV_STATE_DIR" => Some(String::new()),
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        assert_eq!(
            state_dir_with_env(&unset).unwrap(),
            state_dir_with_env(&empty).unwrap()
        );
    }

    #[test]
    fn state_dir_with_env_non_empty_override_used_verbatim() {
        let get_env = |name: &str| match name {
            "LLMENV_STATE_DIR" => Some("/custom/state".to_string()),
            _ => None,
        };
        assert_eq!(
            state_dir_with_env(&get_env).unwrap(),
            PathBuf::from("/custom/state")
        );
    }

    #[test]
    fn state_dir_with_env_tilde_override_is_expanded() {
        // Same bug class as #1910's Config::load fix: LLMENV_STATE_DIR=~/foo
        // must not be treated as a literal directory named "~".
        let get_env = |name: &str| match name {
            "LLMENV_STATE_DIR" => Some("~/custom-state".to_string()),
            "HOME" => Some("/home/user".to_string()),
            _ => None,
        };
        assert_eq!(
            state_dir_with_env(&get_env).unwrap(),
            PathBuf::from("/home/user/custom-state")
        );
    }

    #[test]
    fn state_dir_with_env_empty_home_errors_instead_of_relative_path() {
        let get_env = |name: &str| match name {
            "HOME" => Some(String::new()),
            _ => None,
        };
        let result = state_dir_with_env(&get_env);
        assert!(
            result.is_err(),
            "a set-but-empty HOME must error, not resolve to a relative path: {result:?}"
        );
    }

    #[test]
    fn config_dir_with_env_empty_override_falls_through_to_home() {
        let get_env = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some(String::new()),
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        let result = config_dir_with_env(&get_env).unwrap();
        assert_eq!(result, PathBuf::from("/home/testuser/.config/llmenv"));
    }

    #[test]
    fn config_dir_with_env_empty_equals_unset() {
        let unset = |name: &str| match name {
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        let empty = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some(String::new()),
            "HOME" => Some("/home/testuser".to_string()),
            _ => None,
        };
        assert_eq!(
            config_dir_with_env(&unset).unwrap(),
            config_dir_with_env(&empty).unwrap()
        );
    }

    #[test]
    fn config_dir_with_env_non_empty_override_used_verbatim() {
        let get_env = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some("/custom/config".to_string()),
            _ => None,
        };
        assert_eq!(
            config_dir_with_env(&get_env).unwrap(),
            PathBuf::from("/custom/config")
        );
    }

    #[test]
    fn config_dir_with_env_tilde_override_is_expanded() {
        // Same bug class as #1910's Config::load fix: LLMENV_CONFIG_DIR=~/foo
        // must not be treated as a literal directory named "~".
        let get_env = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some("~/custom-config".to_string()),
            "HOME" => Some("/home/user".to_string()),
            _ => None,
        };
        assert_eq!(
            config_dir_with_env(&get_env).unwrap(),
            PathBuf::from("/home/user/custom-config")
        );
    }

    #[test]
    fn config_dir_with_env_empty_home_errors_instead_of_relative_path() {
        let get_env = |name: &str| match name {
            "HOME" => Some(String::new()),
            _ => None,
        };
        let result = config_dir_with_env(&get_env);
        assert!(
            result.is_err(),
            "a set-but-empty HOME must error, not resolve to a relative path: {result:?}"
        );
    }

    #[test]
    fn read_dir_optional_returns_none_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            read_dir_optional(&tmp.path().join("nope"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn read_dir_optional_returns_some_for_present_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_dir_optional(tmp.path()).unwrap().is_some());
    }

    // #918: a non-NotFound I/O error (EACCES) propagates rather than being
    // masked as an absent directory the way an exists() stat would.
    #[cfg(unix)]
    #[test]
    fn read_dir_optional_propagates_permission_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = read_dir_optional(&child);
        let readable_anyway = std::fs::read_dir(&child).is_ok();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "permission error must propagate, got {result:?}"
        );
    }

    #[test]
    fn is_valid_short_name_accepts_alphanumeric_dot_underscore_dash() {
        for name in ["superpowers", "context-mode", "v1.2.3", "foo_bar", "a"] {
            assert!(is_valid_short_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn is_valid_short_name_rejects_empty_dot_dotdot_and_leading_dash() {
        for name in ["", ".", "..", "-evil"] {
            assert!(!is_valid_short_name(name), "{name} should be rejected");
        }
    }

    #[test]
    fn is_valid_short_name_rejects_path_separator() {
        for name in ["foo/bar", "foo\\bar"] {
            assert!(!is_valid_short_name(name), "{name} should be rejected");
        }
    }

    #[test]
    fn is_valid_short_name_rejects_control_and_non_ascii_characters() {
        // #534: a blocklist-style check misses Unicode formatting characters
        // (zero-width space, RTL override) that an allowlist closes by construction.
        for name in ["foo\0bar", "foo\u{200B}bar", "foo\u{202E}bar", "café"] {
            assert!(!is_valid_short_name(name), "{name} should be rejected");
        }
    }

    proptest::proptest! {
        #[test]
        fn prop_is_valid_short_name_no_panic(s in ".*") {
            let _ = is_valid_short_name(&s);
        }

        #[test]
        fn prop_valid_names_are_ascii_alphanumeric_subset(
            name in "[a-zA-Z][a-zA-Z0-9._-]{0,30}",
        ) {
            if name != "." && name != ".." && !name.starts_with('-') {
                proptest::prop_assert!(is_valid_short_name(&name));
            }
        }

        #[test]
        fn prop_non_ascii_always_rejected(s in "[^\x00-\x7F]+") {
            proptest::prop_assert!(!is_valid_short_name(&s));
        }

        #[test]
        fn prop_valid_short_name_is_never_an_unsafe_join_target(
            name in "[a-zA-Z][a-zA-Z0-9._-]{0,30}",
        ) {
            if is_valid_short_name(&name) {
                proptest::prop_assert!(!is_unsafe_join_target(&name));
            }
        }
    }

    #[test]
    fn cwd_under_prefix_respects_component_boundary() {
        assert!(cwd_under_prefix("/home/alice/git/x", "/home/alice/git/x"));
        assert!(cwd_under_prefix(
            "/home/alice/git/x/sub",
            "/home/alice/git/x"
        ));
        assert!(!cwd_under_prefix(
            "/home/alice/git/xyz",
            "/home/alice/git/x"
        ));
        assert!(!cwd_under_prefix("/home/alice", "/home/alice/git"));
    }

    #[test]
    fn has_parent_component_detects_traversal_substring_misses() {
        // Trailing `..` with no slash — substring check for "../" misses this.
        assert!(has_parent_component("foo/.."));
        assert!(has_parent_component(".."));
        assert!(has_parent_component("/foo/../bar"));
        assert!(has_parent_component("a/b/../c"));
    }

    #[test]
    fn has_parent_component_allows_safe_paths() {
        assert!(!has_parent_component("/home/alice/.cache/llmenv"));
        assert!(!has_parent_component("relative/path"));
        assert!(!has_parent_component("~/.cache/llmenv"));
        // A `..` embedded in a name is not a parent component.
        assert!(!has_parent_component("/foo/..bar/baz"));
        assert!(!has_parent_component("file..txt"));
        assert!(!has_parent_component(""));
    }

    #[test]
    fn has_parent_component_does_not_check_absolute_paths() {
        // Documents that has_parent_component alone is INSUFFICIENT for
        // safe-join validation. Callers must use is_unsafe_join_target.
        assert!(!has_parent_component("/etc/passwd"));
        assert!(!has_parent_component("/abs/secret"));
    }

    #[test]
    fn is_unsafe_join_target_rejects_traversal_and_absolute() {
        // Parent components — same as has_parent_component.
        assert!(is_unsafe_join_target(".."));
        assert!(is_unsafe_join_target("foo/.."));
        assert!(is_unsafe_join_target("a/b/../c"));
        // Absolute paths — would escape via Path::join semantics.
        assert!(is_unsafe_join_target("/etc/passwd"));
        assert!(is_unsafe_join_target("/abs"));
        // Safe: plain relative paths.
        assert!(!is_unsafe_join_target("rel/path"));
        assert!(!is_unsafe_join_target("file.txt"));
        assert!(!is_unsafe_join_target("a/b/c"));
        // Embedded `..` in a name is not a parent component.
        assert!(!is_unsafe_join_target("file..txt"));
    }

    #[test]
    fn tilde_passthrough_for_absolute_and_relative() {
        // Tests the non-HOME-dependent branches.
        assert_eq!(
            expand_tilde("/abs/path").expect("no HOME read"),
            "/abs/path"
        );
        assert_eq!(expand_tilde("rel/path").expect("no HOME read"), "rel/path");
        assert_eq!(expand_tilde("").expect("no HOME read"), "");
    }

    #[test]
    fn expand_tilde_with_env_unset_or_empty_home_is_an_error_naming_home() {
        let unset = |_: &str| Err(VarError::NotPresent);
        let empty = |name: &str| match name {
            "HOME" => Ok(String::new()),
            _ => Err(VarError::NotPresent),
        };
        for get_env in [&unset as &dyn Fn(&str) -> Result<String, VarError>, &empty] {
            for input in ["~", "~/foo"] {
                let err = expand_tilde_with_env(input, &get_env)
                    .expect_err("a ~ path with no HOME must fail");
                assert!(
                    err.to_string().contains("HOME is not set"),
                    "{input}: {err}"
                );
            }
        }
    }

    #[test]
    fn config_and_state_dir_tilde_overrides_fail_without_home() {
        let override_only = |name: &str| match name {
            "LLMENV_CONFIG_DIR" => Some("~/cfg".to_string()),
            "LLMENV_STATE_DIR" => Some("~/state".to_string()),
            _ => None,
        };
        let config_err = config_dir_with_env(&override_only).expect_err("no HOME for ~/cfg");
        assert!(
            config_err.to_string().contains("HOME is not set"),
            "{config_err}"
        );
        let state_err = state_dir_with_env(&override_only).expect_err("no HOME for ~/state");
        assert!(
            state_err.to_string().contains("HOME is not set"),
            "{state_err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn expand_tilde_non_utf8_home_is_an_error_that_names_home() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = std::ffi::OsString::from_vec(vec![0x2f, 0x80]);
        let bad_home = |_: &str| Err(VarError::NotUnicode(raw.clone()));

        for input in ["~", "~/cache"] {
            let err = expand_tilde_with_env(input, &bad_home)
                .expect_err("a non-UTF-8 HOME must not expand silently");
            let msg = format!("{err:#}");
            assert!(msg.contains("HOME"), "{msg}");
            assert!(msg.contains(input), "error must name the input: {msg}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn expand_tilde_non_utf8_home_leaves_other_paths_alone() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = std::ffi::OsString::from_vec(vec![0x80]);
        let bad_home = |_: &str| Err(VarError::NotUnicode(raw.clone()));

        assert_eq!(
            expand_tilde_with_env("/abs/path", &bad_home).expect("absolute paths skip HOME"),
            "/abs/path"
        );
        assert_eq!(
            expand_tilde_with_env("rel/path", &bad_home).expect("relative paths skip HOME"),
            "rel/path"
        );
    }

    #[test]
    fn expand_tilde_bare_tilde_equals_home() {
        let home = std::env::var("HOME").expect("HOME must be set; expand_tilde relies on it");
        let result = expand_tilde("~").expect("HOME is UTF-8 in the test environment");
        assert_eq!(result, home);
        assert!(!result.ends_with('/'));
    }

    // ===== Property tests for atomic-write byte roundtrip (#156 / #157) =====

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn has_parent_component_no_panic(s in ".*") {
            let _ = has_parent_component(&s);
        }

        #[test]
        fn is_unsafe_join_target_no_panic(s in ".*") {
            let _ = is_unsafe_join_target(&s);
        }

        #[test]
        fn has_parent_implies_unsafe_join(s in ".*") {
            // is_unsafe_join_target is a strict superset of has_parent_component
            if has_parent_component(&s) {
                prop_assert!(is_unsafe_join_target(&s),
                    "has_parent_component=true but is_unsafe_join_target=false for: {s:?}");
            }
        }

        #[test]
        fn absolute_path_always_unsafe_join(s in "/.*") {
            prop_assert!(is_unsafe_join_target(&s),
                "absolute path not flagged: {s:?}");
        }

        #[test]
        fn expand_tilde_passthrough_non_tilde(s in "[^~].*") {
            prop_assert_eq!(expand_tilde(&s).expect("non-tilde input never reads HOME"), s);
        }

        #[test]
        fn expand_tilde_never_panics(s in ".*") {
            let _ = expand_tilde(&s);
        }

        #[test]
        fn expand_tilde_with_env_follows_home(
            rest in "[a-z0-9/_.-]{0,20}",
            // A non-empty HOME: an empty one is an error, covered by the unit test below.
            home in "[a-z0-9/_.-]{1,20}",
        ) {
            let input = format!("~/{rest}");
            let read = |_: &str| Ok(home.clone());
            let got = expand_tilde_with_env(&input, &read).expect("a UTF-8 HOME never errors");
            if home.is_empty() {
                prop_assert_eq!(got, input);
            } else {
                prop_assert_eq!(got, format!("{home}/{rest}"));
            }
        }

        #[test]
        fn expand_tilde_with_env_unset_home_errors_only_for_a_tilde_input(s in ".*") {
            let unset = |_: &str| Err(VarError::NotPresent);
            let reads_home = s == "~" || s.starts_with("~/");
            prop_assert_eq!(expand_tilde_with_env(&s, &unset).is_err(), reads_home);
        }

        #[test]
        fn expand_tilde_with_env_errors_only_for_a_tilde_input(s in ".*") {
            let bad = |_: &str| Err(VarError::NotUnicode(std::ffi::OsString::from("x")));
            let reads_home = s == "~" || s.starts_with("~/");
            prop_assert_eq!(expand_tilde_with_env(&s, &bad).is_err(), reads_home);
        }

        #[test]
        fn expand_tilde_slash_contains_home_and_rest(rest in "[a-z0-9/_.-]{0,20}") {
            let home_result = std::env::var("HOME");
            prop_assume!(home_result.is_ok());
            let home = home_result.unwrap();
            let input = format!("~/{rest}");
            let result = expand_tilde(&input).expect("HOME is UTF-8 in the test environment");
            prop_assert!(result.starts_with(&home),
                "expected {result} to start with home={home}");
            prop_assert!(result.ends_with(&rest) || rest.is_empty(),
                "expected {result} to end with rest={rest}");
        }

        #[test]
        fn cwd_under_prefix_reflexive(p in "/[a-z/]{1,20}") {
            prop_assert!(cwd_under_prefix(&p, &p));
        }

        #[test]
        fn cwd_under_prefix_child_under_parent(
            parent in "/[a-z]{1,10}",
            child in "[a-z]{1,10}",
        ) {
            let full = format!("{parent}/{child}");
            prop_assert!(cwd_under_prefix(&full, &parent));
        }

        #[test]
        fn cwd_under_prefix_no_string_prefix_false_positive(
            base in "[a-z]{2,8}",
            extra in "[a-z]{1,4}",
        ) {
            let cwd = format!("/{base}{extra}");
            let prefix = format!("/{base}");
            prop_assert!(!cwd_under_prefix(&cwd, &prefix));
        }

        #[test]
        fn cwd_under_prefix_never_panics(cwd in ".*", prefix in ".*") {
            let _ = cwd_under_prefix(&cwd, &prefix);
        }

        #[test]
        fn cwd_under_prefix_transitive(
            root in "/[a-z]{1,6}",
            mid in "[a-z]{1,6}",
            leaf in "[a-z]{1,6}",
        ) {
            let b = format!("{root}/{mid}");
            let a = format!("{b}/{leaf}");
            prop_assert!(cwd_under_prefix(&b, &root));
            prop_assert!(cwd_under_prefix(&a, &b));
            prop_assert!(cwd_under_prefix(&a, &root));
        }

        #[test]
        fn cwd_under_prefix_not_symmetric(
            parent in "/[a-z]{1,10}",
            child in "[a-z]{1,10}",
        ) {
            let child_path = format!("{parent}/{child}");
            prop_assert!(!cwd_under_prefix(&parent, &child_path));
        }

        #[test]
        fn has_parent_component_safe_components(
            a in "[a-z]{1,8}",
            b in "[a-z]{1,8}",
        ) {
            let path = format!("{a}/{b}");
            prop_assert!(!has_parent_component(&path));
        }

        #[test]
        fn is_unsafe_join_target_join_safety(p in "[a-z/]{1,20}") {
            prop_assume!(!is_unsafe_join_target(&p));
            let joined = std::path::PathBuf::from("/base").join(&p);
            prop_assert!(joined.starts_with("/base"), "join escaped base: {:?}", joined);
        }

        // Arbitrary byte payloads written through write_owner_only_atomic must
        // round-trip exactly via fs::read. Catches truncation, encoding, or
        // mid-write corruption regressions across the full u8 range including
        // NUL bytes and high-bit values.
        #[test]
        fn atomic_write_byte_roundtrip(payload in proptest::collection::vec(any::<u8>(), 0..8192)) {
            let dir = tempfile::TempDir::new().expect("tempdir");
            let path = dir.path().join("payload.bin");
            write_owner_only_atomic(&path, &payload).expect("atomic write");
            let read = std::fs::read(&path).expect("read");
            prop_assert_eq!(payload, read);
        }

        // Repeated overwrites must end with the final payload exactly — no
        // residual bytes from prior writes, no torn state, no permission
        // escalation.
        #[test]
        fn atomic_write_overwrite_idempotent(
            first in proptest::collection::vec(any::<u8>(), 0..4096),
            second in proptest::collection::vec(any::<u8>(), 0..4096),
        ) {
            let dir = tempfile::TempDir::new().expect("tempdir");
            let path = dir.path().join("payload.bin");
            write_owner_only_atomic(&path, &first).expect("write 1");
            write_owner_only_atomic(&path, &second).expect("write 2");
            let read = std::fs::read(&path).expect("read");
            prop_assert_eq!(second, read);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
                prop_assert_eq!(mode & 0o077, 0, "group/other bits set after overwrite: {:o}", mode);
            }
        }
    }

    /// Marks a file executable so the `PATH` tests exercise the real predicate.
    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn binary_on_path_true_for_sh() {
        assert!(binary_on_path("sh"), "sh must be on PATH in any POSIX env");
    }

    #[test]
    fn binary_on_path_false_for_bogus_binary() {
        assert!(
            !binary_on_path("__llmenv_no_such_binary_xyzzy__"),
            "bogus binary must not be found on PATH"
        );
    }

    /// #1382: resolution must not need a `which` binary anywhere. A `PATH`
    /// holding nothing but the directory containing the target still resolves
    /// it — the case that produced a false "not installed" on distroless
    /// images, where `run_launch` turned it into a hard error.
    #[test]
    fn binary_in_path_list_resolves_without_any_helper_binaries() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("some-engine");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        make_executable(&bin);
        let found = resolve_in_path_list("some-engine", dir.path().as_os_str());
        assert_eq!(
            found.as_deref(),
            Some(bin.as_path()),
            "an executable in the only PATH entry must resolve to its full path"
        );
    }

    #[test]
    fn binary_in_path_list_rejects_non_executable_and_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let plain = dir.path().join("not-executable");
        std::fs::write(&plain, "data").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                resolve_in_path_list("not-executable", dir.path().as_os_str()).is_none(),
                "a file without an execute bit is not runnable"
            );
        }
        std::fs::create_dir(dir.path().join("a-directory")).unwrap();
        assert!(
            resolve_in_path_list("a-directory", dir.path().as_os_str()).is_none(),
            "a directory is not an executable even though it has the exec bit"
        );
    }

    /// An empty `PATH` element means "current directory" to a POSIX shell.
    /// Honouring it would let a binary in the working directory shadow a real
    /// engine, so it is skipped deliberately.
    #[test]
    fn binary_in_path_list_ignores_empty_entries() {
        assert!(
            resolve_in_path_list("some-engine", std::ffi::OsStr::new("")).is_none(),
            "an empty PATH must resolve nothing"
        );
        assert!(
            resolve_in_path_list("some-engine", std::ffi::OsStr::new(":")).is_none(),
            "empty PATH entries must not be treated as the cwd"
        );
    }

    /// #1390: the empty-entry guard has to hold even when a matching executable
    /// really is sitting in the process working directory — the hijack the
    /// duplicated resolver in `mcp/proxy.rs` was open to. Uses a name no test
    /// fixture would create so a hit could only have come from cwd resolution.
    #[cfg(unix)]
    #[test]
    fn binary_in_path_list_does_not_resolve_out_of_the_working_directory() {
        let cwd = std::env::current_dir().unwrap();
        let bin = cwd.join("__llmenv_cwd_hijack_probe__");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);
        let found = resolve_in_path_list("__llmenv_cwd_hijack_probe__", std::ffi::OsStr::new("::"));
        std::fs::remove_file(&bin).unwrap();
        assert!(
            found.is_none(),
            "an executable in the working directory must not satisfy an empty PATH entry"
        );
    }

    /// #1400: an explicit relative `PATH` entry is the same cwd hijack as the
    /// empty one, one character apart — a shell resolves both against the working
    /// directory. The executable is really there behind each relative spelling,
    /// so a guardless resolver would find it; only the absolute entry may match.
    #[cfg(unix)]
    #[test]
    fn binary_in_path_list_ignores_relative_entries_that_would_otherwise_resolve() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("some-engine");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);

        // Every relative spelling of "look in the working directory", plus a
        // relative subdirectory — a project-local `bin/` is the realistic case.
        // These are shape assertions; the companion test below is the one that
        // proves the guard, with the executable really reachable through `.`.
        for entry in ["", ".", "./", "bin", "./bin", "../bin"] {
            assert!(
                resolve_in_path_list("some-engine", std::ffi::OsStr::new(entry)).is_none(),
                "relative PATH entry {entry:?} must not resolve"
            );
        }

        // The same lookup against an absolute entry still works, so this narrows
        // the resolver rather than breaking it.
        assert_eq!(
            resolve_in_path_list("some-engine", dir.path().as_os_str()).as_deref(),
            Some(bin.as_path()),
            "an absolute PATH entry must still resolve"
        );
    }

    /// The relative-entry guard has to hold when the file really is reachable
    /// through that entry, not just when it's absent — the trap the slash and
    /// whitespace tests fell into. Runs from a working directory the test owns,
    /// with the executable inside it, so `PATH="."` would resolve without the
    /// guard.
    #[cfg(unix)]
    #[test]
    fn binary_in_path_list_ignores_a_dot_entry_with_the_binary_really_in_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let bin = cwd.join("__llmenv_relative_entry_probe__");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);

        let found =
            resolve_in_path_list("__llmenv_relative_entry_probe__", std::ffi::OsStr::new("."));
        std::fs::remove_file(&bin).unwrap();
        assert!(
            found.is_none(),
            "a `.` PATH entry must not resolve a binary sitting in the working directory"
        );
    }

    /// The name guard has to bite even when the joined path would really resolve.
    /// `binary_on_path_rejects_slash`/`_whitespace` only show that *absent* files
    /// aren't found, which holds with the guard deleted — so they don't actually
    /// pin it. Here the target exists: without the guard, `dir.join("sub/tool")`
    /// finds it and a caller could reach outside the `PATH` entry it named.
    #[cfg(unix)]
    #[test]
    fn binary_in_path_list_rejects_a_traversing_name_that_would_otherwise_resolve() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let bin = sub.join("tool");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);

        // Sanity: the guardless join would have found it.
        assert!(is_executable_file(&dir.path().join("sub/tool")));

        assert!(
            resolve_in_path_list("sub/tool", dir.path().as_os_str()).is_none(),
            "a name containing '/' must be rejected outright, not resolved relative \
             to a PATH entry"
        );
    }

    /// Same shape for the whitespace guard: a file whose name really does contain
    /// a space is present, and must still not resolve — a `PATH` lookup takes a
    /// plain binary name, and accepting one with whitespace would let a value like
    /// `sh -c echo` look installed.
    #[cfg(unix)]
    #[test]
    fn binary_in_path_list_rejects_a_whitespace_name_that_would_otherwise_resolve() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("two words");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        make_executable(&bin);

        assert!(is_executable_file(&bin));
        assert!(
            resolve_in_path_list("two words", dir.path().as_os_str()).is_none(),
            "a name containing whitespace must be rejected outright"
        );
    }

    #[test]
    fn binary_in_path_list_rejects_empty_name() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(
            resolve_in_path_list("", dir.path().as_os_str()).is_none(),
            "an empty name must not resolve to the PATH directory itself"
        );
    }

    #[test]
    fn binary_on_path_rejects_slash() {
        assert!(
            !binary_on_path("/usr/bin/sh"),
            "path with '/' must be rejected without spawning which"
        );
    }

    #[test]
    fn binary_on_path_rejects_whitespace() {
        assert!(
            !binary_on_path("sh -c echo"),
            "name with whitespace must be rejected without spawning which"
        );
        assert!(
            !binary_on_path("sh\techo"),
            "name with tab must be rejected without spawning which"
        );
    }

    #[test]
    fn copy_replacing_symlink_copies_content_to_a_fresh_destination() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::write(&src, b"payload").expect("write src");

        copy_replacing_symlink(&src, &dst).expect("copy");

        assert_eq!(std::fs::read(&dst).expect("read"), b"payload");
    }

    #[test]
    fn copy_replacing_symlink_overwrites_an_existing_regular_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::write(&src, b"new-content").expect("write src");
        std::fs::write(&dst, b"old-content").expect("write dst");

        copy_replacing_symlink(&src, &dst).expect("copy");

        assert_eq!(std::fs::read(&dst).expect("read"), b"new-content");
    }

    /// A plain `std::fs::copy` opens the destination with `O_TRUNC`, which
    /// follows a symlink and truncates/overwrites whatever it points at
    /// instead of replacing the link itself. `copy_replacing_symlink` must
    /// replace the destination directory entry atomically (via `rename`),
    /// which never follows a symlink at the destination.
    #[cfg(unix)]
    #[test]
    fn copy_replacing_symlink_does_not_write_through_a_symlinked_destination() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        let victim = tmp.path().join("victim");
        std::fs::write(&src, b"attacker-controlled").expect("write src");
        std::fs::write(&victim, b"must-not-be-touched").expect("write victim");
        std::os::unix::fs::symlink(&victim, &dst).expect("symlink");

        copy_replacing_symlink(&src, &dst).expect("copy");

        assert!(
            !std::fs::symlink_metadata(&dst)
                .expect("stat dst")
                .file_type()
                .is_symlink(),
            "the symlink at the destination must be replaced, not written through"
        );
        assert_eq!(
            std::fs::read(&dst).expect("read dst"),
            b"attacker-controlled"
        );
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"must-not-be-touched",
            "the symlink's target must be untouched"
        );
    }

    /// The copy preserves the source's permission mode — unlike the
    /// owner-only writers above, this helper is for arbitrary bundle content
    /// (which may need to stay executable), not credentials.
    #[cfg(unix)]
    #[test]
    fn copy_replacing_symlink_preserves_the_source_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::write(&src, b"#!/bin/sh\necho hi\n").expect("write src");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        copy_replacing_symlink(&src, &dst).expect("copy");

        let mode = std::fs::metadata(&dst)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "the executable bit must survive the copy");
    }

    /// Group/other write and setuid/setgid must never survive the copy — a
    /// bundle file sourced from a permissive local path or tarball must not
    /// land group/world-writable inside a directory the agent executes code
    /// from (security-audit, #1426).
    #[cfg(unix)]
    #[test]
    fn copy_replacing_symlink_masks_group_other_write_and_setid_bits() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::write(&src, b"payload").expect("write src");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o6777))
            .expect("chmod 6777");

        copy_replacing_symlink(&src, &dst).expect("copy");

        let mode = std::fs::metadata(&dst)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o755,
            "group/other write and setuid/setgid must be masked off: got {mode:o}"
        );
    }

    /// The temp file itself must be opened exclusively (`O_EXCL`), not
    /// written to through a second, non-exclusive open the way
    /// `std::fs::copy(src, &tmp_path)` would — otherwise a symlink planted at
    /// the exact predictable temp name between the exclusive-open check and a
    /// second open gets written through, which is the residual TOCTOU
    /// security-audit found in the first cut of this function (#1426). A
    /// symlink already sitting at the counter-derived temp name must make
    /// this attempt collide and retry onto the next name, not follow it.
    #[cfg(unix)]
    #[test]
    fn copy_replacing_symlink_does_not_follow_a_symlink_planted_at_the_temp_name() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        let victim = tmp.path().join("victim");
        std::fs::write(&src, b"payload").expect("write src");
        std::fs::write(&victim, b"must-not-be-touched").expect("write victim");

        let pid = std::process::id();
        let now = std::time::SystemTime::now();
        let nanos = now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // Plant a symlink at every temp name the counter could plausibly
        // produce next, forcing at least one `create_new` collision before a
        // fresh (non-symlinked) name is found.
        let next = COPY_TMP_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        for counter in next..next + 4 {
            let name = format!("dst.{pid}.{nanos}.{counter}.tmp");
            std::os::unix::fs::symlink(&victim, tmp.path().join(name))
                .expect("plant symlink at candidate temp name");
        }

        copy_replacing_symlink(&src, &dst).expect("copy despite planted symlinks");

        assert_eq!(std::fs::read(&dst).expect("read dst"), b"payload");
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"must-not-be-touched",
            "a symlink planted at a candidate temp name must never be written through"
        );
    }

    #[test]
    fn copy_replacing_symlink_leaves_no_temp_files() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::write(&src, b"payload").expect("write src");

        copy_replacing_symlink(&src, &dst).expect("copy");

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 2, "found stray files: {entries:?}");
    }

    #[test]
    fn reject_non_regular_file_allows_a_missing_path() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("nope");
        reject_non_regular_file(&path).expect("a missing path must be allowed");
    }

    #[test]
    fn reject_non_regular_file_allows_a_real_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("real");
        std::fs::write(&path, b"content").expect("write");
        reject_non_regular_file(&path).expect("a regular file must be allowed");
    }

    #[cfg(unix)]
    #[test]
    fn reject_non_regular_file_rejects_a_symlink() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"content").expect("write victim");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");

        let err = reject_non_regular_file(&link).expect_err("a symlink must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn reject_non_regular_file_rejects_a_dangling_symlink() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("does-not-exist"), &link)
            .expect("dangling symlink");

        assert!(
            reject_non_regular_file(&link).is_err(),
            "a dangling symlink must be rejected, not treated as absent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reject_non_regular_file_rejects_a_directory() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dir = tmp.path().join("subdir");
        std::fs::create_dir(&dir).expect("mkdir");

        assert!(reject_non_regular_file(&dir).is_err());
    }

    proptest! {
        #[test]
        fn resolve_in_path_list_never_resolves_from_an_all_empty_path(
            name in "[a-zA-Z0-9_.-]{1,12}",
            separators in 0usize..8,
        ) {
            let path_var = ":".repeat(separators);
            prop_assert!(
                resolve_in_path_list(&name, std::ffi::OsStr::new(&path_var)).is_none(),
                "name {:?} resolved against an all-empty PATH {:?}",
                name,
                path_var
            );
        }

        /// Never panics, whatever arbitrary text arrives as either argument, and
        /// anything it does return is the name joined onto one of the `PATH`
        /// entries — never a path it invented.
        #[test]
        fn resolve_in_path_list_returns_only_a_path_var_entry_join(
            name in ".*",
            path_var in ".*",
        ) {
            let os_path = std::ffi::OsStr::new(&path_var);
            if let Some(found) = resolve_in_path_list(&name, os_path) {
                prop_assert_eq!(
                    found.file_name(),
                    Some(std::ffi::OsStr::new(&name)),
                    "resolved {:?} for name {:?}",
                    found,
                    name
                );
                let parent = found.parent().map(std::path::Path::to_path_buf);
                prop_assert!(
                    std::env::split_paths(os_path).any(|dir| Some(&dir) == parent.as_ref()),
                    "resolved {:?}, whose parent is not a PATH entry of {:?}",
                    found,
                    path_var
                );
            }
        }

    }
}
