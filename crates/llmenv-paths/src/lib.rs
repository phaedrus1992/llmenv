//! XDG paths and path helpers.

use std::path::{Path, PathBuf};

/// Expand a leading `~` or `~/` to `$HOME`. Other input is returned unchanged.
/// Returns the input unchanged when `HOME` is unset or empty.
#[must_use]
pub fn expand_tilde(p: &str) -> String {
    expand_tilde_with_env(p, &|name| std::env::var(name).ok())
}

/// [`expand_tilde`] with an injectable env-var provider so tests can exercise
/// the set-but-empty `HOME` case without mutating real process env vars.
fn expand_tilde_with_env(p: &str, get_env: &impl Fn(&str) -> Option<String>) -> String {
    // A set-but-empty HOME has the same failure mode as HOME being unset — an
    // empty home would silently anchor "~/rest" at the filesystem root
    // ("/rest") instead of leaving the input unchanged (#1179).
    let Some(home) = get_env("HOME").filter(|h| !h.is_empty()) else {
        return p.to_string();
    };
    if let Some(rest) = p.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if p == "~" {
        home
    } else {
        p.to_string()
    }
}

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

/// True if `path` contains any parent (`..`) component, parsed
/// component-wise rather than by substring. Catches traversal that string
/// matching misses: `foo/..`, mixed separators on the host OS, and a bare
/// `..` with no trailing slash. A leading `/` (root) is fine; only `..`
/// components are rejected.
///
/// Note: this does NOT check whether `path` is absolute. `Path::join` with
/// an absolute argument returns the argument unchanged, escaping the base
/// directory. When validating relative paths supplied by user-controlled
/// data, use [`is_unsafe_join_target`] instead.
#[must_use]
pub fn has_parent_component(path: &str) -> bool {
    use std::path::Component;
    Path::new(path)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
}

/// True if joining `path` onto a base directory would escape it. Returns
/// true when `path` contains `..` components OR is absolute (since
/// `Path::join` with an absolute argument discards the base). Use this at
/// every site that does `base.join(user_controlled_rel)`.
#[must_use]
pub fn is_unsafe_join_target(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute() || has_parent_component(path)
}

/// True if `name` is safe to use as a single path component (a marketplace,
/// skill, or plugin-collection name) and as a JSON key — ASCII
/// alphanumeric plus `.`/`_`/`-`, not empty, not `.`/`..`, not leading with
/// `-` (git/CLI arg-parsing hazard). Rejects everything a component-based
/// blocklist could miss (control characters, Unicode formatting characters
/// like zero-width space or RTL override, path separators) by construction,
/// rather than by enumerating what to reject (#534).
#[must_use]
pub fn is_valid_short_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.starts_with('-') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
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

/// Return true if `cwd` is at or below `prefix`, treating both as filesystem
/// paths (component-wise) rather than raw strings. This avoids the
/// `/home/alice/git/xyz` matches prefix `/home/alice/git/x` bug.
#[must_use]
pub fn cwd_under_prefix(cwd: &str, prefix: &str) -> bool {
    let cwd_p = Path::new(cwd);
    let pre_p = PathBuf::from(prefix);
    cwd_p.starts_with(&pre_p)
}

pub fn config_dir() -> anyhow::Result<PathBuf> {
    config_dir_with_env(&|name| std::env::var(name).ok())
}

/// [`config_dir`] with an injectable env-var provider so tests can exercise
/// the set-but-empty override case without mutating real process env vars.
fn config_dir_with_env(get_env: &impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    // A set-but-empty override (`LLMENV_CONFIG_DIR=`) must fall through to the
    // `$HOME` default rather than resolving to a relative `PathBuf::from("")` (#1111).
    if let Some(dir) = get_env("LLMENV_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Ok(PathBuf::from(dir))
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
    state_dir_with_env(&|name| std::env::var(name).ok())
}

/// [`state_dir`] with an injectable env-var provider so tests can exercise
/// the set-but-empty override case without mutating real process env vars.
fn state_dir_with_env(get_env: &impl Fn(&str) -> Option<String>) -> anyhow::Result<PathBuf> {
    // A set-but-empty override (`LLMENV_STATE_DIR=`) must fall through to the
    // `$HOME` default rather than resolving to a relative `PathBuf::from("")` (#1111).
    if let Some(dir) = get_env("LLMENV_STATE_DIR").filter(|d| !d.is_empty()) {
        Ok(PathBuf::from(dir))
    } else {
        // Same reasoning as `config_dir_with_env`: a set-but-empty `HOME` must
        // also be treated as unset, not just the override.
        let home = get_env("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow::anyhow!("environment variable not found: HOME"))?;
        Ok(PathBuf::from(home).join(".local/state/llmenv"))
    }
}

/// Create a directory (and any missing parent components) with owner-only
/// permissions (mode 0o700 on Unix) from the moment of creation, and harden
/// it to 0o700 if it already existed at a looser mode. On Windows falls back
/// to `create_dir_all`'s default permissions.
///
/// Use instead of `create_dir_all` followed by a separate `set_permissions`
/// call for any directory that must never be world-readable — the two-call
/// version leaves the directory at the umask default (typically 0o755)
/// between creation and the chmod, a TOCTOU window (#1113), and a caller who
/// skips the follow-up chmod (or an older llmenv version, before this
/// hardening existed) leaves it world-readable indefinitely (#1178).
///
/// # Errors
/// Propagates directory-creation failure, and failure to chmod an
/// already-existing directory (e.g. owned by another user).
pub fn create_dir_owner_only(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // DirBuilder's mode only governs directories it creates. If `dir`
        // already existed (an older llmenv version, a caller that used a
        // bare create_dir_all, a permissive umask), its mode is left
        // untouched -- self-heal it so every caller gets an owner-only
        // directory regardless of whether it was just created.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Write `content` to `path` with owner-only permissions (mode 0o600) on Unix.
/// On Windows falls back to default permissions. Creates the file if absent,
/// truncates if present. Use for any file containing user state or
/// credentials (settings, sync state, MCP configs, ICM memory) where
/// world-readable defaults would leak data on shared systems.
pub fn write_owner_only(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(content)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)?;
    }
    Ok(())
}

/// Atomically write `content` to `path` with owner-only permissions.
///
/// Steps: write to a same-directory temp file `<path>.<pid>.<nanos>.tmp`,
/// `fsync` it for durability, then `rename` over the destination (POSIX
/// atomic replace). Readers observing `path` mid-write see either the prior
/// good contents or the new contents — never a torn document. On error the
/// temp file is removed.
///
/// Use for any structured/JSON state file where a half-written file would
/// break the next read: `icm.json`, `sync.json`, `settings.json`, `mcp.json`.
pub fn write_owner_only_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no parent: {}", path.display()),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no file name: {}", path.display()),
        )
    })?;
    if parent.as_os_str().is_empty() {
        // For paths like "foo.json" (no parent dir), use current dir.
        return write_owner_only_atomic_in_dir(Path::new("."), file_name, path, content);
    }
    // Born owner-only (0o700) at creation, including every missing
    // ancestor — a create_dir_all + post-hoc set_permissions leaves the
    // umask default (typically 0o755, world-listable) both as a TOCTOU
    // window on the immediate parent and permanently on any intermediate
    // ancestor it doesn't chmod (#1178).
    create_dir_owner_only(parent).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("creating/hardening directory {}: {e}", parent.display()),
        )
    })?;
    write_owner_only_atomic_in_dir(parent, file_name, path, content)
}

/// Process-local counter used to disambiguate temp filenames when multiple
/// calls within the same process land in the same nanosecond. Combined with
/// `pid` and `nanos`, this guarantees uniqueness within a process and is
/// extremely unlikely to collide across processes (different pids).
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_owner_only_atomic_in_dir(
    parent: &Path,
    file_name: &std::ffi::OsStr,
    final_path: &Path,
    content: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    // Retry on EEXIST up to a small number of times. A stale temp file (from
    // a prior crashed process with the same pid+nanos slice) or in-process
    // race could collide; the per-process counter and retry loop together
    // guarantee progress without unbounded blocking.
    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..8 {
        let counter = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".{pid}.{nanos}.{counter}.tmp"));
        let tmp_path = parent.join(&tmp_name);

        let result = (|| -> std::io::Result<()> {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp_path)?;
                file.write_all(content)?;
                file.sync_all()?;
            }
            #[cfg(not(unix))]
            {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp_path)?;
                file.write_all(content)?;
                file.sync_all()?;
            }
            std::fs::rename(&tmp_path, final_path)?;
            Ok(())
        })();

        match result {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted temp-file collision retries",
        )
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

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

    #[cfg(unix)]
    #[test]
    fn create_dir_owner_only_is_0o700_from_creation() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested").join("store");
        create_dir_owner_only(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "dir must be born owner-only, got {mode:o}");
    }

    #[test]
    fn create_dir_owner_only_is_idempotent_on_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        create_dir_owner_only(&dir).unwrap();
        create_dir_owner_only(&dir).unwrap();
        assert!(dir.is_dir());
    }

    // A directory created before this hardening shipped (older llmenv
    // version, a caller using bare create_dir_all, a permissive umask) must
    // still end up owner-only the next time something calls
    // create_dir_owner_only on it -- not just directories it creates fresh.
    #[cfg(unix)]
    #[test]
    fn create_dir_owner_only_hardens_a_preexisting_looser_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        create_dir_owner_only(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "pre-existing looser dir must be hardened, got {mode:o}"
        );
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

    #[cfg(unix)]
    #[test]
    fn write_owner_only_sets_mode_0o600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("secret");
        write_owner_only(&path, b"sensitive").expect("write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        // Group/other bits must be clear — file is owner-only.
        assert_eq!(mode & 0o077, 0, "group/other bits set: {mode:o}");
        let body = std::fs::read(&path).expect("read");
        assert_eq!(body, b"sensitive");
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_truncates_existing_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("file");
        write_owner_only(&path, b"longer content").expect("write1");
        write_owner_only(&path, b"short").expect("write2");
        let body = std::fs::read(&path).expect("read");
        assert_eq!(body, b"short");
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_only_atomic_creates_file_with_mode_0o600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("atomic");
        write_owner_only_atomic(&path, b"payload").expect("atomic write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "group/other bits set: {mode:o}");
        assert_eq!(std::fs::read(&path).expect("read"), b"payload");
    }

    #[test]
    fn write_owner_only_atomic_replaces_existing_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("file");
        write_owner_only_atomic(&path, b"v1").expect("v1");
        write_owner_only_atomic(&path, b"v2-longer").expect("v2");
        assert_eq!(std::fs::read(&path).expect("read"), b"v2-longer");
    }

    #[test]
    fn write_owner_only_atomic_leaves_no_temp_files() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("file");
        write_owner_only_atomic(&path, b"x").expect("write");
        write_owner_only_atomic(&path, b"y").expect("write");
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1, "found stray files: {entries:?}");
    }

    #[test]
    fn write_owner_only_atomic_creates_parent_dir() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("a/b/c/file.json");
        write_owner_only_atomic(&path, b"nested").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"nested");
    }

    // #1178: write_owner_only_atomic must create every missing ancestor
    // directory owner-only from the moment of creation, not just the
    // immediate parent. The old create_dir_all + post-hoc set_permissions
    // approach only chmods the immediate parent, leaving intermediate
    // ancestors at the umask default (world-readable) forever, and leaves a
    // TOCTOU window on the immediate parent between creation and chmod.
    #[cfg(unix)]
    #[test]
    fn write_owner_only_atomic_creates_all_missing_ancestors_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp
            .path()
            .join("grandparent")
            .join("parent")
            .join("file.json");
        write_owner_only_atomic(&path, b"nested").expect("write");

        for ancestor in ["grandparent", "grandparent/parent"] {
            let dir = tmp.path().join(ancestor);
            let mode = std::fs::metadata(&dir)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "{ancestor} must be born owner-only, got {mode:o}"
            );
        }
    }

    #[test]
    fn write_owner_only_atomic_concurrent_writers_no_torn_reads() {
        // Spawn N threads writing distinct fixed-size payloads to the same
        // path. Every reader sees one of the written payloads — never a
        // partial document, never an empty file.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("contended.json");
        write_owner_only_atomic(&path, b"initial").expect("seed");

        let payloads: Vec<Vec<u8>> = (0..8)
            .map(|i| format!("{{\"writer\":{i},\"data\":\"{}\"}}", "x".repeat(256)).into_bytes())
            .collect();
        let valid: std::collections::HashSet<Vec<u8>> = std::iter::once(b"initial".to_vec())
            .chain(payloads.iter().cloned())
            .collect();

        let writers: Vec<_> = payloads
            .into_iter()
            .map(|payload| {
                let p = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        write_owner_only_atomic(&p, &payload).expect("concurrent write");
                    }
                })
            })
            .collect();

        let reader_path = path.clone();
        let reader_valid = valid.clone();
        let reader = std::thread::spawn(move || {
            for _ in 0..200 {
                let body = std::fs::read(&reader_path).expect("concurrent read");
                assert!(
                    reader_valid.contains(&body),
                    "reader observed torn write: {body:?}"
                );
            }
        });

        for w in writers {
            w.join().expect("writer join");
        }
        reader.join().expect("reader join");
    }

    #[test]
    fn tilde_passthrough_for_absolute_and_relative() {
        // Tests the non-HOME-dependent branches.
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("rel/path"), "rel/path");
        assert_eq!(expand_tilde(""), "");
    }

    #[test]
    fn expand_tilde_with_env_empty_home_behaves_like_unset() {
        let unset = |_: &str| None;
        let empty = |name: &str| match name {
            "HOME" => Some(String::new()),
            _ => None,
        };
        assert_eq!(
            expand_tilde_with_env("~/foo", &unset),
            expand_tilde_with_env("~/foo", &empty)
        );
        assert_eq!(expand_tilde_with_env("~/foo", &empty), "~/foo");
    }

    #[test]
    fn expand_tilde_bare_tilde_equals_home() {
        let home = std::env::var("HOME").expect("HOME must be set; expand_tilde relies on it");
        let result = expand_tilde("~");
        assert_eq!(result, home);
        assert!(!result.ends_with('/'));
    }

    // ===== Property tests for atomic-write byte roundtrip (#156 / #157) =====

    use proptest::prelude::*;

    proptest! {
        /// A `PATH` of nothing but empty entries resolves nothing, for any name —
        /// the cwd-hijack guard (#1382, #1390) generalised past the `""` and `":"`
        /// the example tests pin.
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
            prop_assert_eq!(expand_tilde(&s), s);
        }

        #[test]
        fn expand_tilde_never_panics(s in ".*") {
            let _ = expand_tilde(&s);
        }

        #[test]
        fn expand_tilde_slash_contains_home_and_rest(rest in "[a-z0-9/_.-]{0,20}") {
            let home_result = std::env::var("HOME");
            prop_assume!(home_result.is_ok());
            let home = home_result.unwrap();
            let input = format!("~/{rest}");
            let result = expand_tilde(&input);
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
}
