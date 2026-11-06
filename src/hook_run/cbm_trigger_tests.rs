//! Tests of the codebase-memory index trigger and its command parts.

use super::cbm_index_cmd::result_stdout;
use super::*;

#[test]
fn the_wrapper_is_used_only_with_a_checkpoint_and_an_executable() {
    let p = Some(std::path::Path::new("/c"));
    let e = Some(std::path::Path::new("/exe"));
    assert!(uses_wrapper(p, e));
    assert!(!uses_wrapper(p, None));
    assert!(!uses_wrapper(None, e));
    assert!(!uses_wrapper(None, None));
}

#[test]
fn a_result_file_that_cannot_be_replaced_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result.json");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("inside"), "x").unwrap();
    assert!(create_result_file(&path).is_err());
}

#[test]
fn the_result_path_is_stable_per_project_and_differs_between_projects() {
    let dir = std::path::Path::new("/cache");
    let a = index_result_path(dir, std::path::Path::new("/work/a"));
    assert_eq!(a, index_result_path(dir, std::path::Path::new("/work/a")));
    assert_ne!(a, index_result_path(dir, std::path::Path::new("/work/b")));
    assert_eq!(a.parent(), Some(dir));
    let name = a.file_name().unwrap().to_string_lossy().into_owned();
    let key = name
        .strip_prefix("index-result-")
        .and_then(|n| n.strip_suffix(".json"))
        .unwrap();
    assert!(
        key.len() == 16 && key.bytes().all(|b| b.is_ascii_hexdigit()),
        "{name}"
    );
}

#[test]
fn the_result_stdout_truncates_the_file_and_falls_back_to_null() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result.json");
    std::fs::write(&path, "old result that is long").unwrap();
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", "echo new"]).stdout(result_stdout(&path));
    assert!(cmd.status().unwrap().success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    // A path in a folder that does not exist cannot be opened: the child still runs.
    let missing = dir.path().join("no/such/dir/result.json");
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-c", "echo ignored"])
        .stdout(result_stdout(&missing));
    assert!(cmd.status().unwrap().success());
    assert!(!missing.exists());
}

#[test]
fn trigger_codebase_memory_index_creates_owner_only_bounded_log() {
    use std::os::unix::fs::PermissionsExt;
    let state_dir = tempfile::tempdir().unwrap();
    let cm = crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: None,
        mcp_permissions: None,
    };
    trigger_index_with(
        std::path::Path::new("/repos/proj"),
        &cm,
        state_dir.path(),
        || Ok("/usr/bin/true".into()),
    );

    let log_path = state_dir.path().join("codebase-memory").join("index.log");
    let meta = std::fs::metadata(&log_path)
        .unwrap_or_else(|e| panic!("expected {} to exist: {e}", log_path.display()));
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn an_indexer_started_directly_writes_its_result_but_the_wrapper_does_it_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let result = dir.path().join("result.json");
    let run = |checkpointed: bool| {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo '{}'"])
            .stdout(std::process::Stdio::null());
        direct_index_stdout(&mut cmd, checkpointed, &result);
        cmd.status().unwrap();
    };
    run(true);
    assert!(
        !result.exists(),
        "the wrapper writes the file when there is a checkpoint"
    );
    run(false);
    assert_eq!(std::fs::read_to_string(&result).unwrap().trim(), "{}");
}

// #1196: a user-configured `index_path` can be shared with a
// codebase-memory-mcp process running under a different uid (separate
// service account, container with different uid mapping). Forcing it to
// 0700 — appropriate for llmenv's own state tree — breaks that sharing
// with an EACCES surfaced only via `tracing::debug!`. Only the *default*
// (state_dir-rooted) cache dir gets hardened; an explicit override keeps
// whatever permissions its owner already gave it.
#[cfg(unix)]
#[test]
fn trigger_codebase_memory_index_leaves_user_index_path_permissions_alone() {
    use std::os::unix::fs::PermissionsExt;
    let state_dir = tempfile::tempdir().unwrap();
    let index_dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(index_dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let cm = crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: Some(index_dir.path().to_str().unwrap().to_string()),
        mcp_permissions: None,
    };

    trigger_index_with(
        std::path::Path::new("/repos/proj"),
        &cm,
        state_dir.path(),
        || Ok("/usr/bin/true".into()),
    );

    let mode = std::fs::metadata(index_dir.path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o755,
        "a user-configured index_path must keep its prior permissions, got {mode:o}"
    );
}

/// The notice list is shared by the tests of one process, so the two tests on the wrapper notice
/// take turns.
static WRAPPER_NOTICE_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

const WRAPPER_NOTICE: &str = "runs without its wrapper";

fn index_config() -> crate::config::CodebaseMemory {
    crate::config::CodebaseMemory {
        allowed_roots: vec![],
        mem_budget_mb: None,
        when: vec!["proj".to_string()],
        index_path: None,
        mcp_permissions: None,
    }
}

#[test]
fn an_index_job_with_a_checkpoint_and_its_own_program_gives_no_wrapper_notice() {
    let _turn = WRAPPER_NOTICE_TURN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::agent_notice::forget_containing(WRAPPER_NOTICE);
    let state_dir = tempfile::tempdir().unwrap();
    trigger_index_with(
        std::path::Path::new("/repos/proj"),
        &index_config(),
        state_dir.path(),
        || Ok("/usr/bin/true".into()),
    );
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(!notices.contains(WRAPPER_NOTICE), "{notices}");
}

#[test]
fn an_index_job_with_no_checkpoint_tells_the_agent_that_it_runs_without_the_wrapper() {
    let _turn = WRAPPER_NOTICE_TURN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::agent_notice::forget_containing(WRAPPER_NOTICE);
    let dir = tempfile::tempdir().unwrap();
    // A state folder that is a file cannot hold a checkpoint.
    let not_a_folder = dir.path().join("state");
    std::fs::write(&not_a_folder, b"x").unwrap();
    trigger_index_with(
        std::path::Path::new("/repos/proj"),
        &index_config(),
        &not_a_folder,
        || Ok("/usr/bin/true".into()),
    );
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(notices.contains(WRAPPER_NOTICE), "{notices}");
    assert!(notices.contains("the job has no checkpoint"), "{notices}");
}
