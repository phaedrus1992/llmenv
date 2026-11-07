//! A notice that a detached child records reaches the agent on the next hook output (#3398).

#![expect(
    clippy::unwrap_used,
    reason = "test code: a failed fixture stops the test"
)]

mod support;

use std::fs;
use std::path::Path;
use std::time::Duration;

const CONFIG: &str = r#"
scope:
  network: []
  host: []
  user: []
adapter:
  engine: claude-code
"#;

fn write_config(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("config.yaml");
    fs::write(&path, CONFIG).unwrap();
    path
}

/// The stdout of one `stop` hook run in `dir`.
fn stop_hook_stdout(dir: &Path, config: &Path) -> String {
    let payload = serde_json::json!({
        "hook_event_name": "Stop",
        "session_id": "child-notice-test",
        "last_assistant_message": "done",
    })
    .to_string();
    let output = support::isolated_llmenv_cmd(dir)
        .env("LLMENV_CONFIG", config)
        .args(["hook-run", "stop"])
        .write_stdin(payload.as_str())
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap();
    assert!(output.status.success(), "the hook must exit 0");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn a_spooled_notice_is_printed_by_the_next_hook_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let spool = dir.path().join("notice_spool");
    fs::create_dir_all(&spool).unwrap();
    fs::write(
        spool.join("1-0.txt"),
        "child notice test: the job failed. Fix: run it again.",
    )
    .unwrap();

    let stdout = stop_hook_stdout(dir.path(), &config);

    assert!(
        stdout.contains("child notice test: the job failed"),
        "{stdout}"
    );
    assert_eq!(
        fs::read_dir(&spool).unwrap().count(),
        0,
        "a printed notice is removed"
    );
    let again = stop_hook_stdout(dir.path(), &config);
    assert!(
        !again.contains("child notice test"),
        "a notice is printed once: {again}"
    );
}

#[test]
fn a_notice_from_a_real_index_child_reaches_the_next_hook() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let checkpoints = dir.path().join("checkpoints");
    fs::create_dir_all(&checkpoints).unwrap();
    let checkpoint = checkpoints.join("cbm-index-test.json");
    let body = serde_json::json!({
        "kind": "cbm-index",
        "key": "test",
        "phase": "started",
        "inputs": {
            "project_root": dir.path().display().to_string(),
            "index_path": null,
            "mem_budget_mb": null,
            "result_path": null,
        },
        "started_at": 0,
        "attempts": 1,
        "session_id": null,
    });
    fs::write(&checkpoint, body.to_string()).unwrap();

    // The indexer is not on this PATH, so the child fails and records a notice in its own
    // process. Only the spool carries it to the next hook run.
    let child = support::isolated_llmenv_cmd(dir.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["cbm-index-run", "--checkpoint"])
        .arg(&checkpoint)
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap();
    assert!(
        !child.status.success(),
        "the child must fail without the indexer"
    );

    let stdout = stop_hook_stdout(dir.path(), &config);
    assert!(stdout.contains("the code index job failed"), "{stdout}");
}

#[test]
fn a_child_that_fails_before_it_runs_reaches_the_next_hook() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    // A checkpoint outside the checkpoint folder is refused before the job runs.
    let outside = dir.path().join("elsewhere.json");
    fs::write(&outside, "{}").unwrap();
    let child = support::isolated_llmenv_cmd(dir.path())
        .args(["cbm-index-run", "--checkpoint"])
        .arg(&outside)
        .timeout(Duration::from_secs(20))
        .output()
        .unwrap();
    assert!(
        !child.status.success(),
        "the child must refuse the checkpoint"
    );

    // The agent gets the fixed notice. The cause stays in the log (#3507).
    let stdout = stop_hook_stdout(dir.path(), &config);
    assert!(stdout.contains("a detached job failed. Fix:"), "{stdout}");
    assert!(!stdout.contains("elsewhere.json"), "{stdout}");
}
