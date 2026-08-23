# Launch Mid-Session Supervision Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task-by-task (global policy overrides
> subagent-driven-development — see dev-sprint composite #1480's handoff
> notes). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `llmenv launch` three mid-session behaviors it doesn't have
today: relaunch the engine after a crash (#1284), notice a nearly-expired
credential (#1285, detect+notify only), and notice a config edit made while
the session is running (#1286).

**Architecture:** Extract `launch`'s code out of `src/cli/mod.rs` into its own
`src/launch/` module. Add a graceful-relaunch loop around the existing
spawn/supervise cycle. Add a per-session Unix socket (new — the v1 design's
socket never shipped) with one verb, `pending_events`, that two new
background tasks (drift watch, credential watch) push into. `hook_run` polls
that verb on every invocation it already makes and renders any notice through
the adapter's existing `emit_hook_context()`.

**Tech Stack:** Rust, tokio (`net`, `time` features added this plan), serde_json,
`assert_cmd` for integration tests (matching `tests/launch.rs`'s existing style).

**Spec:** `docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md`

## Global Constraints

- Unix-only (matches base `launch` design's own non-goal — no Windows path).
- No `unsafe` (workspace lint: `unsafe_code = "deny"`).
- `print_stdout`/`print_stderr` are `deny` in lib code; `src/launch/` is part
  of the `llmenv` binary crate, which allows user-facing `println!`/`eprintln!`
  — follow the existing pattern in `src/cli/mod.rs` (`eprintln!` for
  user-visible warnings, `tracing::debug!`/`tracing::error!` for diagnostics).
- `cargo fmt` after every file edit; `cargo clippy --all-targets --all-features -- -D warnings`
  and `cargo test --workspace` must pass before each commit.
- Every new/moved `pub(crate)` item keeps its existing visibility — this is
  all internal to the `llmenv` binary crate, no new public API.
- The mid-session notice channel must degrade silently: a missing socket, a
  timed-out connection, or a malformed response is never a user-visible error,
  matching the design doc's "Degradation" contract.

---

## Task 1: Extract `launch` into its own module

**Files:**
- Create: `src/launch/mod.rs`
- Modify: `src/cli/mod.rs` (remove the moved items, add a thin call-through)
- Modify: `src/lib.rs:23-24` (add `pub mod launch;` between `icm` and `materialize`)
- Modify: `Cargo.toml:83` (add `net`, `time` to the `tokio` feature list — needed
  starting Task 3, adding now so Task 1's move compiles against the final
  feature set without a second Cargo.toml churn)

**Interfaces:**
- Produces: `pub(crate) fn launch::run(engine: &str, args: Vec<String>, scope: LaunchScope) -> anyhow::Result<()>`
  (renamed from `run_launch` — `run` reads clearly as `launch::run` at the
  call site). `pub(crate) struct launch::LaunchScope { scope: Option<String>, tag: Option<String>, compress: bool }`.
- Consumes: nothing new — this task moves code, it doesn't change behavior.

This is a pure move: no behavior change, existing `tests/launch.rs` must pass
unmodified afterward. That test suite *is* this task's test — there's no new
test to write first.

- [ ] **Step 1: Add `net` and `time` to the tokio feature list**

In `Cargo.toml`, change:
```toml
tokio = { version = "=1.53.1", features = ["rt-multi-thread", "macros", "fs", "process", "io-util", "sync", "signal"] }
```
to:
```toml
tokio = { version = "=1.53.1", features = ["rt-multi-thread", "macros", "fs", "process", "io-util", "sync", "signal", "net", "time"] }
```

- [ ] **Step 2: Create `src/launch/mod.rs` and move the launch-specific items**

Cut these items from `src/cli/mod.rs` (verbatim, no changes) and paste into
`src/launch/mod.rs`, adding `use anyhow::Context;` and any other imports
`cargo check` reports missing:

- `struct LaunchScope` (currently private at `src/cli/mod.rs:1472`) → make it
  `pub(crate) struct LaunchScope` with `pub(crate)` fields.
- `fn run_launch` (`src/cli/mod.rs:1482-1519`) → rename to `pub(crate) fn run`.
- `fn spawn_and_supervise` (`src/cli/mod.rs:1630-1695`)
- `fn write_stdin_payload` (`src/cli/mod.rs:1707-1719`)
- `fn write_child_stdin` (`src/cli/mod.rs:1730-1746`)
- `fn forward_signal` (`src/cli/mod.rs:1767-...`, the `#[cfg(unix)]` block)

Leave `run_supervised`, `command_for_binary`, `command_at_path`,
`exit_with_status` in `src/cli/mod.rs` — they're used by non-launch call
sites too (`login`, `setup`, `edit`'s `$EDITOR`), so moving them would make
`src/launch/mod.rs` reach back into `cli` for its own dependents, the wrong
direction. `src/launch/mod.rs` calls `crate::cli::run_supervised(...)` etc.,
same as before, just qualified.

- [ ] **Step 3: Update `src/lib.rs`**

Add `pub mod launch;` between `pub mod icm;` (line 23) and `pub mod materialize;` (line 25).

- [ ] **Step 4: Update the `Command::Launch` match arm in `src/cli/mod.rs`**

Change:
```rust
        Some(Command::Launch {
            scope,
            tag,
            compress,
            engine,
            args,
        }) => {
            run_launch(
                &engine,
                args,
                LaunchScope {
                    scope,
                    tag,
                    compress,
                },
            )?;
        }
```
to:
```rust
        Some(Command::Launch {
            scope,
            tag,
            compress,
            engine,
            args,
        }) => {
            crate::launch::run(
                &engine,
                args,
                crate::launch::LaunchScope {
                    scope,
                    tag,
                    compress,
                },
            )?;
        }
```

- [ ] **Step 5: Fix visibility on anything `src/launch/mod.rs` now needs from `cli`**

`resolve_env`, `command_at_path`, `run_supervised` are currently private
(`fn`, not `pub(crate) fn`) in `src/cli/mod.rs` because only other functions
in the same file called them. Change each to `pub(crate) fn` so
`src/launch/mod.rs` can call `crate::cli::resolve_env(...)` etc. Run `cargo check`
and fix any further visibility errors the same way.

- [ ] **Step 6: Run the full existing launch test suite to confirm no behavior change**

Run: `cargo test --test launch`
Expected: all existing tests in `tests/launch.rs` PASS, unmodified.

- [ ] **Step 7: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add Cargo.toml Cargo.lock src/lib.rs src/launch/mod.rs src/cli/mod.rs
git commit -m "refactor: extract launch into its own module"
```

---

## Task 2: Graceful relaunch core (#1284)

**Files:**
- Modify: `src/launch/mod.rs` (add `RelaunchState`, wire `--auto-restart` and the
  restart loop into `run`)
- Modify: `src/cli/mod.rs:152-169` (`Command::Launch` — add `--auto-restart` flag)
- Modify: `src/cli/mod.rs:644-660` (pass the new flag through)
- Test: `tests/launch.rs` (new tests)

**Interfaces:**
- Produces: `pub(crate) struct RelaunchCap { attempts: Vec<std::time::Instant> }`
  with `fn record_and_check(&mut self, now: std::time::Instant) -> bool` (returns
  `true` if under the cap after recording this attempt, `false` if the cap is
  now exceeded). `const RELAUNCH_MAX_ATTEMPTS: usize = 3;`
  `const RELAUNCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);`
- Consumes: `launch::run`'s existing call to `run_supervised`/`spawn_and_supervise`
  from Task 1.

- [ ] **Step 1: Write the failing unit test for the restart cap**

Add to `src/launch/mod.rs`'s `#[cfg(test)] mod tests`:
```rust
#[test]
fn relaunch_cap_allows_up_to_the_configured_max_within_the_window() {
    let mut cap = RelaunchCap::default();
    let base = std::time::Instant::now();
    assert!(cap.record_and_check(base));
    assert!(cap.record_and_check(base + std::time::Duration::from_secs(1)));
    assert!(cap.record_and_check(base + std::time::Duration::from_secs(2)));
    // 4th attempt within the window exceeds the cap of 3.
    assert!(!cap.record_and_check(base + std::time::Duration::from_secs(3)));
}

#[test]
fn relaunch_cap_resets_once_attempts_age_out_of_the_window() {
    let mut cap = RelaunchCap::default();
    let base = std::time::Instant::now();
    assert!(cap.record_and_check(base));
    assert!(cap.record_and_check(base + std::time::Duration::from_secs(1)));
    assert!(cap.record_and_check(base + std::time::Duration::from_secs(2)));
    assert!(!cap.record_and_check(base + std::time::Duration::from_secs(3)));
    // A 5th attempt after the window has fully elapsed since the 1st: only
    // attempts 2-4 are still inside the window, so this is the window's 4th
    // — still over. After RELAUNCH_WINDOW has passed since attempt 1, it's
    // this test's job to prove the *old* attempts stop counting once they
    // age past the window boundary.
    let long_after = base + RELAUNCH_WINDOW + std::time::Duration::from_secs(1);
    assert!(cap.record_and_check(long_after));
}
```

- [ ] **Step 2: Run to verify it fails (type doesn't exist yet)**

Run: `cargo test -p llmenv --lib launch::tests::relaunch_cap -- --nocapture`
Expected: FAIL to compile — `RelaunchCap` not found.

- [ ] **Step 3: Implement `RelaunchCap`**

Add to `src/launch/mod.rs`:
```rust
const RELAUNCH_MAX_ATTEMPTS: usize = 3;
const RELAUNCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

/// Caps how many times `launch` will relaunch a crashing child within a
/// rolling window, so a child that crashes on every start doesn't loop
/// forever. Attempts older than [`RELAUNCH_WINDOW`] no longer count.
#[derive(Debug, Default)]
struct RelaunchCap {
    attempts: Vec<std::time::Instant>,
}

impl RelaunchCap {
    /// Record an attempt at `now` and report whether the cap still allows
    /// relaunching (i.e. this attempt was the `RELAUNCH_MAX_ATTEMPTS`-th or
    /// earlier within the window).
    fn record_and_check(&mut self, now: std::time::Instant) -> bool {
        self.attempts.retain(|t| now.duration_since(*t) < RELAUNCH_WINDOW);
        self.attempts.push(now);
        self.attempts.len() <= RELAUNCH_MAX_ATTEMPTS
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p llmenv --lib launch::tests::relaunch_cap -- --nocapture`
Expected: both tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/launch/mod.rs
git commit -m "feat(launch): add relaunch attempt cap"
```

- [ ] **Step 6: Write the failing integration test for crash detection + prompt**

Add to `tests/launch.rs`:
```rust
#[test]
fn launch_prompts_to_restart_after_a_crash() {
    let (dir, config_path) = setup_config();
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_EXIT_CODE", "1");
    cmd.write_stdin("n\n");
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .code(1)
        .stderr(predicates::str::contains("Restart?"));
}

#[test]
fn launch_auto_restart_relaunches_without_prompting() {
    let (dir, config_path) = setup_config();
    let mut cmd = launch_cmd_no_args(dir.path(), &config_path);
    cmd.arg("--auto-restart").arg("claude");
    cmd.env("FAKE_ENGINE_EXIT_CODE", "1");
    // The cap (3 attempts) is hit and launch gives up, reporting the last
    // crash's exit code rather than looping forever.
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .code(1)
        .stderr(predicates::str::contains("restart attempts exceeded"));
}
```

Add `predicates = "3"` to `[dev-dependencies]` in `Cargo.toml` if not already
present — check first with `rg '^predicates' Cargo.toml`.

- [ ] **Step 7: Run to verify both fail**

Run: `cargo test --test launch launch_prompts_to_restart -- --nocapture`
Run: `cargo test --test launch launch_auto_restart -- --nocapture`
Expected: both FAIL — no restart behavior exists yet, so the process exits
immediately with code 1 and no "Restart?"/"restart attempts exceeded" text.

- [ ] **Step 8: Add the `--auto-restart` flag**

In `src/cli/mod.rs`'s `Command::Launch` variant (lines 152-169), add a field:
```rust
        /// Relaunch the engine automatically after a crash, up to the
        /// restart-attempt cap, instead of prompting
        #[arg(long)]
        auto_restart: bool,
```

Update the match arm (lines 644-660) to destructure and pass it through:
```rust
        Some(Command::Launch {
            scope,
            tag,
            compress,
            engine,
            args,
            auto_restart,
        }) => {
            crate::launch::run(
                &engine,
                args,
                crate::launch::LaunchScope {
                    scope,
                    tag,
                    compress,
                    auto_restart,
                },
            )?;
        }
```

Add `auto_restart: bool` to `LaunchScope` in `src/launch/mod.rs`.

- [ ] **Step 9: Implement the relaunch loop in `launch::run`**

Replace the single `run_supervised` call in `run` with a loop:
```rust
pub(crate) fn run(engine: &str, args: Vec<String>, narrow: LaunchScope) -> anyhow::Result<()> {
    let adapter = crate::adapter::adapter_for_launch_target(engine).ok_or_else(|| {
        anyhow::anyhow!(
            "unrecognized engine '{engine}' — expected one of: {}",
            crate::adapter::registered_adapters()
                .iter()
                .map(|a| a.binary_name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let Some(bin_path) = crate::paths::resolve_on_path(adapter.binary_name()) else {
        anyhow::bail!(
            "'{bin}' not found on PATH — install it before running `llmenv launch {engine}`",
            bin = adapter.binary_name()
        );
    };

    let resolved = crate::cli::resolve_env(narrow.scope, narrow.tag, narrow.compress)?;
    let mut cap = RelaunchCap::default();

    loop {
        let mut cmd = crate::cli::command_at_path(&bin_path, adapter.binary_name());
        cmd.args(&args);
        for (key, value) in &resolved.vars {
            cmd.env(key, value);
        }
        cmd.stdin(std::process::Stdio::inherit());
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());

        let status = crate::cli::run_supervised(cmd, adapter.binary_name(), None)?;

        if status.success() {
            crate::cli::exit_with_status(status);
        }

        let reason = match status.signal() {
            Some(sig) => format!("terminated by signal {sig}"),
            None => format!("exited with code {}", status.code().unwrap_or(-1)),
        };
        eprintln!("llmenv: engine {reason}");

        if !cap.record_and_check(std::time::Instant::now()) {
            eprintln!("llmenv: restart attempts exceeded, giving up");
            crate::cli::exit_with_status(status);
        }

        if narrow.auto_restart {
            eprintln!("llmenv: auto-restarting");
            continue;
        }

        eprint!("Restart? [y/N] ");
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).unwrap_or(0) == 0
            || !answer.trim().eq_ignore_ascii_case("y")
        {
            crate::cli::exit_with_status(status);
        }
    }
}
```

Add `use std::os::unix::process::ExitStatusExt;` at the top of `src/launch/mod.rs`
for `.signal()`.

- [ ] **Step 10: Run to verify both integration tests pass**

Run: `cargo test --test launch launch_prompts_to_restart -- --nocapture`
Run: `cargo test --test launch launch_auto_restart -- --nocapture`
Expected: both PASS.

- [ ] **Step 11: Run the full existing launch suite to confirm no regression**

Run: `cargo test --test launch`
Expected: all PASS, including the pre-existing tests from before this task.

- [ ] **Step 12: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/cli/mod.rs src/launch/mod.rs tests/launch.rs Cargo.toml Cargo.lock
git commit -m "feat(launch): relaunch the engine after a crash (#1284)"
```

---

## Task 3: Per-session socket + `pending_events` verb

**Files:**
- Create: `src/launch/socket.rs`
- Modify: `src/launch/mod.rs` (bind the socket before spawning, set
  `LLMENV_LAUNCH_SOCKET`, run the accept loop alongside supervision, unlink
  on exit)
- Test: `src/launch/socket.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub(crate) type NoticeSlot = std::sync::Arc<tokio::sync::Mutex<Option<String>>>;`
  - `pub(crate) fn socket_path(pid: u32) -> anyhow::Result<std::path::PathBuf>`
  - `pub(crate) fn bind(pid: u32) -> anyhow::Result<(tokio::net::UnixListener, NoticeSlot, std::path::PathBuf)>`
  - `pub(crate) async fn serve(listener: tokio::net::UnixListener, notices: NoticeSlot)`
    (never returns under normal operation — a caller runs it as a spawned
    task and drops it, or wraps it in a `tokio::select!`, alongside supervision)
- Consumes: nothing from earlier tasks beyond `launch::run`'s existing structure.

- [ ] **Step 1: Write the failing unit test for the socket path**

Add to a new `src/launch/socket.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_uses_xdg_runtime_dir_when_set() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: test-only, single-threaded within this process's test harness
        // for env vars — matches the pattern `tests/support` already uses for
        // isolating LLMENV_CONFIG.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir.path()) };
        let path = socket_path(12345).unwrap();
        assert_eq!(path, dir.path().join("llmenv").join("launch-12345.sock"));
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv --lib launch::socket::tests -- --nocapture`
Expected: FAIL to compile — `socket_path` not found (module doesn't exist yet).

- [ ] **Step 3: Implement `socket_path` and `bind`**

```rust
//! Per-session Unix socket for `launch` (#1480): lets a background task
//! (drift watch, credential watch) deliver a one-line notice to the next
//! `hook_run` invocation the engine spawns, without `launch` owning the
//! child's stdio. See docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Longest request this server accepts — generous for a fixed one-verb
/// protocol, and small enough that a malformed/hostile client can't make the
/// server allocate an unbounded buffer.
const MAX_REQUEST_LEN: u32 = 4096;

/// Shared mailbox: `None` means nothing pending. A background task sets
/// `Some(text)`; the socket server takes it (clearing back to `None`) the
/// first time a client asks — exactly-once delivery.
pub(crate) type NoticeSlot = Arc<Mutex<Option<String>>>;

/// Path for this `launch` invocation's per-session socket. `pid` is
/// `launch`'s own pid, so the path is unique per session by construction.
///
/// # Errors
/// Returns an error when neither `XDG_RUNTIME_DIR` nor llmenv's state dir can
/// be resolved, or the directory can't be created.
pub(crate) fn socket_path(pid: u32) -> anyhow::Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("llmenv"),
        _ => crate::paths::state_dir()?,
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("launch-{pid}.sock")))
}

/// Bind the per-session socket, returning the listener, the notice mailbox
/// background tasks push into, and the bound path (for
/// `LLMENV_LAUNCH_SOCKET` and later cleanup).
///
/// # Errors
/// Returns an error when the path can't be resolved or the bind fails.
pub(crate) fn bind(pid: u32) -> anyhow::Result<(UnixListener, NoticeSlot, PathBuf)> {
    let path = socket_path(pid)?;
    // A stale file at this exact path would only exist if this pid was
    // reused since a prior `launch` crashed without tearing down — remove it
    // first so `bind` doesn't fail with "address in use".
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding launch socket at {}", path.display()))?;
    Ok((listener, Arc::new(Mutex::new(None)), path))
}

/// Accept connections until the caller drops this future (i.e. when
/// `launch`'s own supervision loop exits and stops polling it). Each
/// connection is handled on its own spawned task so one slow/malformed
/// client can't block the next.
pub(crate) async fn serve(listener: UnixListener, notices: NoticeSlot) {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!("launch: socket accept failed: {e:#}");
                continue;
            }
        };
        let notices = Arc::clone(&notices);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, notices).await {
                tracing::debug!("launch: socket connection failed: {e:#}");
            }
        });
    }
}

async fn handle_connection(mut stream: UnixStream, notices: NoticeSlot) -> anyhow::Result<()> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    anyhow::ensure!(len <= MAX_REQUEST_LEN, "launch socket request too large: {len} bytes");
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    let request: Request = serde_json::from_slice(&buf)?;

    let response = match request.verb.as_str() {
        "pending_events" => {
            let mut slot = notices.lock().await;
            Response { notice: slot.take() }
        }
        other => anyhow::bail!("unknown launch socket verb: {other}"),
    };

    let payload = serde_json::to_vec(&response)?;
    let len: u32 = payload.len().try_into().context("launch socket response too large")?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct Request {
    verb: String,
}

#[derive(serde::Serialize)]
struct Response {
    notice: Option<String>,
}
```

Add `mod socket;` to `src/launch/mod.rs`.

- [ ] **Step 4: Run to verify the path test passes**

Run: `cargo test -p llmenv --lib launch::socket::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Write the failing round-trip test**

Add to `src/launch/socket.rs`'s test module:
```rust
#[tokio::test]
async fn pending_events_delivers_a_queued_notice_exactly_once() {
    let (listener, notices, path) = bind(std::process::id()).unwrap();
    *notices.lock().await = Some("config changed".to_string());
    let server = tokio::spawn(serve(listener, notices));

    let first = fetch(&path).await;
    assert_eq!(first, Some("config changed".to_string()));

    let second = fetch(&path).await;
    assert_eq!(second, None, "a notice must not be delivered twice");

    server.abort();
    let _ = std::fs::remove_file(&path);
}

async fn fetch(path: &std::path::Path) -> Option<String> {
    let mut stream = UnixStream::connect(path).await.unwrap();
    let request = serde_json::to_vec(&Request { verb: "pending_events".to_string() }).unwrap();
    stream.write_all(&(request.len() as u32).to_be_bytes()).await.unwrap();
    stream.write_all(&request).await.unwrap();
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await.unwrap();
    let response: Response = serde_json::from_slice(&buf).unwrap();
    response.notice
}
```

Note `Request`/`Response` need `#[cfg_attr(test, derive(Clone))]`-free direct
construction to work from the test module — since the test module is a
submodule of `socket.rs`, it already has access to the private `Request`/
`Response` structs and their fields; add `pub(super)` … actually simplest: the
test module is `mod tests` inside the same file, so private fields are already
visible. No visibility change needed.

- [ ] **Step 6: Run to verify it fails**

Run: `cargo test -p llmenv --lib launch::socket::tests::pending_events -- --nocapture`
Expected: FAIL — likely a bind conflict or missing pieces before Step 3's
code exists; if Step 3 already landed, this specific test should compile and
initially fail only if the exactly-once logic is wrong. Since Step 3 already
implements `take()` correctly, this test is expected to PASS on first run —
run it anyway to confirm, per the task's own step ordering; if the assertions
already implemented in Step 3 are correct, treat this as verification rather
than a genuine red step, and proceed straight to Step 7.

- [ ] **Step 7: Confirm it passes**

Run: `cargo test -p llmenv --lib launch::socket::tests -- --nocapture`
Expected: both tests PASS.

- [ ] **Step 8: Wire the socket into `launch::run`**

In `src/launch/mod.rs`, `run` currently calls `run_supervised` synchronously
per loop iteration (Task 2). `run_supervised` builds its own single-threaded
tokio runtime per call — the socket needs to live *across* every relaunch
iteration (same `launch` pid, one socket for the whole session), so it can no
longer be bound inside the loop. Restructure `run` to build one runtime for
the whole function and bind the socket before the loop:

```rust
pub(crate) fn run(engine: &str, args: Vec<String>, narrow: LaunchScope) -> anyhow::Result<()> {
    let adapter = /* unchanged from Task 2 */;
    let bin_path = /* unchanged from Task 2 */;
    let resolved = crate::cli::resolve_env(narrow.scope, narrow.tag, narrow.compress)?;

    let (listener, notices, socket_path) = socket::bind(std::process::id())?;
    let _cleanup = SocketCleanup(socket_path.clone());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for launch")?;

    rt.block_on(async {
        tokio::spawn(socket::serve(listener, Arc::clone(&notices)));
        run_supervision_loop(engine, adapter.as_ref(), &bin_path, &args, &resolved, &socket_path, narrow.auto_restart).await
    })
}

/// Unlinks the per-session socket on every exit path via `Drop`, including a
/// panic unwind — the socket is the one artifact `launch` genuinely owns for
/// its own lifetime (see design doc "Teardown").
struct SocketCleanup(std::path::PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
```

Move the loop body from Task 2's `run` into a new `async fn run_supervision_loop`
with the same logic, plus setting `LLMENV_LAUNCH_SOCKET` on the child command:
```rust
cmd.env("LLMENV_LAUNCH_SOCKET", socket_path);
```
alongside the existing `resolved.vars` loop. `run_supervised` currently builds
its own runtime per call (Task 1's move kept it in `src/cli/mod.rs`
unchanged) — since `run` now already owns a runtime, call
`spawn_and_supervise` directly (it's already `async fn`, moved into
`src/launch/mod.rs` in Task 1) instead of going back through the
synchronous `crate::cli::run_supervised` wrapper, avoiding a nested-runtime
panic.

- [ ] **Step 9: Write the failing integration test for env propagation**

Add to `tests/launch.rs`:
```rust
#[test]
fn launch_sets_llmenv_launch_socket_in_child_env() {
    let (dir, config_path) = setup_config();
    let mut cmd = launch_cmd(dir.path(), &config_path);
    let env_dump = dir.path().join("env.txt");
    cmd.env("FAKE_ENGINE_ENV_DUMP", &env_dump);
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();
    let dumped = fs::read_to_string(&env_dump).unwrap();
    assert!(
        dumped.lines().any(|l| l.starts_with("LLMENV_LAUNCH_SOCKET=")),
        "child env missing LLMENV_LAUNCH_SOCKET:\n{dumped}"
    );
}
```

- [ ] **Step 10: Run to verify it fails, then implement, then verify it passes**

Run: `cargo test --test launch launch_sets_llmenv_launch_socket -- --nocapture`
Expected first: FAIL (env var not set yet, if Step 8 isn't done) or PASS
(if Step 8 already landed it) — implement Step 8 fully if not already, then:
Run again, expect PASS.

- [ ] **Step 11: Run the full launch suite for regressions**

Run: `cargo test --test launch`
Expected: all PASS, including Task 2's crash/restart tests (the socket must
not interfere with a relaunch — same bound socket persists across it).

- [ ] **Step 12: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/launch/mod.rs src/launch/socket.rs tests/launch.rs
git commit -m "feat(launch): add per-session socket with pending_events verb"
```

---

## Task 4: `hook_run` client-side check + `emit_hook_context` wiring

**Files:**
- Create: `src/hook_run/launch_client.rs`
- Modify: `src/hook_run/mod.rs:522-529` (append a pending notice, if any,
  before calling `emit_hook_context`)
- Test: `src/hook_run/launch_client.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub(crate) fn check_pending_notice() -> Option<String>`
- Consumes: `LLMENV_LAUNCH_SOCKET` env var (set by Task 3); the wire format
  from `src/launch/socket.rs` (`{"verb": "pending_events"}` request,
  `{"notice": ...}` response) — duplicated here deliberately rather than
  shared as a library type, since the two sides never need to agree on more
  than the JSON shape and a shared type would pull `src/launch` into
  `src/hook_run`'s dependency graph for a two-field wire format.

- [ ] **Step 1: Write the failing test for "no socket configured"**

Create `src/hook_run/launch_client.rs`:
```rust
//! Client side of `launch`'s per-session socket (#1480): checks for a
//! pending mid-session notice (config drift, credential expiry) on every
//! `hook_run` invocation. See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_when_env_var_is_unset() {
        // SAFETY: test-only; no other test in this process sets this var.
        unsafe { std::env::remove_var("LLMENV_LAUNCH_SOCKET") };
        assert_eq!(check_pending_notice(), None);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv --lib hook_run::launch_client::tests -- --nocapture`
Expected: FAIL to compile — `check_pending_notice` not found.

- [ ] **Step 3: Implement `check_pending_notice`**

```rust
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Budget for the whole connect-request-response round trip, matching the
/// v1 `launch` design's connect-then-fall-back guess.
const BUDGET: Duration = Duration::from_millis(50);

/// Check the resident `launch` process (if any) for a pending mid-session
/// notice. Returns `None` for every failure mode — no `LLMENV_LAUNCH_SOCKET`
/// set, no socket file, a connect/IO error, a timeout, or a malformed
/// response — this must never turn into a hook failure.
pub(crate) fn check_pending_notice() -> Option<String> {
    let path = std::env::var_os("LLMENV_LAUNCH_SOCKET")?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async move {
        tokio::time::timeout(BUDGET, fetch(path)).await.ok().flatten()
    })
}

async fn fetch(path: std::ffi::OsString) -> Option<String> {
    let mut stream = UnixStream::connect(path).await.ok()?;
    let request = serde_json::json!({ "verb": "pending_events" });
    let bytes = serde_json::to_vec(&request).ok()?;
    let len: u32 = bytes.len().try_into().ok()?;
    stream.write_all(&len.to_be_bytes()).await.ok()?;
    stream.write_all(&bytes).await.ok()?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await.ok()?;
    let response: serde_json::Value = serde_json::from_slice(&buf).ok()?;
    response.get("notice")?.as_str().map(str::to_owned)
}
```

Add `mod launch_client;` to `src/hook_run/mod.rs`.

- [ ] **Step 4: Run to verify the first test passes**

Run: `cargo test -p llmenv --lib hook_run::launch_client::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Write the failing end-to-end test against a real bound socket**

Add to `src/hook_run/launch_client.rs`'s test module:
```rust
#[tokio::test]
async fn delivers_a_queued_notice_from_a_real_socket() {
    let (listener, notices, path) = crate::launch::socket::bind(std::process::id() + 1).unwrap();
    *notices.lock().await = Some("credentials expire soon".to_string());
    let server = tokio::spawn(crate::launch::socket::serve(listener, notices));

    // SAFETY: test-only; this test owns this env var for its duration.
    unsafe { std::env::set_var("LLMENV_LAUNCH_SOCKET", &path) };
    let notice = tokio::task::spawn_blocking(check_pending_notice)
        .await
        .unwrap();
    unsafe { std::env::remove_var("LLMENV_LAUNCH_SOCKET") };

    assert_eq!(notice, Some("credentials expire soon".to_string()));
    server.abort();
    let _ = std::fs::remove_file(&path);
}
```

This requires `crate::launch::socket::bind`/`serve`/`NoticeSlot` to be
`pub(crate)` and reachable from `hook_run` — they already are (Task 3 made
them `pub(crate)` at the crate root's `launch` module), so no visibility
change is needed. `check_pending_notice` builds its own current-thread
runtime internally, so it can't be called directly from an already-async
`#[tokio::test]` (`Builder::new_current_thread().build()` inside a running
runtime works fine — it's a separate runtime instance — but calling
`rt.block_on` from within another runtime's worker thread panics with
"Cannot start a runtime from within a runtime". Route through
`spawn_blocking`, which runs on a separate thread pool where building and
blocking on a fresh runtime is safe.

- [ ] **Step 6: Run to verify it fails, then confirm it passes**

Run: `cargo test -p llmenv --lib hook_run::launch_client::tests -- --nocapture`
Expected: PASS (Step 3's implementation already covers this path; this step
confirms the real, non-mocked round trip works end-to-end through both
modules).

- [ ] **Step 7: Wire the notice into `hook_run::run`'s existing render call**

In `src/hook_run/mod.rs`, the current code at lines 522-529:
```rust
            } else {
                let out = adapter.emit_hook_context(hook_event_name, &text);
                if !out.is_empty()
                    && let Err(e) = writeln!(std::io::stdout(), "{out}")
                    && e.kind() != std::io::ErrorKind::BrokenPipe
                {
                    eprintln!("llmenv: failed to write hook output: {e}");
                }
            }
```
becomes:
```rust
            } else {
                let mut text = text;
                if let Some(notice) = launch_client::check_pending_notice() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&notice);
                }
                let out = adapter.emit_hook_context(hook_event_name, &text);
                if !out.is_empty()
                    && let Err(e) = writeln!(std::io::stdout(), "{out}")
                    && e.kind() != std::io::ErrorKind::BrokenPipe
                {
                    eprintln!("llmenv: failed to write hook output: {e}");
                }
            }
```

- [ ] **Step 8: Run the full hook_run test suite for regressions**

Run: `cargo test -p llmenv --lib hook_run::`
Expected: all existing `hook_run` unit tests PASS unmodified — this change is
additive (an empty `check_pending_notice()` result, the common case with no
`launch` session active, changes nothing about `text`).

- [ ] **Step 9: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/hook_run/mod.rs src/hook_run/launch_client.rs
git commit -m "feat(hook-run): deliver launch's pending mid-session notice"
```

---

## Task 5: Config-drift detection (#1286)

**Files:**
- Create: `src/launch/drift.rs`
- Modify: `src/launch/mod.rs` (spawn the drift-watch task alongside the
  socket server and supervision loop)
- Test: `src/launch/drift.rs`, `tests/launch.rs`

**Interfaces:**
- Produces: `pub(crate) async fn watch(baseline_hash: String, config_path: std::path::PathBuf, notices: crate::launch::socket::NoticeSlot, interval: std::time::Duration)`
  (loops forever on `interval`; a caller spawns it as a task alongside
  `socket::serve` and drops/aborts it when the session ends — `interval` is a
  parameter, not a hardcoded constant, so a test can pass a short one instead
  of adding an env-var test seam to production code).
  `pub(crate) const DRIFT_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);`
- Consumes: `crate::materialize::cache::hash_manifest`, `crate::cli::stale_status`,
  `crate::launch::socket::NoticeSlot` (Task 3).

- [ ] **Step 1: Write the failing unit test for the comparison logic**

Add to `src/launch/drift.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_drifted_is_true_when_current_hash_differs_from_baseline() {
        assert!(has_drifted("abc", "def"));
    }

    #[test]
    fn has_drifted_is_false_when_current_hash_matches_baseline() {
        assert!(!has_drifted("abc", "abc"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv --lib launch::drift::tests -- --nocapture`
Expected: FAIL to compile — `has_drifted` not found.

- [ ] **Step 3: Implement the drift-watch module**

```rust
//! Config-drift watch for `launch` (#1286): a session-scoped comparison
//! against the config `launch` resolved at startup, independent of the
//! `SessionStart`-only, Claude-Code-only check `hook_run::should_check_stale`
//! already performs (that one predates `launch` and doesn't cover drift
//! *during* an active session). See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

use std::path::PathBuf;
use std::time::Duration;

use crate::launch::socket::NoticeSlot;

pub(crate) const DRIFT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

const DRIFT_NOTICE: &str =
    "llmenv config changed since this session started; restart to pick up changes.";

/// Whether `current`'s content hash differs from the session's `baseline`.
fn has_drifted(baseline: &str, current: &str) -> bool {
    baseline != current
}

/// Recompute the current config's content hash the same way `run_check_stale`
/// does, reusing its manifest-building pipeline rather than a second
/// implementation. Returns `Ok(None)` when there's no content to
/// materialize (mirrors `run_check_stale`'s own "not drifted" case for an
/// empty config).
fn current_hash(config_path: &std::path::Path) -> anyhow::Result<Option<String>> {
    let config = crate::config::Config::load(config_path)?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);
    let firing = crate::cli::firing_bundles(&config.bundle, &active, None);
    match crate::cli::build_manifest(&config, config_dir, &active, &firing, false)? {
        Some((manifest, _)) => Ok(Some(crate::materialize::cache::hash_manifest(&manifest)?)),
        None => Ok(None),
    }
}

/// Poll for config drift every `interval` and queue a notice the first time
/// it's detected. Runs until the caller drops/aborts this task. Fail-soft:
/// an error recomputing the hash is logged and retried next interval, never
/// surfaced as a session failure.
pub(crate) async fn watch(
    baseline_hash: String,
    config_path: PathBuf,
    notices: NoticeSlot,
    interval: Duration,
) {
    let mut interval = tokio::time::interval(interval);
    let mut already_notified = false;
    loop {
        interval.tick().await;
        if already_notified {
            continue;
        }
        let path = config_path.clone();
        let current = match tokio::task::spawn_blocking(move || current_hash(&path)).await {
            Ok(Ok(hash)) => hash,
            Ok(Err(e)) => {
                tracing::debug!("launch: drift check failed: {e:#}");
                continue;
            }
            Err(e) => {
                tracing::debug!("launch: drift check task panicked: {e:#}");
                continue;
            }
        };
        let Some(current) = current else { continue };
        if has_drifted(&baseline_hash, &current) {
            *notices.lock().await = Some(DRIFT_NOTICE.to_string());
            already_notified = true;
        }
    }
}
```

`crate::cli::firing_bundles`/`crate::cli::build_manifest` are currently
private in `src/cli/mod.rs` (used only by `run_check_stale`) — change both to
`pub(crate) fn`. Add `mod drift;` to `src/launch/mod.rs`.

- [ ] **Step 4: Run to verify the unit tests pass**

Run: `cargo test -p llmenv --lib launch::drift::tests -- --nocapture`
Expected: both PASS.

- [ ] **Step 5: Wire drift-watch into `launch::run`**

In `run_supervision_loop` (Task 3, Step 8), compute the baseline hash once
before the loop starts (reusing `current_hash`, ignoring a `None`/error
baseline — no baseline means nothing to compare against, matching
`run_check_stale`'s own "Unknown" case) and spawn the watch task alongside
`socket::serve`:
```rust
    if let Ok(Some(baseline)) = drift::current_hash(&config_path) {
        tokio::spawn(drift::watch(
            baseline,
            config_path.clone(),
            Arc::clone(&notices),
            drift::DRIFT_CHECK_INTERVAL,
        ));
    }
```
`config_path` needs to be threaded into `run_supervision_loop`'s parameters —
`resolve_env` (called in `run` before entering the async block) doesn't
currently expose the path it loaded; get it via `crate::paths::config_path()?`
in `run`, alongside the existing `resolve_env` call, and pass it down.

- [ ] **Step 6: Write the failing async unit test for the watch loop**

Add to `src/launch/drift.rs`'s test module. This drives `watch` directly with
a short interval rather than spawning a whole `launch` process — faster and
doesn't depend on fake-engine timing:
```rust
#[tokio::test]
async fn watch_queues_a_notice_once_the_config_changes() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.yaml");
    std::fs::write(&config_path, "scope:\n  user: []\n").unwrap();
    let baseline = current_hash(&config_path).unwrap().unwrap();

    let notices: NoticeSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let handle = tokio::spawn(watch(
        baseline,
        config_path.clone(),
        std::sync::Arc::clone(&notices),
        Duration::from_millis(20),
    ));

    // No change yet: nothing queued after a couple of ticks.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(*notices.lock().await, None);

    std::fs::write(&config_path, "scope:\n  user:\n    - id: x\n").unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        notices.lock().await.as_deref() == Some(DRIFT_NOTICE),
        "expected a drift notice to be queued after the config changed"
    );

    handle.abort();
}
```

- [ ] **Step 7: Run to verify it fails, then confirm it passes**

Run: `cargo test -p llmenv --lib launch::drift::tests::watch_queues -- --nocapture`
Expected: FAIL until Step 5's `watch` implementation and signature (Step 3)
are both in place; PASS after — Step 3 already implements the loop this
test exercises, so this step is largely confirmation that the real,
un-mocked file-write-to-detected-notice path works end to end within the
module.

- [ ] **Step 8: Run the full launch suite for regressions**

Run: `cargo test --test launch`
Expected: all PASS.

- [ ] **Step 9: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/launch/mod.rs src/launch/drift.rs src/cli/mod.rs tests/launch.rs
git commit -m "feat(launch): notice config drift during an active session (#1286)"
```

---

## Task 6: Credential-expiry detection and notice (#1285, narrowed scope)

**Files:**
- Create: `src/launch/credential_watch.rs`
- Modify: `src/launch/mod.rs` (spawn the credential-watch task, Claude Code
  sessions only — this is Claude Code's OAuth cache, the only credential
  cache that currently exists)
- Test: `src/launch/credential_watch.rs`, `tests/launch.rs`

**Interfaces:**
- Produces: `pub(crate) fn is_near_expiry(creds: &crate::auth::Credentials, threshold: std::time::Duration, now_unix_ms: i64) -> bool`
  `pub(crate) async fn watch(adapter_root: std::path::PathBuf, notices: crate::launch::socket::NoticeSlot, interval: std::time::Duration)`
  (`interval` is a parameter, not a hardcoded constant, so a test can pass a
  short one instead of adding an env-var test seam to production code).
  `pub(crate) const EXPIRY_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);`
  `pub(crate) const EXPIRY_WARNING_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(300);`
- Consumes: `crate::auth::credentials::load_cached`, `crate::auth::credentials::save_cached`
  (test setup — writes a fixture through the same code path production uses
  to read it, so the test never has to know the cache's on-disk layout),
  `Credentials::expires_at`, `Credentials::is_expired_now` (all already
  `pub(crate)`), `crate::launch::socket::NoticeSlot`.

Confirm `crate::auth::Credentials` and `crate::auth::credentials::load_cached`
are reachable from `src/launch/` — `src/auth/mod.rs` currently exposes
`credentials` as a submodule; check its visibility (`mod credentials;` vs
`pub(crate) mod credentials;`) and widen it to `pub(crate)` if needed, same
adjustment as Task 1 Step 5 made for `cli`.

- [ ] **Step 1: Write the failing unit tests for the threshold check**

Add to `src/launch/credential_watch.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn creds_expiring_at(expires_at_ms: i64) -> crate::auth::Credentials {
        crate::auth::Credentials::from_json(serde_json::json!({
            "claudeAiOauth": { "accessToken": "x", "expiresAt": expires_at_ms }
        }))
        .unwrap()
    }

    #[test]
    fn is_near_expiry_true_when_inside_the_threshold_window() {
        let now = 1_000_000_000_000_i64;
        let creds = creds_expiring_at(now + 60_000); // 60s from now
        assert!(is_near_expiry(&creds, EXPIRY_WARNING_THRESHOLD, now));
    }

    #[test]
    fn is_near_expiry_false_when_outside_the_threshold_window() {
        let now = 1_000_000_000_000_i64;
        let creds = creds_expiring_at(now + 3_600_000); // 1h from now
        assert!(!is_near_expiry(&creds, EXPIRY_WARNING_THRESHOLD, now));
    }

    #[test]
    fn is_near_expiry_true_when_already_expired() {
        let now = 1_000_000_000_000_i64;
        let creds = creds_expiring_at(now - 1_000);
        assert!(is_near_expiry(&creds, EXPIRY_WARNING_THRESHOLD, now));
    }
}
```

Check `crate::auth::Credentials::from_json`'s exact visibility
(`pub(crate) fn from_json` per `src/auth/credentials.rs:108` — already
crate-visible) and `Credentials`'s own visibility (`pub struct Credentials`
per line 77 — already fine to construct/use from `src/launch/`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llmenv --lib launch::credential_watch::tests -- --nocapture`
Expected: FAIL to compile — `is_near_expiry` not found.

- [ ] **Step 3: Implement the credential-watch module**

```rust
//! Credential-expiry detection for `launch` (#1285, narrowed scope): notices
//! when the cached Claude Code OAuth credential is close to expiry.
//! Detection and notice only — llmenv has no OAuth refresh call of its own
//! today (Claude Code performs its own refresh; llmenv only caches the
//! result). See
//! docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md.

use std::path::PathBuf;
use std::time::Duration;

use crate::auth::Credentials;
use crate::launch::socket::NoticeSlot;

pub(crate) const EXPIRY_CHECK_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const EXPIRY_WARNING_THRESHOLD: Duration = Duration::from_secs(300);

const EXPIRY_NOTICE: &str =
    "credentials expire soon; run `llmenv login` if the engine reports an auth failure.";

/// Whether `creds` expires within `threshold` of `now_unix_ms`, or has
/// already expired. `now_unix_ms` is a parameter (not read internally) so
/// this stays a pure function the unit tests can drive directly.
pub(crate) fn is_near_expiry(creds: &Credentials, threshold: Duration, now_unix_ms: i64) -> bool {
    let Some(expires_at) = creds.expires_at() else {
        return false;
    };
    let threshold_ms: i64 = threshold.as_millis().try_into().unwrap_or(i64::MAX);
    expires_at.saturating_sub(now_unix_ms) <= threshold_ms
}

/// Poll the cached credential every `interval` and queue a notice the first
/// time it's inside the warning threshold. Runs until the caller
/// drops/aborts this task. Fail-soft: a read error or an absent cache is
/// treated as "nothing to warn about," not an error — most sessions have no
/// cached credential at all (e.g. an API-key-only setup), and that is not
/// itself a problem.
pub(crate) async fn watch(adapter_root: PathBuf, notices: NoticeSlot, interval: Duration) {
    let mut interval = tokio::time::interval(interval);
    let mut already_notified = false;
    loop {
        interval.tick().await;
        if already_notified {
            continue;
        }
        let root = adapter_root.clone();
        let creds = match tokio::task::spawn_blocking(move || {
            crate::auth::credentials::load_cached(&root)
        })
        .await
        {
            Ok(Ok(Some(creds))) => creds,
            Ok(Ok(None)) => continue,
            Ok(Err(e)) => {
                tracing::debug!("launch: credential expiry check failed: {e:#}");
                continue;
            }
            Err(e) => {
                tracing::debug!("launch: credential expiry check task panicked: {e:#}");
                continue;
            }
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "current time in ms fits i64 until the year 292278994"
        )]
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if is_near_expiry(&creds, EXPIRY_WARNING_THRESHOLD, now_unix_ms) {
            *notices.lock().await = Some(EXPIRY_NOTICE.to_string());
            already_notified = true;
        }
    }
}
```

Add `mod credential_watch;` to `src/launch/mod.rs`. Check
`crate::auth::Credentials::expires_at`'s visibility
(`pub(crate) fn expires_at(&self) -> Option<i64>` per
`src/auth/credentials.rs:141` — already crate-visible) — no change needed.

- [ ] **Step 4: Run to verify the unit tests pass**

Run: `cargo test -p llmenv --lib launch::credential_watch::tests -- --nocapture`
Expected: all three PASS.

- [ ] **Step 5: Wire credential-watch into `launch::run`**

In `run_supervision_loop`, only for a Claude Code session (the only engine
with a credential cache today — mirrors `should_check_stale`'s own
Claude-Code-only gate for the same reason):
```rust
    if adapter.name() == "claude-code" {
        let adapter_root = crate::paths::cache_dir()?.join(adapter.name());
        tokio::spawn(credential_watch::watch(
            adapter_root,
            Arc::clone(&notices),
            credential_watch::EXPIRY_CHECK_INTERVAL,
        ));
    }
```
Place this alongside the drift-watch spawn from Task 5, Step 5.

- [ ] **Step 6: Write the failing async unit test for the watch loop**

Add to `src/launch/credential_watch.rs`'s test module. This writes the
fixture through `save_cached` (the same function production code pairs with
`load_cached`) rather than guessing the cache's on-disk layout, and drives
`watch` directly with a short interval:
```rust
#[tokio::test]
async fn watch_queues_a_notice_for_a_soon_to_expire_credential() {
    let dir = tempfile::tempdir().unwrap();
    let adapter_root = dir.path().join("claude-code");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let creds = creds_expiring_at(now_ms + 10_000);
    crate::auth::credentials::save_cached(&adapter_root, &creds).unwrap();

    let notices: NoticeSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let handle = tokio::spawn(watch(
        adapter_root,
        std::sync::Arc::clone(&notices),
        Duration::from_millis(20),
    ));

    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        notices.lock().await.as_deref(),
        Some(EXPIRY_NOTICE),
        "expected an expiry notice to be queued"
    );

    handle.abort();
}
```

- [ ] **Step 7: Run to verify it fails, then confirm it passes**

Run: `cargo test -p llmenv --lib launch::credential_watch::tests::watch_queues -- --nocapture`
Expected: FAIL until Step 5's `watch` signature (Step 3) is in place; PASS
after.

- [ ] **Step 8: Run the full launch and auth suites for regressions**

Run: `cargo test --test launch`
Run: `cargo test -p llmenv --lib auth::`
Expected: all PASS.

- [ ] **Step 9: Format, lint, commit**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
git add src/launch/mod.rs src/launch/credential_watch.rs src/auth/mod.rs tests/launch.rs
git commit -m "feat(launch): notice a soon-to-expire credential (#1285)"
```

---

## Task 7: Docs + changelog

**Files:**
- Modify: `website/docs/` — find `launch`'s existing page (`rg -l "llmenv launch" website/docs/`)
  and add sections for: `--auto-restart` and the restart-attempt cap, the
  config-drift notice, the credential-expiry notice (state plainly that this
  is detection-only, not silent refresh).
- Modify: `CHANGELOG.md` — three `### Added` entries under `[Unreleased]`,
  one per issue, each tagged with the version this ships in (check
  `Cargo.toml`'s current `[Unreleased]` heading — do not invent a version
  number; follow `RELEASING.md`).

**Interfaces:** none — documentation only.

- [ ] **Step 1: Locate and read the existing `launch` docs page**

Run: `rg -l "llmenv launch" website/docs/`
Read the file(s) found to match its existing structure/tone before editing.

- [ ] **Step 2: Add the three new behaviors to the docs page**

Follow AGENTS.md's hard rule: tag each new section with
`(added in v4.0.0)` (confirm this is still the correct target version by
checking `CHANGELOG.md`'s current `[Unreleased]` section and `Cargo.toml`'s
version at the time this task runs — this plan was written assuming v4.0.0
per the composite's milestone, but re-verify rather than trust this comment
if time has passed).

Document, in plain language:
- `--auto-restart`, the default restart cap (3 attempts per 5-minute
  window), and what happens once the cap is hit (final error, no more
  retries).
- The config-drift notice: what triggers it, that it only warns (never
  auto-applies), and that it appears in the agent's context on its next
  turn rather than instantly.
- The credential-expiry notice: state explicitly that llmenv does not
  silently refresh the credential — the user still runs `llmenv login`
  manually. Link or reference the follow-up issue filed in dev-sprint's
  Phase 6 for actual silent refresh (fill in the real issue number once
  filed — this plan doesn't invent one).

- [ ] **Step 3: Invoke the `keepachangelog` skill**

Use the skill (don't hand-write the format from memory) to add three entries
under `## [Unreleased]` in `CHANGELOG.md`:
- Added: `launch` relaunches the engine after a crash, with `--auto-restart`
  and a restart-attempt cap (#1284).
- Added: `launch` notices a config change made while a session is running
  and warns in the agent's context (#1286).
- Added: `launch` notices a soon-to-expire cached credential and warns in
  the agent's context (#1285).

Link each entry to the docs page from Step 2 per AGENTS.md's changelog
linking convention.

- [ ] **Step 4: Verify docs build**

Run: `cd website && npm run build`
Expected: succeeds with no broken links/build errors.

- [ ] **Step 5: Commit**

```bash
cargo fmt --check  # confirm no drift from earlier steps
git add website/docs/ CHANGELOG.md
git commit -m "docs: document launch mid-session supervision (#1284, #1285, #1286)"
```

---

## Final verification (before handing off to nbl-dev:ship-issue's pre-pr-review)

- [ ] `cargo test --workspace`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo fmt --check`
- [ ] `cd website && npm run build`
- [ ] Re-read `docs/superpowers/specs/2026-08-23-launch-mid-session-supervision-design.md`
      against the final diff — confirm every "Decided direction" and
      "Per-issue application" item has a corresponding task above, and that
      nothing drifted from the spec during implementation without a
      documented reason.
