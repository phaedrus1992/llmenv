#![expect(clippy::unwrap_used, reason = "test scaffolding")]
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
