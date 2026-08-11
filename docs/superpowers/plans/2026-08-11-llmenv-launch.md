# `llmenv launch <engine>` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:executing-plans` to
> implement this plan task-by-task. **Do NOT use `superpowers:subagent-driven-development`**
> — repo policy (this user's global CLAUDE.md) overrides that skill's own
> recommendation; work each task inline in the current session instead. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `llmenv launch <engine> [-- <args>...]`, which resolves the environment
(reusing `export`'s existing pipeline) and then spawns the target engine (`claude`,
`crush`, or `opencode`) as a **supervised child process** — inherited stdio, forwarded
signals, propagated exit code — instead of the ambient shell-hook model.

**Architecture:** Two additive changes to the existing `llmenv` binary, no new crate.
(1) `run_export`'s resolution logic is extracted into a `resolve_env` function so
`export` and the new `launch` share one code path with zero duplication. (2) A new
`Command::Launch` CLI arm resolves the target engine via a new
`adapter_for_launch_target` lookup, resolves env via `resolve_env`, spawns the engine
with `tokio::process::Command` (inherited stdio, resolved env layered onto the
inherited environment), and supervises it: SIGINT/SIGTERM/SIGHUP are received and
ignored (the terminal already delivers them to the child directly, same process
group), and `launch`'s own exit code mirrors the child's (its exit code, or
`128 + signum` if it died by signal).

**Tech Stack:** Rust 2024 edition, existing `llmenv` binary crate (`src/`), `clap`
derive CLI, `tokio` (adding the `signal` feature to the existing dependency —
no new crate), `assert_cmd` + `tempfile` for integration tests (existing dev-deps).

## Global Constraints

- Workspace lints (`Cargo.toml:23-29`): `unsafe_code = "forbid"`,
  `clippy::unwrap_used = "deny"`, `clippy::expect_used = "deny"`,
  `clippy::panic = "deny"`. Every fallible call in new code uses `?`/`.context()`/
  `.with_context()` — never `.unwrap()`/`.expect()` outside test files (test files
  carry their own `#![expect(clippy::unwrap_used, reason = "test scaffolding")]`,
  matching the existing convention in `tests/smoke_suite.rs:1`).
- `cargo fmt` runs automatically as a pre-commit hook (confirmed by the `cargo fmt
  --check` step already observed in this repo's commit hook output) — no manual
  `cargo fmt` step is listed per task, but if a commit is rejected for formatting,
  run `cargo fmt` and re-commit.
- Every new `pub`/`pub(crate)` function with non-obvious behavior gets a doc
  comment, matching the existing style throughout `src/adapter/mod.rs` and
  `src/cli/mod.rs`.
- Every user-facing change needs a `CHANGELOG-3.md` entry under `[Unreleased]` and
  matching `website/docs/` coverage (`AGENTS.md` hard rule) — handled in Task 7.
- Rust edition 2024, existing `anyhow::Result` error-handling convention at the
  CLI-command level (`src/main.rs:75-78` converts any `Err` from `cli::run()` into
  `eprintln!("llmenv: {e:#}"); std::process::exit(1);` — new code doesn't need its
  own top-level error printing, just propagate `anyhow::Result` up).

---

## File Structure

- **Modify `Cargo.toml:62`** — add `"signal"` to the `tokio` feature list (no new
  dependency; `tokio` is already pinned at `=1.53.1`).
- **Modify `src/adapter/mod.rs`** — add `adapter_for_launch_target`, a new lookup
  function alongside the existing `adapter_for_engine`/`active_adapter` (same file,
  same section, ~line 314).
- **Modify `src/cli/mod.rs`** — add the `Launch` variant to the `Command` enum
  (alongside `Export`, ~line 133), add its dispatch arm (alongside the `Export` arm,
  ~line 571), extract `resolve_env`/`ResolvedEnv` out of `run_export` (~lines
  957-1289), and add `run_launch` + `exit_with_status` + `supervise_child`.
- **Create `tests/launch.rs`** — integration tests for `launch`, following the
  `tests/smoke_suite.rs` pattern (`support::isolated_llmenv_cmd`,
  `assert_cmd`/`tempfile`).
- **Create `tests/fixtures/fake_engine.sh`** — a tiny shell script that stands in for
  `claude`/`crush`/`opencode` in tests: dumps its environment to a file, sleeps if
  asked, and exits with a configurable code — so `launch` tests don't depend on a
  real engine binary being installed in CI.
- **Modify `website/docs/commands.md`** — new `## launch` section (Task 7).
- **Modify `CHANGELOG-3.md`** — new entry under `[Unreleased]` → `### Added`
  (Task 7).
- **Regenerate `website/docs/changelog.md`** via `scripts/sync-changelog-doc.sh`
  (Task 7) — this file is generated, never hand-edited (see its own header comment).

---

### Task 1: Enable tokio's `signal` feature

**Files:**
- Modify: `Cargo.toml:62`

**Interfaces:**
- Produces: `tokio::signal::unix::{signal, SignalKind}` becomes available for Task 5.

- [ ] **Step 1: Add the feature**

Change `Cargo.toml:62` from:

```toml
tokio = { version = "=1.53.1", features = ["rt-multi-thread", "macros", "fs", "process", "io-util", "sync"] }
```

to:

```toml
tokio = { version = "=1.53.1", features = ["rt-multi-thread", "macros", "fs", "process", "io-util", "sync", "signal"] }
```

- [ ] **Step 2: Verify it builds**

Run: `cargo check`
Expected: succeeds (this step only changes the dependency's enabled features, no
code uses it yet).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: enable tokio signal feature for llmenv launch"
```

---

### Task 2: `adapter_for_launch_target`

**Files:**
- Modify: `src/adapter/mod.rs` (add function near `adapter_for_engine`, ~line 314;
  add tests near the existing adapter-registry tests, ~line 630+)

**Interfaces:**
- Consumes: `registered_adapters() -> Vec<Box<dyn AgentAdapter>>` (existing,
  `src/adapter/mod.rs:299`), `AgentAdapter::binary_name(&self) -> &'static str`
  (existing trait method), `engine_id(adapter: &dyn AgentAdapter) -> String`
  (existing, `src/adapter/mod.rs:327`).
- Produces: `pub fn adapter_for_launch_target(target: &str) -> Option<Box<dyn AgentAdapter>>`
  — used by Task 4's `run_launch`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module in `src/adapter/mod.rs` (near the existing
`registered_adapters`/`engine_id` tests around line 630):

```rust
#[test]
fn adapter_for_launch_target_matches_binary_name() {
    let a = adapter_for_launch_target("claude").expect("binary name 'claude' should resolve");
    assert_eq!(a.binary_name(), "claude");
}

#[test]
fn adapter_for_launch_target_matches_engine_id() {
    let a =
        adapter_for_launch_target("claude_code").expect("engine id 'claude_code' should resolve");
    assert_eq!(a.binary_name(), "claude");
}

#[test]
fn adapter_for_launch_target_matches_other_adapters() {
    assert_eq!(
        adapter_for_launch_target("crush").expect("crush").binary_name(),
        "crush"
    );
    assert_eq!(
        adapter_for_launch_target("opencode").expect("opencode").binary_name(),
        "opencode"
    );
}

#[test]
fn adapter_for_launch_target_rejects_unknown_target() {
    assert!(
        adapter_for_launch_target("__llmenv_no_such_engine_xyzzy__").is_none(),
        "an unrecognized engine must not silently resolve to any adapter"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib adapter_for_launch_target -- --exact`
Expected: FAIL to compile — `adapter_for_launch_target` is not defined yet.

- [ ] **Step 3: Implement `adapter_for_launch_target`**

Add just after `adapter_for_engine` (`src/adapter/mod.rs:314-318`):

```rust
/// Resolve an adapter for `llmenv launch <target>`, matching `target` against
/// either the adapter's binary name (what a user types on the command line,
/// e.g. `claude`) or its engine id (the underscore form, e.g. `claude_code`).
///
/// Unlike [`adapter_for_engine`], this returns `None` instead of silently
/// falling back to env-sniffing on no match: `launch` must error loudly on an
/// unrecognized engine rather than risk launching the wrong binary.
#[must_use]
pub fn adapter_for_launch_target(target: &str) -> Option<Box<dyn AgentAdapter>> {
    registered_adapters()
        .into_iter()
        .find(|a| a.binary_name() == target || engine_id(a.as_ref()) == target)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib adapter_for_launch_target`
Expected: all 4 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/adapter/mod.rs
git commit -m "feat(adapter): add adapter_for_launch_target lookup"
```

---

### Task 3: Extract `resolve_env` out of `run_export`

This is a behavior-preserving refactor with one deliberate, documented improvement:
today, `run_export`'s print loop validates each variable name/value **as it prints**
(`src/cli/mod.rs:1262-1286`), so an invalid variable discovered partway through can
leave some `export KEY=VALUE` lines already printed before the process bails
nonzero — partial, malformed output on error. This task moves validation to run
**before** anything is returned/printed, so a validation failure now produces zero
output instead of partial output. No other behavior changes; `export`'s existing
tests are the regression check.

**Files:**
- Modify: `src/cli/mod.rs:957-1289` (the full current body of `run_export`)

**Interfaces:**
- Consumes: everything `run_export` already consumes (`paths::config_path`,
  `hook_run::load_cached_config`, `scope::matcher::Env::detect`, `scope::evaluate`,
  `firing_bundles`, `build_manifest`, `installed_adapters`, `materialize_from_manifest`,
  `validate_var_name`, `validate_var_value` — all existing, unchanged).
- Produces: `struct ResolvedEnv { vars: BTreeMap<String, String>, firing_bundle_names: Vec<String> }`
  and `fn resolve_env(scope: Option<String>, tag: Option<String>, compress: bool) -> anyhow::Result<ResolvedEnv>`
  — consumed by both the refactored `run_export` and, in Task 4, `run_launch`.

- [ ] **Step 1: Confirm the regression baseline passes before touching anything**

Run: `cargo test --test smoke_suite -- export`
Expected: PASS (this establishes the "still works" baseline you'll re-check after
the refactor — every test whose name contains `export`, e.g.
`smoke_claude_code_basic_export`, `smoke_crush_basic_export`).

- [ ] **Step 2: Add the `ResolvedEnv` struct and extract `resolve_env`**

Just above `fn run_export` (`src/cli/mod.rs:957`), add:

```rust
/// The result of resolving the environment for the active scope, before it's
/// either printed as `export` lines (`run_export`) or applied to a supervised
/// child process (`run_launch`). Shared by both so there is exactly one
/// resolution code path (#1056).
struct ResolvedEnv {
    /// Every variable llmenv would export, in deterministic (`BTreeMap`) order.
    vars: std::collections::BTreeMap<String, String>,
    /// Names of the bundles that fired, for `export --explain`'s
    /// `# source: adapter (bundles: ...)` annotation.
    firing_bundle_names: Vec<String>,
}
```

Then change `fn run_export`'s signature and body: rename it to `resolve_env`,
change its return type, cut everything from the current line 963
(`let config_path = paths::config_path()?;`) through line 1260
(`crate::memory::prune::auto_prune_if_enabled(&config);`) **verbatim** — every line
of that block is unchanged — and append validation + the return:

```rust
fn resolve_env(
    scope: Option<String>,
    tag: Option<String>,
    compress: bool,
) -> anyhow::Result<ResolvedEnv> {
    // <-- everything currently at src/cli/mod.rs:963-1260, unchanged, goes here.
    //     This ends right after the `crate::memory::prune::auto_prune_if_enabled(&config);`
    //     line — do not move the `if explain { ... } else { ... }` block (that
    //     stays in run_export, see Step 3) or the trailing `Ok(())`.

    // Validate every variable before returning any of them — a failure here
    // must produce zero output, not the partial output the old inline
    // per-print validation could leave behind.
    for (key, value) in &vars {
        validate_var_name(key).with_context(|| format!("variable '{key}'"))?;
        validate_var_value(value).with_context(|| format!("variable '{key}': invalid value"))?;
    }

    Ok(ResolvedEnv {
        vars,
        firing_bundle_names: bundles_for_icm,
    })
}
```

(`bundles_for_icm` is the `Vec<String>` already computed at the old line 1250 —
`firing.iter().map(|b| b.name.clone()).collect::<Vec<_>>()` — reused here instead of
being recomputed, since it's exactly the bundle-name list `--explain` needs.)

- [ ] **Step 3: Rewrite `run_export` to call `resolve_env`**

Add a new `run_export` right after `resolve_env`:

```rust
fn run_export(
    scope: Option<String>,
    tag: Option<String>,
    explain: bool,
    compress: bool,
) -> anyhow::Result<()> {
    let resolved = resolve_env(scope, tag, compress)?;

    if explain {
        let bundle_list = resolved.firing_bundle_names.join(", ");
        for (key, value) in resolved.vars {
            if key.starts_with("LLMENV_") {
                println!("# source: llmenv introspection");
            } else {
                println!("# source: adapter (bundles: {bundle_list})");
            }
            println!("export {}={}", key, shell_escape(&value));
        }
    } else {
        for (key, value) in resolved.vars {
            println!("export {}={}", key, shell_escape(&value));
        }
    }

    Ok(())
}
```

The dispatch arm at `src/cli/mod.rs:571-578` (`Some(Command::Export { .. }) =>
run_export(scope, tag, explain, compress)?;`) is unchanged — `run_export`'s
signature didn't change, only its body.

- [ ] **Step 4: Run the regression baseline again**

Run: `cargo test --test smoke_suite -- export`
Expected: PASS — identical to Step 1's result. If any test's stdout assertions
differ, the extraction introduced a behavior change; diff against the pre-refactor
body (`git diff`) to find what moved incorrectly.

- [ ] **Step 5: Run the full existing test suite for this file**

Run: `cargo test --lib` (unit tests in `src/cli/mod.rs`, e.g. `shell_escape_*`,
`validate_var_name_*`, `firing_bundles_*`)
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cli/mod.rs
git commit -m "refactor(cli): extract resolve_env from run_export

Shares the resolution pipeline between export and the upcoming launch
subcommand. Also fixes partial output on a validation failure: all
variables are now validated before any are returned, instead of
mid-print."
```

---

### Task 4: `llmenv launch <engine>` skeleton (spawn, inherit stdio, propagate exit code)

No signal handling yet — that's Task 5. This task's `launch` behaves like a plain
wrapper: if the terminal sends SIGINT, both `launch` and its child receive it and
both terminate (default OS signal disposition), which is already correct for the
common interactive case; Task 5 makes that explicit and correct for the
non-default cases (e.g. `launch` invoked from a script that sends signals only to
its direct child's pid).

**Files:**
- Modify: `src/cli/mod.rs` (add `Command::Launch` variant near `Command::Export`,
  ~line 133; add its dispatch arm near the `Export` arm, ~line 571; add
  `run_launch` and `exit_with_status` near `run_export`)

**Interfaces:**
- Consumes: `resolve_env` (Task 3), `crate::adapter::adapter_for_launch_target`
  (Task 2), `crate::adapter::binary_on_path` (existing, `src/adapter/mod.rs:350`),
  `crate::adapter::registered_adapters` (existing).
- Produces: `fn run_launch(engine: &str, args: Vec<String>) -> anyhow::Result<()>`,
  `fn exit_with_status(status: std::process::ExitStatus) -> !` — the latter is
  reused unchanged by Task 5.

- [ ] **Step 1: Add the CLI variant**

In the `Command` enum (`src/cli/mod.rs`, right after the `Export { .. }` variant
ending at line 146), add:

```rust
/// Resolve the environment and run `engine` as a supervised child process —
/// see #1056. Everything after `--` is passed through to the engine
/// unmodified.
Launch {
    /// Engine to launch: a binary name (claude, crush, opencode) or the
    /// underscore-form engine id (claude_code)
    engine: String,
    /// Arguments passed through to the engine binary unmodified
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
},
```

- [ ] **Step 2: Add the dispatch arm**

Right after the `Export` dispatch arm (`src/cli/mod.rs:571-578`):

```rust
Some(Command::Launch { engine, args }) => {
    run_launch(&engine, args)?;
}
```

- [ ] **Step 3: Write the failing integration tests**

Create `tests/fixtures/fake_engine.sh`:

```bash
#!/usr/bin/env bash
# Test double for a real engine binary (claude/crush/opencode). Dumps its
# environment to $FAKE_ENGINE_ENV_DUMP (if set), sleeps for
# $FAKE_ENGINE_SLEEP_SECS (if set, for signal-propagation tests), then exits
# with $FAKE_ENGINE_EXIT_CODE (default 0).
set -euo pipefail

if [[ -n "${FAKE_ENGINE_ENV_DUMP:-}" ]]; then
  env >"$FAKE_ENGINE_ENV_DUMP"
fi

if [[ -n "${FAKE_ENGINE_SLEEP_SECS:-}" ]]; then
  sleep "$FAKE_ENGINE_SLEEP_SECS"
fi

exit "${FAKE_ENGINE_EXIT_CODE:-0}"
```

Make it executable: `chmod +x tests/fixtures/fake_engine.sh`.

Create `tests/launch.rs`:

```rust
#![expect(clippy::unwrap_used, reason = "test scaffolding")]
//! Integration tests for `llmenv launch <engine>` (#1056).
//!
//! Uses `tests/fixtures/fake_engine.sh` as a stand-in for a real engine binary
//! (claude/crush/opencode) so these tests don't depend on any of them being
//! installed. The fake binary is placed on `PATH` under the name the target
//! adapter's `binary_name()` returns (e.g. `claude`), so `launch`'s own PATH
//! lookup finds it exactly as it would find the real thing.

use assert_cmd::Command;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

mod support;

/// Current OS user, used to make a user scope match in test configs — mirrors
/// `tests/smoke_suite.rs`'s helper of the same name.
fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "runner".to_string())
}

/// Minimal valid config: one user scope tagged `test`, claude-code adapter.
/// Mirrors `tests/smoke_suite.rs::config_base`.
fn config_base() -> String {
    format!(
        r#"
scope:
  network: []
  host: []
  user:
    - id: test-user
      match:
        user: {user}
      tags: [test]

tag:
  test: ""

cache:
  cache_dir: "__CACHE_DIR__"
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#,
        user = current_user(),
    )
}

/// Write `config.yaml` into a fresh temp dir, rewriting the cache-dir
/// placeholder to a path inside that same dir (test isolation — see
/// `tests/smoke_suite.rs::setup_config`'s doc comment for why this matters).
fn setup_config() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    let cache_dir = dir.path().join("cache");
    fs::write(
        &config_path,
        config_base().replace("__CACHE_DIR__", &cache_dir.display().to_string()),
    )
    .unwrap();
    (dir, config_path)
}

/// Absolute path to `tests/fixtures/fake_engine.sh`.
fn fake_engine_script() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_engine.sh")
}

/// Create `<dir>/bin/<binary_name>` as a copy of `fake_engine.sh`, executable,
/// and return `<dir>/bin` so it can be prepended to `PATH`. `binary_name` is
/// the name `launch` will look up (e.g. `"claude"`), matching what
/// `AgentAdapter::binary_name()` returns for the adapter under test.
fn install_fake_engine(dir: &std::path::Path, binary_name: &str) -> std::path::PathBuf {
    let bin_dir = dir.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let target = bin_dir.join(binary_name);
    fs::copy(fake_engine_script(), &target).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    }
    bin_dir
}

/// Build a `launch claude` command with the fake engine on `PATH` ahead of
/// the real one (if any), pointed at the given isolated config.
fn launch_cmd(config_dir: &std::path::Path, config_path: &std::path::Path) -> Command {
    let bin_dir = install_fake_engine(config_dir, "claude");
    let mut cmd = support::isolated_llmenv_cmd(config_dir);
    cmd.env("LLMENV_CONFIG", config_path);
    cmd.env(
        "PATH",
        format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    cmd.arg("launch").arg("claude");
    cmd
}

#[test]
fn launch_propagates_child_exit_code() {
    let (dir, config_path) = setup_config();
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_EXIT_CODE", "7");
    cmd.timeout(Duration::from_secs(15))
        .assert()
        .code(7);
}

#[test]
fn launch_sets_resolved_env_on_the_child() {
    let (dir, config_path) = setup_config();
    let dump_path = dir.path().join("env_dump.txt");
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ENV_DUMP", &dump_path);
    cmd.timeout(Duration::from_secs(15)).assert().success();

    let dumped = fs::read_to_string(&dump_path).unwrap();
    assert!(
        dumped.contains("CLAUDE_CONFIG_DIR="),
        "child env should carry the adapter's resolved CLAUDE_CONFIG_DIR:\n{dumped}"
    );
    assert!(
        dumped.contains("LLMENV_ACTIVE_TAGS=test"),
        "child env should carry the resolved LLMENV_ACTIVE_TAGS:\n{dumped}"
    );
}

#[test]
fn launch_rejects_unrecognized_engine() {
    let (dir, config_path) = setup_config();
    let mut cmd = support::isolated_llmenv_cmd(dir.path());
    cmd.env("LLMENV_CONFIG", &config_path);
    cmd.arg("launch").arg("__llmenv_no_such_engine_xyzzy__");
    cmd.timeout(Duration::from_secs(15))
        .assert()
        .failure()
        .stderr(predicates::str::contains("unrecognized engine"));
}

#[test]
fn launch_errors_when_binary_not_on_path() {
    let (dir, config_path) = setup_config();
    let mut cmd = support::isolated_llmenv_cmd(dir.path());
    cmd.env("LLMENV_CONFIG", &config_path);
    // No fake engine installed, and PATH is narrowed to a dir with nothing in it.
    let empty_path_dir = dir.path().join("empty-path");
    fs::create_dir_all(&empty_path_dir).unwrap();
    cmd.env("PATH", &empty_path_dir);
    cmd.arg("launch").arg("claude");
    cmd.timeout(Duration::from_secs(15))
        .assert()
        .failure()
        .stderr(predicates::str::contains("not found on PATH"));
}
```

Add `predicates` as a dev-dependency if it isn't already available to this test
target — check first:

Run: `grep -n '^predicates' Cargo.toml`
Expected: a line already exists (it's used by `tests/smoke_suite.rs`, which
imports `predicates::prelude::*`) — no change needed if so.

- [ ] **Step 4: Run the new tests to verify they fail**

Run: `cargo test --test launch`
Expected: FAIL to compile — `Command::Launch` / `run_launch` don't exist yet.

- [ ] **Step 5: Implement `run_launch` and `exit_with_status`**

Add near `run_export` in `src/cli/mod.rs`:

```rust
/// `llmenv launch <engine>`: resolve the environment the same way `export`
/// does, then spawn `engine` as a supervised child process with that
/// environment applied on top of the inherited one, inherited stdio, and the
/// child's exit code propagated as `launch`'s own (see #1056).
fn run_launch(engine: &str, args: Vec<String>) -> anyhow::Result<()> {
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

    if !crate::adapter::binary_on_path(adapter.binary_name()) {
        anyhow::bail!(
            "'{}' not found on PATH — install it before running `llmenv launch {engine}`",
            adapter.binary_name()
        );
    }

    let resolved = resolve_env(None, None, false)?;

    let mut cmd = std::process::Command::new(adapter.binary_name());
    cmd.args(&args);
    for (key, value) in &resolved.vars {
        cmd.env(key, value);
    }
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn '{}'", adapter.binary_name()))?;
    let status = child
        .wait()
        .with_context(|| format!("failed to wait on '{}'", adapter.binary_name()))?;

    exit_with_status(status);
}

/// Exit the current process mirroring `status`: the child's own exit code on
/// a normal exit, or `128 + signum` (POSIX convention — what a shell's `$?`
/// shows for a signal-killed process) if it died by signal.
fn exit_with_status(status: std::process::ExitStatus) -> ! {
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            std::process::exit(128 + sig);
        }
    }
    std::process::exit(1);
}
```

- [ ] **Step 6: Run the new tests to verify they pass**

Run: `cargo test --test launch`
Expected: all 4 tests PASS.

- [ ] **Step 7: Run the full regression baseline once more**

Run: `cargo test --test smoke_suite && cargo test --lib`
Expected: PASS (confirms Task 3's extraction plus this task's new `Launch` arm
haven't disturbed `export`/anything else).

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml src/cli/mod.rs tests/launch.rs tests/fixtures/fake_engine.sh
git commit -m "feat(cli): add llmenv launch <engine> skeleton"
```

---

### Task 5: Signal handling (SIGINT/SIGTERM/SIGHUP)

Swaps the blocking spawn+wait in `run_launch` for an async one that races the
child's exit against incoming signals, so `launch` never exits ahead of (or
because of) a signal its child is also handling — it waits the child out and
propagates whatever the child's actual exit status turns out to be.

**Files:**
- Modify: `src/cli/mod.rs` (rewrite the spawn/wait portion of `run_launch`; add
  `supervise_child`)

**Interfaces:**
- Consumes: `tokio::signal::unix::{signal, SignalKind}` (Task 1),
  `exit_with_status` (Task 4, unchanged).
- Produces: `async fn supervise_child(child: tokio::process::Child) -> anyhow::Result<std::process::ExitStatus>`.

- [ ] **Step 1: Write the failing test**

The mechanism this task implements is "`launch` receives SIGINT/SIGTERM/SIGHUP and
keeps waiting on its child instead of dying on its own account." Test that directly:
signal `launch`'s own pid (not its child — real terminal-delivered process-group
signal propagation to the grandchild is OS behavior outside anything this task
changes, not what needs testing here) with a *short* sleeping child, and confirm
`launch` (a) doesn't return early and (b) propagates the child's real exit code
once it actually exits — proof it waited the sleep out rather than dying on the
signal.

Add to `tests/launch.rs`:

```rust
#[test]
#[cfg(unix)]
fn launch_survives_sigint_and_propagates_child_exit_code() {
    let (dir, config_path) = setup_config();
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_SLEEP_SECS", "2");
    cmd.env("FAKE_ENGINE_EXIT_CODE", "5");

    let mut child = cmd.spawn().expect("spawn llmenv launch");

    // Give the fake engine time to actually start sleeping before signaling
    // `launch` itself (not the fake engine) with SIGINT.
    std::thread::sleep(Duration::from_millis(300));

    // SAFETY: signaling our own freshly-spawned child process by its own pid.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }

    let start = std::time::Instant::now();
    let status = child.wait().expect("wait on llmenv launch");
    let elapsed = start.elapsed();

    // Without the ignore-and-keep-waiting handler, `launch` dies immediately
    // on SIGINT (default disposition) instead of waiting out the fake
    // engine's remaining ~1.7s sleep — a near-instant return means the
    // handler isn't doing its job.
    assert!(
        elapsed >= Duration::from_millis(1200),
        "launch should have kept waiting on the child through its sleep \
         instead of dying on SIGINT; only waited {elapsed:?}"
    );
    // The fake engine's *real* exit code (5), not a signal-derived one —
    // proof launch waited for and propagated the actual child exit instead
    // of exiting on its own account of the SIGINT.
    assert_eq!(
        status.code(),
        Some(5),
        "launch's exit code should be the fake engine's real exit code; got {status:?}"
    );
}
```

This needs the `libc` crate as a dev-dependency for `libc::kill`/`libc::SIGINT`.
Check first:

Run: `grep -n '^libc' Cargo.toml`

If absent, add under `[dev-dependencies]` (create that section if it doesn't
exist, placed after `[dependencies]`):

```toml
[dev-dependencies]
libc = "=0.2.180"
```

(Confirm `0.2.180` is still the current stable `libc` release before pinning —
run `cargo search libc` or check crates.io; use whatever the actual latest patch
is at implementation time, pinned exactly per this repo's "no `^`/`~`" convention.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test launch launch_survives_sigint_and_propagates_child_exit_code`
Expected: FAIL — either a compile error (`libc` not yet a dependency) or (once
that's fixed) the `elapsed >= 1200ms` assertion fails, because `run_launch` is
still using a plain blocking `wait()` with no signal handling: `launch` dies
immediately on the unhandled SIGINT (default OS disposition) instead of waiting
out the child's sleep, so `elapsed` comes back near-instant.

- [ ] **Step 3: Implement `supervise_child` and rewire `run_launch`**

Replace the spawn/wait block in `run_launch` (from `let mut cmd =
std::process::Command::new(...)` through the `exit_with_status(status);` call) with:

```rust
    let mut cmd = tokio::process::Command::new(adapter.binary_name());
    cmd.args(&args);
    for (key, value) in &resolved.vars {
        cmd.env(key, value);
    }
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn '{}'", adapter.binary_name()))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for launch supervision")?;
    let status = rt.block_on(supervise_child(child))?;

    exit_with_status(status);
```

Add `supervise_child` right after `run_launch`:

```rust
/// Wait for `child` to exit while ignoring SIGINT/SIGTERM/SIGHUP delivered to
/// this process: the terminal already delivers the same signal directly to
/// the child (same process group, standard `Command::spawn` behavior), so
/// `launch` doesn't need to forward it — it needs to *not die first*, and
/// instead keep waiting until the child's own exit status is known.
async fn supervise_child(
    mut child: tokio::process::Child,
) -> anyhow::Result<std::process::ExitStatus> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let mut sighup = signal(SignalKind::hangup()).context("failed to install SIGHUP handler")?;

    loop {
        tokio::select! {
            status = child.wait() => {
                return status.context("failed to wait on child engine process");
            }
            _ = sigint.recv() => {
                tracing::debug!("launch: received SIGINT, still waiting on child");
            }
            _ = sigterm.recv() => {
                tracing::debug!("launch: received SIGTERM, still waiting on child");
            }
            _ = sighup.recv() => {
                tracing::debug!("launch: received SIGHUP, still waiting on child");
            }
        }
    }
}
```

- [ ] **Step 4: Run the new test to verify it passes**

Run: `cargo test --test launch launch_survives_sigint_and_propagates_child_exit_code`
Expected: PASS.

- [ ] **Step 5: Run every launch test plus the full regression baseline**

Run: `cargo test --test launch && cargo test --test smoke_suite && cargo test --lib`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/cli/mod.rs tests/launch.rs
git commit -m "feat(cli): forward SIGINT/SIGTERM/SIGHUP correctly in llmenv launch"
```

---

### Task 6: Docs + changelog

**Files:**
- Modify: `website/docs/commands.md` (new `## launch` section, after `## export`
  and before `## regenerate`, i.e. after line 35 / before line 37 in the current
  file)
- Modify: `CHANGELOG-3.md` (new entry under `## [Unreleased] - ReleaseDate` →
  `### Added`, creating that subsection if it doesn't already exist there)
- Regenerate: `website/docs/changelog.md` (generated file, never hand-edited)

**Interfaces:** none — this task has no code interfaces, only prose.

- [ ] **Step 1: Add the `## launch` section to `commands.md`**

Insert after the `## export` section's closing (current line 35, the
`--compress` bullet) and before `## regenerate` (current line 37):

```markdown

## `launch`

```text
llmenv launch <engine> [-- ARGS...]
```

(added in v4.0.0) Resolve the environment the same way `export` does, then run
`<engine>` (a binary name — `claude`, `crush`, `opencode` — or the underscore-form
engine id, e.g. `claude_code`) as a supervised child process: inherited stdio, the
resolved environment applied on top of the inherited one, SIGINT/SIGTERM/SIGHUP
correctly forwarded, and `launch`'s own exit code mirroring the child's (or
`128 + signum` if the child died by signal). Anything after `--` is passed through
to the engine binary unmodified, e.g. `llmenv launch claude -- --resume`.

Unlike the shell-hook + `export` model, `launch` doesn't require any shell
integration — it works the same from an interactive shell, a script, a CI job, or
an IDE task. `export` and the shell hook remain available for scripts/CI that want
resolved env vars without launching an engine.
```

(Drop the inner ` ``` ` fencing shown above when actually writing the file — that's
this plan's own code-block wrapper around the markdown snippet, not part of the
snippet.)

- [ ] **Step 2: Add the CHANGELOG-3.md entry**

In `CHANGELOG-3.md`, under `## [Unreleased] - ReleaseDate` (currently line 12),
add an `### Added` subsection (before the existing `### Fixed` subsection at line
14, since `Added` conventionally precedes `Fixed` in Keep a Changelog ordering):

```markdown
### Added

- `llmenv launch <engine>` runs an engine (`claude`/`crush`/`opencode`) as a
  supervised child process with the resolved environment already applied,
  instead of relying on the shell precmd/PROMPT_COMMAND hook + `export`. Works
  from any shell, a script, CI, or an IDE task — no shell integration required.
  See [Commands](https://phaedrus1992.github.io/llmenv/docs/commands#launch)
  (#1056)
```

- [ ] **Step 3: Regenerate the generated changelog doc**

Run: `scripts/sync-changelog-doc.sh`
Expected: `website/docs/changelog.md` is rewritten to include the new entry.

- [ ] **Step 4: Verify docs/changelog sync**

Run: `cargo test --test docs_sync`
Expected: PASS (this is the CI check mentioned in `sync-changelog-doc.sh`'s own
header comment — it fails if `website/docs/changelog.md` drifts from the
`CHANGELOG-*.md` sources).

- [ ] **Step 5: Run markdownlint on both changed docs files**

Run: `markdownlint-cli2 website/docs/commands.md CHANGELOG-3.md website/docs/changelog.md`
Expected: 0 issues (note: `docs/superpowers/` is excluded from linting by
`.markdownlint-cli2.yaml`, but `website/docs/` and root `CHANGELOG-*.md` are not
excluded — confirm this command actually lints them, not silently matches zero
files, before trusting a clean result).

- [ ] **Step 6: Commit**

```bash
git add website/docs/commands.md website/docs/changelog.md CHANGELOG-3.md
git commit -m "docs: document llmenv launch"
```

---

## Not in this plan (tracked separately, see the design doc)

The per-session unix socket (so `hook-run`/`export`/`statusline`/`check-stale`
invoked by the engine's own lifecycle hooks can reach `launch`'s warm state) and the
`hook-run` warm path over that socket are sub-issues 3 and 4 of
`docs/superpowers/specs/2026-08-11-llmenv-launch-design.md`'s decomposition — a
materially separate, independently testable optimization layer. `launch` as built by
this plan is already fully functional without it: every hook the engine fires during
a session just resolves cold, exactly like today. That socket gets its own
brainstorming → plan cycle once this ships and the baseline is proven out.
