//! XDG paths and path helpers.

use std::path::{Path, PathBuf};

/// Expand a leading `~` or `~/` to `$HOME`. Other input is returned unchanged.
/// Returns the input unchanged when `HOME` is unset.
#[must_use]
pub fn expand_tilde(p: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
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
    if let Ok(dir) = std::env::var("LLMENV_CONFIG_DIR") {
        Ok(PathBuf::from(dir))
    } else {
        let home = std::env::var("HOME")?;
        Ok(PathBuf::from(home).join(".config/llmenv"))
    }
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("config.yaml"))
}

pub fn state_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("LLMENV_STATE_DIR") {
        Ok(PathBuf::from(dir))
    } else {
        let home = std::env::var("HOME")?;
        Ok(PathBuf::from(home).join(".local/state/llmenv"))
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
    std::fs::create_dir_all(parent)?;
    // Harden parent dir to 0o700 (owner-only). Without this, default umask
    // 0o022 leaves the state dir at 0o755 (world-listable), leaking the
    // existence and names of state files on shared systems. Failure is
    // non-fatal — on platforms that don't support it (Windows), or if the
    // dir was created by another process and we lack chmod rights, we still
    // proceed with the file-level 0o600 protection.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
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
    fn expand_tilde_bare_tilde_equals_home() {
        let home = std::env::var("HOME").expect("HOME must be set; expand_tilde relies on it");
        let result = expand_tilde("~");
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
