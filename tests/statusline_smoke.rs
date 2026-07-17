#![expect(clippy::unwrap_used, reason = "test scaffolding")]
//! CLI-level smoke tests for `llmenv statusline`.
//!
//! Unlike the unit/orchestrator-level tests in `src/cli/statusline/mod.rs`
//! (which call `run_statusline` directly as a Rust function), these tests
//! spawn the real compiled `llmenv` binary via `assert_cmd`, mirroring the
//! convention in `tests/smoke_suite.rs`: a temp `config.yaml`, env vars
//! pointing at it, and assertions on the subprocess's actual stdout.
//!
//! This proves the full pipeline — config parsing, data-file loading, stdin
//! parsing, widget rendering, icon/format overrides — works end-to-end
//! through the actual binary, not just via internal function calls.
//!
//! Refs #836.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

/// Write `config.yaml` into a fresh temp dir and return the dir (kept alive
/// for the test) plus its path. The dir is used as `LLMENV_CONFIG_DIR`; the
/// separate `CLAUDE_CONFIG_DIR` data-file dir is set up by callers via
/// `statusline_cmd`'s own `data_dir` parameter, not this helper.
fn setup_config(content: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    fs::write(&config_path, content).unwrap();
    (dir, config_path)
}

/// Build a `Command` for `llmenv statusline`, pointed at the temp config and
/// with `CLAUDE_CONFIG_DIR` set to `data_dir` so `run_statusline_cmd` resolves
/// `llmenv-status.json` there instead of the real materialized cache dir.
fn statusline_cmd(
    config_dir: &std::path::Path,
    config_path: &std::path::Path,
    data_dir: &std::path::Path,
) -> Command {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    cmd.env("LLMENV_CONFIG", config_path)
        .env("LLMENV_CONFIG_DIR", config_dir)
        .env("LLMENV_STATE_DIR", config_dir)
        .env("CLAUDE_CONFIG_DIR", data_dir)
        .arg("statusline");
    cmd
}

/// Run a command with an explicit timeout and assert it completes within it.
fn assert_completes_within(mut cmd: Command, timeout_secs: u64) -> assert_cmd::assert::Assert {
    cmd.timeout(Duration::from_secs(timeout_secs)).assert()
}

/// Minimal valid config with no `statusline:` section at all, so the CLI
/// falls back to `DEFAULT_ROW`.
const CONFIG_NO_STATUSLINE: &str = r#"
scope:
  network: []
  host: []
  user: []

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code
"#;

/// Config with a custom `statusline.rows` template.
fn config_with_rows(rows_yaml: &str) -> String {
    format!(
        r#"
scope:
  network: []
  host: []
  user: []

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code

statusline:
  rows:
{rows_yaml}
"#
    )
}

#[test]
fn smoke_statusline_default_row_engine_only() {
    let (dir, config_path) = setup_config(CONFIG_NO_STATUSLINE);
    // No llmenv-status.json written at all — data dir exists but is empty.
    let data_dir = TempDir::new().unwrap();

    let mut cmd = statusline_cmd(dir.path(), &config_path, data_dir.path());
    cmd.write_stdin(r#"{"model": {"display_name": "Claude Opus 4.8"}}"#);

    assert_completes_within(cmd, 10)
        .success()
        .stdout(predicate::str::contains("Opus"))
        .stdout(predicate::str::contains(" │ "));
}

#[test]
fn smoke_statusline_custom_row_template() {
    let (dir, config_path) = setup_config(&config_with_rows("    - \"{model} | {branch}\""));
    let data_dir = TempDir::new().unwrap();

    let mut cmd = statusline_cmd(dir.path(), &config_path, data_dir.path());
    cmd.write_stdin(
        r#"{"model": {"display_name": "Claude Opus 4.8"}, "branch": {"name": "release/3.x"}}"#,
    );

    assert_completes_within(cmd, 10)
        .success()
        .stdout(predicate::str::contains("Opus"))
        .stdout(predicate::str::contains("release/3.x"))
        .stdout(predicate::str::contains(" | "))
        .stdout(predicate::str::contains(" │ ").not());
}

#[test]
fn smoke_statusline_renders_llmenv_widgets_from_data_file() {
    let (dir, config_path) = setup_config(&config_with_rows(
        "    - \"{scopes} {plugins} {mcps} {config_stale}\"",
    ));
    let data_dir = TempDir::new().unwrap();
    fs::write(
        data_dir.path().join("llmenv-status.json"),
        serde_json::json!({
            "$schema": "llmenv-status-v1",
            "v": 1,
            "ts": "2026-07-17T00:00:00Z",
            "scopes": { "tags": ["dev", "rust"] },
            "plugins": { "total": 3, "errors": 0 },
            "mcps": { "total": 2, "errors": 0 },
            "config_stale": true
        })
        .to_string(),
    )
    .unwrap();

    let mut cmd = statusline_cmd(dir.path(), &config_path, data_dir.path());
    cmd.write_stdin("{}");

    assert_completes_within(cmd, 10)
        .success()
        .stdout(predicate::str::contains("dev · rust"))
        .stdout(predicate::str::contains("◇ 3"))
        .stdout(predicate::str::contains("MCP 2"))
        .stdout(predicate::str::contains("~"));
}

#[test]
fn smoke_statusline_custom_icon_set_and_widget_format_override() {
    let (dir, config_path) = setup_config(
        r#"
scope:
  network: []
  host: []
  user: []

cache:
  sync_interval_minutes: 60

adapter:
  engine: claude-code

statusline:
  rows:
    - "{context_pct} {config_stale}"
  style:
    icon_set: simple
  icons:
    config_stale: "STALE"
  widgets:
    context_pct:
      format: "ctx={pct}"
"#,
    );
    let data_dir = TempDir::new().unwrap();
    fs::write(
        data_dir.path().join("llmenv-status.json"),
        serde_json::json!({
            "$schema": "llmenv-status-v1",
            "v": 1,
            "ts": "2026-07-17T00:00:00Z",
            "config_stale": true
        })
        .to_string(),
    )
    .unwrap();

    let mut cmd = statusline_cmd(dir.path(), &config_path, data_dir.path());
    cmd.write_stdin(r#"{"context_window": {"remaining_percentage": 60.0}}"#);

    assert_completes_within(cmd, 10)
        .success()
        .stdout(predicate::str::contains("ctx=40"))
        .stdout(predicate::str::contains("STALE"));
}

#[test]
fn smoke_statusline_missing_everything_degrades_gracefully() {
    let (dir, config_path) = setup_config(CONFIG_NO_STATUSLINE);
    // Data dir exists but no llmenv-status.json is written into it.
    let data_dir = TempDir::new().unwrap();

    let mut cmd = statusline_cmd(dir.path(), &config_path, data_dir.path());
    cmd.write_stdin("not json");

    assert_completes_within(cmd, 10).success();
}
