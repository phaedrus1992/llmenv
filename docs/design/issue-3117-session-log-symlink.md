# Issue #3117 — refuse a symlinked session log at setup

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/3117
- **Milestone:** `v3.14.0`
- **Base branch:** `release/3.x`
- **Code:** `src/session_log/file_sink.rs`, `src/main.rs`, `src/hook_run/session_emit.rs`
- **Type:** behavior change at sink setup, tests, docs

## Problem

`is_full` in `src/session_log/rotation.rs:44` reads the size with `std::fs::metadata`, which follows a symbolic link.
`rotate_if_full` (`src/session_log/rotation.rs:21`) then renames the path itself, which moves the link and not the file.
So for a symlinked log the size check and the rename act on different files.

Today a symlinked log fails only at append time.
`FileSink::try_append` (`src/session_log/file_sink.rs:107`) opens with `O_NOFOLLOW` through `crate::paths::refuse_symlink`, so the open fails with `ELOOP`.
The sink stays on, and each process records an append-failure notice.

## Decision

Owner decision on #3117 (2026-11-03), option 1:

> Reject a symlinked session log at setup, with a fix message that names the path. The size check and the rename code stay as they are. Acceptance: a test with a symlinked log fails setup with that message.

## What does not change

- `is_full`, `rotate_if_full`, and `FileSink::try_append`.
- `resolve_file_path` (`src/session_log/file_sink.rs:29`). Its other callers only read the path: `llmenv doctor` (`src/cli/doctor/session_log_path.rs:51`), the cache GC (`src/cli/doctor/cache_gc/session_rotation.rs:43`), the status data (`src/materialize/status_data.rs:284`), and the SessionStart reaper (`src/hook_run/mod.rs:1972`). They keep working on a symlinked path.
- The doctor check `check_path` (`src/cli/doctor/session_log_path.rs:82`). It already reports a symbolic link as "not a regular file".
- Only the last path component is checked. A symbolic link in a parent folder is out of scope.

## Design

### New types and functions in `src/session_log/file_sink.rs`

Add an error type for the setup of a writing sink:

```rust
/// Why the session-log sink cannot start.
#[derive(Debug)]
pub enum SinkPathError {
    /// The configured or default path cannot be resolved.
    Resolve(anyhow::Error),
    /// The log path is a symbolic link. The size check follows the link, and the rotation
    /// renames the link itself, so the two would act on different files (#3117).
    Symlink(std::path::PathBuf),
}
```

Implement `std::fmt::Display` and `std::error::Error` for it with `thiserror`, as other error types in the crate do.
The `Display` text of each variant:

- `Resolve(e)`: `{0:#}`, the alternate form of the `anyhow` error, so `{err}` prints the whole cause chain as `{e:#}` does today.
- `Symlink(path)`: `session_log.file.path {path} is a symbolic link, and llmenv does not write the session log through a link. Fix: set session_log.file.path to the real file, or replace the link with a regular file.` Use `path.display()` for `{path}`.

Add the setup functions for a writing sink.
They mirror `resolve_file_path` and `resolve_file_path_with` (`src/session_log/file_sink.rs:29` and `:34`):

```rust
/// Resolve the session-log path for a sink that writes to it.
///
/// # Errors
/// Returns `SinkPathError::Resolve` when the path cannot be resolved, and
/// `SinkPathError::Symlink` when the path is a symbolic link.
pub fn resolve_sink_path(configured: Option<&str>) -> Result<PathBuf, SinkPathError> {
    resolve_sink_path_with(configured, default_file_path)
}

/// [`resolve_sink_path`] with the default path given, for a caller that has its own default.
///
/// # Errors
/// The same as [`resolve_sink_path`].
pub fn resolve_sink_path_with(
    configured: Option<&str>,
    default: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> Result<PathBuf, SinkPathError> {
    let path = resolve_file_path_with(configured, default).map_err(SinkPathError::Resolve)?;
    if is_symlink(&path) {
        return Err(SinkPathError::Symlink(path));
    }
    Ok(path)
}

/// Whether `path` itself is a symbolic link. A missing path is not one: the first append creates it.
/// Any other metadata error is not one either: the append then reports the real failure.
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}
```

### Re-export

`src/main.rs` is the binary crate, and it imports `llmenv::session_log::resolve_file_path` through the re-export in `src/session_log/mod.rs` (line 19).
Add `resolve_sink_path`, `resolve_sink_path_with`, and `SinkPathError` to that same re-export.

### Caller 1: `src/main.rs:8` (`session_log_file_path`)

Change the call from `resolve_file_path` to `resolve_sink_path`, imported through the re-export.
On any `Err(e)`, keep the current behavior: print `llmenv: failed to resolve the session log path: {e}; the file log is off` with `eprintln!`, and return `None`.
`{e}` of a `SinkPathError::Resolve` prints the same text as today's `{e:#}`, because its `Display` uses `{0:#}`.
The file log stays off for that run.

### Caller 2: `resolve_file_sink_path` in `src/hook_run/session_emit.rs` (around line 40)

This function takes `(cfg, default: impl FnOnce() -> anyhow::Result<PathBuf>)` and calls `resolve_file_path_with` today.
Change that call to `resolve_sink_path_with` with the same arguments, and match on the error:

- `SinkPathError::Resolve(e)`: keep today's `tracing::warn!` text and behavior unchanged. The test `file_sink_without_path_warns_with_fix_when_default_fails` must keep passing without edits.
- `SinkPathError::Symlink(_)`: call `crate::agent_notice::record_logged(format!("llmenv: the session log is off for this event: {err}"))`, where `{err}` is the `Display` text of the error. Return `None`.

The engine drops a hook's stderr, and the default filter hides `warn!`.
So the symlink case uses an agent notice, as the append failure does today.
Each hook event is its own process, so the notice repeats once per event.
That is the same rate as the append-failure notice that it replaces.

### `src/session_log/tracing_layer.rs:114`

`capture_file_logs_at` calls `FileSink::new` directly for tests.
Do not change it.

## Tests

Add these tests to the `tests` module of `src/session_log/file_sink.rs`.
Mark each test that makes a symbolic link `#[cfg(unix)]`, and use `std::os::unix::fs::symlink`.

1. `a_symlinked_log_path_is_refused_with_the_path_and_the_fix`
   - Make a temp folder with a regular file `real.jsonl` and a link `log.jsonl` that points to it.
   - Call `resolve_sink_path(Some(<link path as &str>))`.
   - Assert the result is `Err(SinkPathError::Symlink(p))` with `p` equal to the link path.
   - Assert the `Display` text contains the link path, `symbolic link`, and `Fix:`.
2. `a_missing_log_path_is_accepted` — a path in a temp folder that does not exist gives `Ok(path)`.
3. `a_regular_log_file_is_accepted` — a regular file gives `Ok(path)`.
4. `a_dangling_symlink_is_refused` — a link whose target does not exist gives `Err(SinkPathError::Symlink(_))`.
5. `a_relative_path_is_a_resolve_error` — `resolve_sink_path(Some("relative.jsonl"))` gives `Err(SinkPathError::Resolve(_))`.

Add one test to the `tests` module of `src/hook_run/session_emit.rs`:

6. `a_symlinked_log_path_turns_the_sink_off_with_a_notice`
   - Configure `session_log.file.path` to a symbolic link, the same way the existing tests in that file build a config.
   - Call `resolve_file_sink_path` with that config and a default closure that returns an error. A configured path never calls the default.
   - Assert it returns `None`.
   - Assert `crate::agent_notice::current().unwrap_or_default()` contains the link path and `symbolic link`. The notice list is process-wide, so use `contains` only.

## Docs

Edit `website/docs/configuration.md`, section `## session_log:`, the `path` row of the table (around line 1289).
Add this sentence to the row:

```markdown
A symbolic link is refused, and the file log stays off with a notice that names the path (changed in v3.14.0).
```

## Changelog

Add one entry under `## [Unreleased]` → `### Changed` in `CHANGELOG-3.md`:

```markdown
- Refuse a session log path that is a symbolic link when the file sink starts, with a notice that names the path. Rotation measured the link target but renamed the link. [fix:session-log] [#3117]
```

Take the tag from the commit that adds `resolve_sink_path`.
Then run `scripts/sync-changelog-doc.sh` and commit `website/docs/changelog.md` with it.

## Spec-writer calls

- Only the two writing setup sites refuse the link. The read-only callers keep `resolve_file_path`, because `doctor` already reports the link and the reaper only reads metadata.
- The hook path uses an agent notice, not a `warn!`, because a `warn!` never reaches the agent.
- A dangling link is refused too. It is still a link, and the first append would fail on it.
