//! Tests for `llmenv init`. Each run uses a temp folder, so no default config is touched.

#![expect(
    clippy::expect_used,
    reason = "test code: a failed fixture stops the test"
)]

use super::init_cmd::{init_config_dir, run_init, write_starter_if_missing};
use crate::config::Config;
use std::path::{Path, PathBuf};

#[test]
fn a_new_folder_gets_a_valid_config_and_both_starter_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("cfg");
    run_init(Some(target.clone()), None).expect("init writes into a new folder");

    assert!(target.join("AGENTS.md").is_file(), "agent guide missing");
    assert!(target.join("README.md").is_file(), "README missing");
    Config::load(&target.join("config.yaml")).expect("the written config must load");
}

#[test]
fn a_second_init_leaves_the_existing_config_alone() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("cfg");
    run_init(Some(target.clone()), None).expect("first init");
    let config_path = target.join("config.yaml");
    let before = std::fs::read_to_string(&config_path).expect("read config");

    run_init(Some(target.clone()), None).expect("second init is not an error");

    let after = std::fs::read_to_string(&config_path).expect("read config");
    assert_eq!(before, after, "an existing config must not be rewritten");
}

#[test]
fn a_starter_file_the_user_wrote_is_kept() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("cfg");
    std::fs::create_dir_all(&target).expect("create the folder");
    std::fs::write(target.join("README.md"), "mine").expect("write a README");

    run_init(Some(target.clone()), None).expect("init");

    let readme = std::fs::read_to_string(target.join("README.md")).expect("read README");
    assert_eq!(readme, "mine", "init must not overwrite the user's README");
}

#[test]
fn a_git_clone_is_refused_until_it_is_built() {
    let dir = tempfile::tempdir().expect("temp dir");
    let result = run_init(
        Some(dir.path().join("cfg")),
        Some("https://example.com/repo.git".to_string()),
    );
    let message = match &result {
        Ok(()) => String::new(),
        Err(e) => format!("{e:#}"),
    };
    assert!(result.is_err(), "a repo argument must not be accepted yet");
    assert!(message.contains("not yet implemented"), "{message}");
}

#[cfg(unix)]
#[test]
fn a_non_utf8_init_path_names_the_problem() {
    use std::os::unix::ffi::OsStrExt;
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(&[0xff, b'x']));
    let result = init_config_dir(Some(path));
    let message = match &result {
        Ok(_) => String::new(),
        Err(e) => format!("{e:#}"),
    };
    assert!(result.is_err(), "a non-UTF-8 init path must be refused");
    assert!(message.contains("not valid UTF-8"), "{message}");
}

#[test]
fn a_starter_write_into_a_missing_folder_names_the_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = Path::new("no-such-folder").join("AGENTS.md");
    let result = write_starter_if_missing(&dir.path().join(missing), "x", "README");
    let message = match &result {
        Ok(()) => String::new(),
        Err(e) => format!("{e:#}"),
    };
    assert!(result.is_err(), "a starter needs an existing folder");
    assert!(message.contains("creating README at"), "{message}");
}
