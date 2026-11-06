use super::check_git_remote;

#[test]
fn a_directory_outside_git_is_not_a_repo() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(!super::git_repo::is_git_repo(dir.path()).expect("git runs"));
}

#[test]
fn an_initialized_directory_is_a_repo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("run git init");
    assert!(init.success());
    assert!(super::git_repo::is_git_repo(dir.path()).expect("git runs"));
}

#[test]
fn a_git_failure_is_not_reported_as_no_remote() {
    // A `.git` file that names a missing gitdir makes git exit 128 (#2710).
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(".git"),
        "gitdir: /nonexistent/llmenv-2710\n",
    )
    .expect("write");
    let err = check_git_remote(dir.path()).expect_err("a broken checkout has no remote");
    assert!(
        !err.to_string().contains("no remote configured"),
        "got: {err}"
    );
}
