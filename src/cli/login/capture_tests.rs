//! Tests for capturing a login credential. They use the file backend, so no keychain item is touched.

#![expect(
    clippy::expect_used,
    reason = "test code: a failed fixture stops the test"
)]

use super::capture_login_credentials;
use crate::auth::credentials::{Backend, load_cached};
use std::path::Path;

const FUTURE_MS: u64 = 4_102_444_800_000;
const PAST_MS: u64 = 1_000;

/// A credential document that expires at `expires_at_ms`.
fn blob(expires_at_ms: u64) -> String {
    serde_json::json!({
        "claudeAiOauth": {"accessToken": "token", "expiresAt": expires_at_ms}
    })
    .to_string()
}

/// A login dir that holds one credential document.
fn login_dir_with(expires_at_ms: u64) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join(".credentials.json"), blob(expires_at_ms))
        .expect("write the login credential");
    dir
}

#[test]
fn a_fresh_credential_is_cached_and_written_to_the_current_folder() {
    let adapter = tempfile::tempdir().expect("temp dir");
    let login = login_dir_with(FUTURE_MS);
    let folder = tempfile::tempdir().expect("temp dir");

    capture_login_credentials(
        Backend::File,
        adapter.path(),
        login.path(),
        Some(folder.path()),
    )
    .expect("a fresh credential must be captured");

    assert!(
        load_cached(adapter.path())
            .expect("read the cache")
            .is_some(),
        "the credential must reach the durable cache"
    );
    assert!(
        folder.path().join(".credentials.json").is_file(),
        "the session folder must be logged in without a re-export"
    );
}

#[test]
fn a_login_without_a_credential_saves_nothing_and_succeeds() {
    let adapter = tempfile::tempdir().expect("temp dir");
    let login = tempfile::tempdir().expect("temp dir");
    let folder = tempfile::tempdir().expect("temp dir");

    capture_login_credentials(
        Backend::File,
        adapter.path(),
        login.path(),
        Some(folder.path()),
    )
    .expect("a login with no credential is not an error");

    assert!(
        load_cached(adapter.path())
            .expect("read the cache")
            .is_none()
    );
    assert!(!folder.path().join(".credentials.json").exists());
}

#[test]
fn an_expired_credential_is_refused_and_nothing_is_written() {
    let adapter = tempfile::tempdir().expect("temp dir");
    let login = login_dir_with(PAST_MS);
    let folder = tempfile::tempdir().expect("temp dir");

    let err = capture_login_credentials(
        Backend::File,
        adapter.path(),
        login.path(),
        Some(folder.path()),
    )
    .expect_err("an expired credential must be refused");

    let message = format!("{err:#}");
    assert!(message.contains("expired"), "{message}");
    assert!(message.contains("Fix:"), "{message}");
    assert!(
        load_cached(adapter.path())
            .expect("read the cache")
            .is_none()
    );
    assert!(!folder.path().join(".credentials.json").exists());
}

#[test]
fn no_current_folder_caches_the_credential_only() {
    let adapter = tempfile::tempdir().expect("temp dir");
    let login = login_dir_with(FUTURE_MS);

    capture_login_credentials(Backend::File, adapter.path(), login.path(), None)
        .expect("a missing session folder is not an error");

    assert!(
        load_cached(adapter.path())
            .expect("read the cache")
            .is_some()
    );
}

#[test]
fn a_current_folder_that_is_not_a_directory_is_skipped() {
    let adapter = tempfile::tempdir().expect("temp dir");
    let login = login_dir_with(FUTURE_MS);
    let not_a_dir = adapter.path().join("plain-file");
    std::fs::write(&not_a_dir, "x").expect("write a file");

    capture_login_credentials(
        Backend::File,
        adapter.path(),
        login.path(),
        Some(Path::new(&not_a_dir)),
    )
    .expect("a file where a folder was expected is skipped, not written");

    assert!(
        load_cached(adapter.path())
            .expect("read the cache")
            .is_some()
    );
}
