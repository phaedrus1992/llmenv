#![expect(clippy::unwrap_used, reason = "test scaffolding")]
//! `llmenv task ls` and its `list` alias: the deliberate choice of `--session` or `--all`
//! (#1124, #2905).

use predicates::prelude::PredicateBooleanExt as _;
use tempfile::TempDir;

mod support;
use support::isolated_llmenv_cmd as llmenv;

/// Start a session so subsequent `task add` calls auto-resolve to it.
fn start_session(dir: &std::path::Path, name: &str) {
    llmenv(dir)
        .args(["task", "session", "start", name])
        .assert()
        .success();
}

// #1124: `ls` must not silently default to every session's tasks -- force a
// deliberate choice between --session <id> and --all.
#[test]
fn ls_with_neither_session_nor_all_fails() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "ls"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--session"))
        .stderr(predicates::str::contains("--all"));
}

#[test]
fn ls_with_all_flag_succeeds() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "ls", "--all", "--format", "json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Ship the release"));
}

// #3883: a skipped task file shows on stderr, because the agent notice reaches hook output only.
#[test]
fn ls_says_on_stderr_when_it_skips_a_task_file() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    let bad = dir.path().join("tasks").join("broken.json");
    std::fs::write(&bad, "{ not json").unwrap();

    llmenv(dir.path())
        .args(["task", "ls", "--all", "--format", "json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Ship the release"))
        .stderr(predicates::str::contains("1 task file(s) skipped"))
        .stderr(predicates::str::contains("broken.json"));
}

#[test]
fn ls_prints_no_skip_line_when_every_task_file_reads() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "ls", "--all", "--format", "json"])
        .assert()
        .success()
        .stderr(predicates::str::contains("skipped").not());
}

#[test]
fn ls_with_all_and_session_is_rejected_by_clap() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "ls", "--all", "--session", "some-session"])
        .assert()
        .failure();
}

// --current-project alone doesn't satisfy the requirement -- it narrows by
// project, not by session, so it's still "everything in this project" rather
// than the single deliberate choice #1124 wants to force.
#[test]
fn ls_with_only_current_project_still_requires_session_or_all() {
    let dir = TempDir::new().unwrap();
    llmenv(dir.path())
        .args(["task", "ls", "--current-project"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--session"));
}

// #2905: agents type `task list`; it must run the same command as `task ls`.
#[test]
fn list_is_an_alias_of_ls() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "add", "Ship the release"])
        .assert()
        .success();
    llmenv(dir.path())
        .args(["task", "list", "--session", "sprint", "--format", "json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Ship the release"));
}

// The list-style subcommands accept both spellings, so an agent's guess works (#2905).
#[test]
fn every_list_subcommand_accepts_both_spellings() {
    let dir = TempDir::new().unwrap();
    start_session(dir.path(), "sprint");
    llmenv(dir.path())
        .args(["task", "session", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("sprint"));
    for args in [["memory", "ls", "--help"], ["memory", "list", "--help"]] {
        llmenv(dir.path()).args(args).assert().success();
    }
}
