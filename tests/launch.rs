#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
//! Integration tests for `llmenv launch <engine>` (#1056).
//!
//! Uses `tests/fixtures/fake_engine.sh` as a stand-in for a real engine binary
//! (claude/crush/opencode) so these tests don't depend on any of them being
//! installed. The fake binary is placed on `PATH` under the name the target
//! adapter's `binary_name()` returns (e.g. `claude`), so `launch`'s own PATH
//! lookup finds it exactly as it would find the real thing.

use std::fs;
use std::time::Duration;

use assert_cmd::Command;
use tempfile::TempDir;

mod support;

/// Budget for a single `launch` invocation, matching `smoke_suite`'s
/// `LONG_TIMEOUT_SECS` — these are hang detectors, not performance assertions.
const LAUNCH_TIMEOUT_SECS: u64 = 30;

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
/// placeholder to a path inside that same dir. Without that, `cache.cache_dir`
/// defaults to the real `~/.cache/llmenv` and concurrent tests resolving the
/// same tags overwrite each other's materialized output (#1254).
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

/// An `llmenv launch` command with the fake engine on `PATH` ahead of the real
/// one (if any) and no arguments yet — for tests that need llmenv's own flags
/// *before* the engine name (#1384).
fn launch_cmd_no_args(config_dir: &std::path::Path, config_path: &std::path::Path) -> Command {
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
    cmd.arg("launch");
    cmd
}

/// Build a `launch claude` command with the fake engine on `PATH` ahead of
/// the real one (if any), pointed at the given isolated config.
fn launch_cmd(config_dir: &std::path::Path, config_path: &std::path::Path) -> Command {
    let mut cmd = launch_cmd_no_args(config_dir, config_path);
    cmd.arg("claude");
    cmd
}

#[test]
fn launch_propagates_child_exit_code() {
    let (dir, config_path) = setup_config();
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_EXIT_CODE", "7");
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .code(7);
}

/// Variable names `llmenv export` emits for the given isolated config, parsed
/// off its `export KEY=VALUE` lines. Only the names are extracted: values are
/// shell-quoted and some (`LLMENV_ICM_CONTEXT`) span multiple lines, so
/// unquoting them here would just reimplement `shell_escape` in the test.
fn exported_var_names(config_dir: &std::path::Path, config_path: &std::path::Path) -> Vec<String> {
    let mut cmd = support::isolated_llmenv_cmd(config_dir);
    cmd.env("LLMENV_CONFIG", config_path);
    cmd.arg("export");
    let out = cmd
        .timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("export "))
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.to_string()))
        .collect()
}

/// `launch` must not grow a second resolution behavior: every variable `export`
/// resolves for the same scope has to reach the supervised child. Asserted as
/// name-set parity against a real `export` run rather than against a hardcoded
/// list, because which variables resolve depends on the ambient environment and
/// working directory (project scopes, host tags), not just the test's config.
#[test]
fn launch_child_env_matches_export() {
    let (dir, config_path) = setup_config();
    let expected = exported_var_names(dir.path(), &config_path);
    assert!(
        expected.iter().any(|k| k == "LLMENV_ACTIVE_TAGS"),
        "export should have resolved at least LLMENV_ACTIVE_TAGS; got {expected:?}"
    );

    let dump_path = dir.path().join("env_dump.txt");
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ENV_DUMP", &dump_path);
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();

    let dumped = fs::read_to_string(&dump_path).unwrap();
    for key in &expected {
        assert!(
            dumped.lines().any(|l| l.starts_with(&format!("{key}="))),
            "child env is missing '{key}', which `export` resolved for the same \
             scope — launch must apply the identical resolved environment:\n{dumped}"
        );
    }
}

/// The resolved value, not just the name, has to reach the child — a child that
/// merely inherited the parent's ambient `LLMENV_ACTIVE_TAGS` would satisfy a
/// name-only check while carrying the wrong environment entirely.
#[test]
fn launch_child_env_carries_resolved_values_not_inherited_ones() {
    let (dir, config_path) = setup_config();
    let dump_path = dir.path().join("env_dump.txt");
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ENV_DUMP", &dump_path);
    // A sentinel the resolver can never produce; if the child sees this value,
    // launch passed the inherited environment through instead of the resolved one.
    cmd.env("LLMENV_ACTIVE_TAGS", "__inherited_sentinel__");
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();

    let dumped = fs::read_to_string(&dump_path).unwrap();
    let tags = dumped
        .lines()
        .find_map(|l| l.strip_prefix("LLMENV_ACTIVE_TAGS="))
        .expect("child env should carry LLMENV_ACTIVE_TAGS");
    assert_ne!(
        tags, "__inherited_sentinel__",
        "launch overwrote nothing — the child inherited the ambient \
         LLMENV_ACTIVE_TAGS instead of the resolved one"
    );
    assert!(
        tags.split(',').any(|t| t == "test"),
        "resolved tags should include the config's 'test' tag; got {tags:?}"
    );
}

#[test]
fn launch_passes_trailing_args_through_to_the_engine() {
    let (dir, config_path) = setup_config();
    let argv_path = dir.path().join("argv.txt");
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ARGV_DUMP", &argv_path);
    cmd.arg("--").arg("--resume").arg("session-42");
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();

    let argv = fs::read_to_string(&argv_path).unwrap();
    assert_eq!(
        argv, "--resume\nsession-42\n",
        "engine should receive the post-`--` args verbatim, got: {argv:?}"
    );
}

/// Variable names `llmenv export` emits with extra flags appended — the same
/// parse as [`exported_var_names`], used to hold `launch`'s scope narrowing to
/// `export`'s (#1384).
fn exported_var_names_with(
    config_dir: &std::path::Path,
    config_path: &std::path::Path,
    extra: &[&str],
) -> Vec<String> {
    let mut cmd = support::isolated_llmenv_cmd(config_dir);
    cmd.env("LLMENV_CONFIG", config_path);
    cmd.arg("export").args(extra);
    let out = cmd
        .timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("export "))
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.to_string()))
        .collect()
}

/// The literal parity `export` has always had and `launch` didn't (#1384):
/// `--scope` narrows resolution, and the post-`--` arguments still reach the
/// engine untouched, despite `args` being a `trailing_var_arg`.
#[test]
fn launch_scope_flag_narrows_and_still_passes_trailing_args() {
    let (dir, config_path) = setup_config();
    let argv_path = dir.path().join("argv.txt");
    let dump_path = dir.path().join("env_dump.txt");

    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ARGV_DUMP", &argv_path);
    cmd.env("FAKE_ENGINE_ENV_DUMP", &dump_path);
    cmd.args(["--scope", "test-user", "--", "--resume"]);
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(&argv_path).unwrap(),
        "--resume\n",
        "llmenv's own --scope must not leak into the engine's argv, and --resume must reach it"
    );
    let dumped = fs::read_to_string(&dump_path).unwrap();
    let tags = dumped
        .lines()
        .find_map(|l| l.strip_prefix("LLMENV_ACTIVE_TAGS="))
        .expect("child env should carry LLMENV_ACTIVE_TAGS");
    assert!(
        tags.split(',').any(|t| t == "test"),
        "the named scope's tag should have resolved; got {tags:?}"
    );
}

/// Same flags, written before the engine name. Both orders have to work: clap
/// only starts collecting the trailing var-arg at `--`, so a flag anywhere ahead
/// of that is llmenv's.
#[test]
fn launch_scope_flag_may_precede_the_engine() {
    let (dir, config_path) = setup_config();
    let argv_path = dir.path().join("argv.txt");

    let mut cmd = launch_cmd_no_args(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ARGV_DUMP", &argv_path);
    cmd.args(["--scope", "test-user", "claude", "--", "--resume"]);
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();

    assert_eq!(fs::read_to_string(&argv_path).unwrap(), "--resume\n");
}

/// An engine with its own `--scope` is still reachable: everything after `--`
/// belongs to the engine, including names llmenv also uses.
#[test]
fn launch_leaves_the_engines_own_scope_flag_alone_after_the_separator() {
    let (dir, config_path) = setup_config();
    let argv_path = dir.path().join("argv.txt");

    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ARGV_DUMP", &argv_path);
    cmd.args(["--", "--scope", "engine-side", "--compress"]);
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(&argv_path).unwrap(),
        "--scope\nengine-side\n--compress\n",
        "post-`--` arguments are the engine's, even when they collide with llmenv's flag names"
    );
}

/// A requested scope that isn't active warns rather than failing — identical to
/// `export`'s behavior, asserted against `export` itself so the two can't drift.
#[test]
fn launch_warns_about_an_inactive_scope_like_export() {
    let (dir, config_path) = setup_config();
    const NEEDLE: &str = "not active in current environment";

    let mut export_cmd = support::isolated_llmenv_cmd(dir.path());
    export_cmd.env("LLMENV_CONFIG", &config_path);
    export_cmd.args(["export", "--scope", "__no_such_scope__"]);
    export_cmd
        .timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success()
        .stderr(predicates::str::contains(NEEDLE));

    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.args(["--scope", "__no_such_scope__"]);
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success()
        .stderr(predicates::str::contains(NEEDLE));
}

/// `--tag` and `--compress` reach `resolve_env` too, held to `export`'s resolved
/// variable set for the same flags rather than to a hardcoded list.
#[test]
fn launch_tag_and_compress_flags_match_export() {
    let (dir, config_path) = setup_config();
    let flags = ["--tag", "test", "--compress"];
    let expected = exported_var_names_with(dir.path(), &config_path, &flags);
    assert!(
        expected.iter().any(|k| k == "LLMENV_ACTIVE_TAGS"),
        "export should have resolved at least LLMENV_ACTIVE_TAGS; got {expected:?}"
    );

    let dump_path = dir.path().join("env_dump.txt");
    let mut cmd = launch_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ENV_DUMP", &dump_path);
    cmd.args(flags);
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();

    let dumped = fs::read_to_string(&dump_path).unwrap();
    for key in &expected {
        assert!(
            dumped.lines().any(|l| l.starts_with(&format!("{key}="))),
            "child env is missing '{key}', which `export {}` resolved:\n{dumped}",
            flags.join(" ")
        );
    }
}

/// Same configuration as [`launch_cmd`], but as a plain `std::process::Command`
/// so the test can hold the spawned child and signal it mid-run — `assert_cmd`'s
/// wrapper runs to completion, which is exactly what this test must not do.
#[cfg(unix)]
fn launch_std_cmd(
    config_dir: &std::path::Path,
    config_path: &std::path::Path,
) -> std::process::Command {
    let bin_dir = install_fake_engine(config_dir, "claude");
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("llmenv"));
    for key in [
        "LLMENV_CONFIG_DIR",
        "LLMENV_STATE_DIR",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "HOME",
    ] {
        cmd.env(key, config_dir);
    }
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

/// `launch` must not die on a signal its child is also receiving: the terminal
/// Spawn `launch` with a sleeping engine and block until the engine is
/// actually running, returning the `launch` child and the elapsed-time origin.
///
/// Waits on a marker the fake engine writes *before* it sleeps rather than a
/// fixed delay: signalling on a timer raced env resolution, which can outlast
/// any delay short enough to keep the test quick, and killing `launch` before
/// it has spawned anything tests the default signal disposition instead of the
/// supervisor.
#[cfg(unix)]
fn launch_sleeping_engine(
    dir: &std::path::Path,
    config_path: &std::path::Path,
    exit_code: &str,
) -> std::process::Child {
    let ready_marker = dir.join("engine_started.txt");
    let mut cmd = launch_std_cmd(dir, config_path);
    cmd.env("FAKE_ENGINE_ENV_DUMP", &ready_marker);
    cmd.env("FAKE_ENGINE_SLEEP_SECS", "2");
    cmd.env("FAKE_ENGINE_EXIT_CODE", exit_code);

    let child = cmd.spawn().expect("spawn llmenv launch");

    let wait_started = std::time::Instant::now();
    while !ready_marker.exists() {
        assert!(
            wait_started.elapsed() < Duration::from_secs(LAUNCH_TIMEOUT_SECS),
            "fake engine never started under `llmenv launch`"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

/// The guard `binary_on_path` applies has to hold at *exec* time, not just at
/// check time. `std::process::Command` execs a bare name via `execvp`, which runs
/// its own `PATH` search and — per POSIX — treats a zero-length entry as the
/// working directory. So with `PATH=":<real bin dir>"`, the check found the real
/// engine while `execvp` ran `./claude` from the cwd, and `launch` layered the
/// resolved environment (MCP endpoints, tokens) onto it.
///
/// `launch` resolves once and spawns the absolute path, so the decoy is ignored.
#[test]
#[cfg(unix)]
fn launch_ignores_an_engine_decoy_in_the_working_directory() {
    let (dir, config_path) = setup_config();
    let bin_dir = install_fake_engine(dir.path(), "claude");
    let argv_path = dir.path().join("argv.txt");

    // The decoy: a `claude` in the process working directory that would report
    // itself if run. `cwd` is the test's own temp dir, set on the command below.
    let cwd = dir.path().join("run-from-here");
    fs::create_dir_all(&cwd).unwrap();
    let decoy = cwd.join("claude");
    fs::write(
        &decoy,
        "#!/bin/sh\nprintf 'DECOY' > \"$FAKE_ENGINE_ARGV_DUMP\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&decoy, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut cmd = support::isolated_llmenv_cmd(dir.path());
    cmd.env("LLMENV_CONFIG", &config_path);
    cmd.current_dir(&cwd);
    // The leading `:` is the empty entry — what `PATH="$MAYBE_UNSET:$PATH"` in a
    // shell profile produces. The real engine is present, later in the list.
    cmd.env("PATH", format!(":{}", bin_dir.display()));
    cmd.env("FAKE_ENGINE_ARGV_DUMP", &argv_path);
    cmd.arg("launch").arg("claude");
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .success();

    // `install_fake_engine`'s stub writes one line per argument, so an empty file
    // is the real engine having run with no args. "DECOY" means the cwd copy ran.
    let dumped = fs::read_to_string(&argv_path).unwrap();
    assert!(
        !dumped.contains("DECOY"),
        "launch executed the `claude` in its working directory via the empty PATH \
         entry instead of the real one on PATH"
    );
}

/// Send `signal` (a `kill(1)` name like `TERM`) to `pid`.
#[cfg(unix)]
fn send_signal(pid: u32, signal: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("send signal to llmenv launch");
    assert!(status.success(), "kill -{signal} should have succeeded");
}

/// SIGTERM reaches `launch` alone in every non-interactive context the docs
/// advertise — `docker stop` signals PID 1, systemd `KillMode=mixed` signals
/// the main pid, a supervisor or CI runner does `kill <pid>`. The terminal
/// never generates SIGTERM, so nothing else delivers it to the engine: unless
/// `launch` forwards it, the engine keeps running and nothing shuts down until
/// the caller's SIGKILL deadline (#1383).
#[test]
#[cfg(unix)]
fn launch_forwards_sigterm_to_the_engine() {
    let (dir, config_path) = setup_config();
    let mut child = launch_sleeping_engine(dir.path(), &config_path, "5");

    let start = std::time::Instant::now();
    send_signal(child.id(), "TERM");
    let status = child.wait().expect("wait on llmenv launch");
    let elapsed = start.elapsed();

    // The engine sleeps 2s and would exit 5 on its own. Dying by SIGTERM well
    // before that is the proof it actually received the forwarded signal.
    assert!(
        elapsed < Duration::from_millis(1500),
        "launch should have forwarded SIGTERM and exited promptly, not waited \
         out the engine's 2s sleep; took {elapsed:?} with {status:?}"
    );
    assert_eq!(
        status.code(),
        Some(128 + 15),
        "engine died by SIGTERM, so launch should report 128+15; got {status:?}"
    );
}

/// SIGHUP has the same pid-targeted delivery problem as SIGTERM when it comes
/// from a supervisor rather than a terminal hangup.
#[test]
#[cfg(unix)]
fn launch_forwards_sighup_to_the_engine() {
    let (dir, config_path) = setup_config();
    let mut child = launch_sleeping_engine(dir.path(), &config_path, "5");

    let start = std::time::Instant::now();
    send_signal(child.id(), "HUP");
    let status = child.wait().expect("wait on llmenv launch");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(1500),
        "launch should have forwarded SIGHUP and exited promptly; took {elapsed:?}"
    );
    assert_eq!(
        status.code(),
        Some(128 + 1),
        "engine died by SIGHUP, so launch should report 128+1; got {status:?}"
    );
}

/// delivers SIGINT to the whole process group, and if `launch` exits on it the
/// caller sees a signal-derived status while the engine is still shutting down.
/// Signalling `launch`'s own pid and then asserting it waited the child out and
/// reported the child's real exit code proves the handler is doing its job.
///
/// This also guards the deliberate asymmetry with SIGTERM/SIGHUP: SIGINT must
/// *not* be forwarded. The terminal already delivers it to the whole foreground
/// process group, and an agent TUI typically reads a second interrupt as "force
/// quit" — so forwarding would turn one Ctrl-C into two. If SIGINT were
/// forwarded here the engine would die by signal and this would see 130 rather
/// than its real exit code.
#[test]
#[cfg(unix)]
fn launch_survives_sigint_and_propagates_child_exit_code() {
    let (dir, config_path) = setup_config();
    // The fake engine writes this dump *before* it starts sleeping, so its
    // existence is the signal that the child is actually up and `launch` has
    // installed its handlers. Signalling on a fixed delay instead raced env
    // resolution, which can outlast any delay short enough to keep the test
    // quick — and killing `launch` before it has spawned anything tests the
    // default signal disposition rather than the supervisor.
    let ready_marker = dir.path().join("engine_started.txt");
    let mut cmd = launch_std_cmd(dir.path(), &config_path);
    cmd.env("FAKE_ENGINE_ENV_DUMP", &ready_marker);
    cmd.env("FAKE_ENGINE_SLEEP_SECS", "2");
    cmd.env("FAKE_ENGINE_EXIT_CODE", "5");

    let mut child = cmd.spawn().expect("spawn llmenv launch");

    let wait_started = std::time::Instant::now();
    while !ready_marker.exists() {
        assert!(
            wait_started.elapsed() < Duration::from_secs(LAUNCH_TIMEOUT_SECS),
            "fake engine never started under `llmenv launch`"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let start = std::time::Instant::now();
    let killed = std::process::Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .expect("send SIGINT to llmenv launch");
    assert!(killed.success(), "kill -INT should have succeeded");

    let status = child.wait().expect("wait on llmenv launch");
    let elapsed = start.elapsed();

    // Without the ignore-and-keep-waiting handler, `launch` dies immediately on
    // SIGINT (default disposition) instead of waiting out the fake engine's
    // remaining ~1.7s sleep — a near-instant return means the handler is absent.
    assert!(
        elapsed >= Duration::from_millis(1500),
        "launch should have kept waiting on the child through its 2s sleep \
         instead of dying on SIGINT; returned after {elapsed:?} with {status:?}"
    );
    // The fake engine's *real* exit code (5), not a signal-derived one — proof
    // launch waited for and propagated the actual child exit instead of exiting
    // on its own account of the SIGINT.
    assert_eq!(
        status.code(),
        Some(5),
        "launch's exit code should be the fake engine's real exit code; got {status:?}"
    );
}

#[test]
fn launch_rejects_unrecognized_engine() {
    let (dir, config_path) = setup_config();
    let mut cmd = support::isolated_llmenv_cmd(dir.path());
    cmd.env("LLMENV_CONFIG", &config_path);
    cmd.arg("launch").arg("__llmenv_no_such_engine_xyzzy__");
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .failure()
        .stderr(predicates::str::contains("unrecognized engine"));
}

/// #1382: the PATH probe must not depend on a `which` binary being present.
/// Distroless and minimal container images routinely ship without it, and
/// `launch` is the first caller that turns a negative probe into a hard error —
/// so a missing `which` there produced "'claude' not found on PATH — install
/// it" for an engine that was installed and runnable.
///
/// PATH here contains *only* the fake engine's directory: no `which`, no
/// coreutils. The engine must still be found and launched.
#[test]
fn launch_finds_the_engine_without_which_on_path() {
    let (dir, config_path) = setup_config();
    let bin_dir = install_fake_engine(dir.path(), "claude");
    let mut cmd = support::isolated_llmenv_cmd(dir.path());
    cmd.env("LLMENV_CONFIG", &config_path);
    cmd.env("PATH", &bin_dir);
    cmd.env("FAKE_ENGINE_EXIT_CODE", "3");
    cmd.arg("launch").arg("claude");
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .code(3);
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
    cmd.timeout(Duration::from_secs(LAUNCH_TIMEOUT_SECS))
        .assert()
        .failure()
        .stderr(predicates::str::contains("not found on PATH"));
}
