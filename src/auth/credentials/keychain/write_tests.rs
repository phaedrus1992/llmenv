//! Tests for the keychain write seam. A fake setter stands in for the Security framework, so no real
//! keychain item is written.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test code: a failed fixture stops the test, and a lost worker is set up with a panic"
)]

use super::keychain_write_with;
use crate::auth::credentials::Credentials;
use std::path::Path;
use std::time::Duration;

fn creds() -> Credentials {
    Credentials::from_json(serde_json::json!({
        "claudeAiOauth": {"accessToken": "token", "expiresAt": 4_102_444_800_000u64}
    }))
    .expect("the fixture is a credential document")
}

fn config_dir() -> &'static Path {
    Path::new("llmenv-keychain-write-test")
}

#[test]
fn a_successful_setter_writes_without_error() {
    let result = keychain_write_with(config_dir(), &creds(), Duration::from_secs(5), |_, _, _| {
        Ok(())
    });
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn a_setter_error_names_the_service_and_the_cause() {
    let err = keychain_write_with(config_dir(), &creds(), Duration::from_secs(5), |_, _, _| {
        Err("user interaction is not allowed".to_string())
    })
    .expect_err("a setter error must fail the write");
    let message = format!("{err:#}");
    assert!(
        message.contains("writing keychain item for service"),
        "{message}"
    );
    assert!(
        message.contains("user interaction is not allowed"),
        "{message}"
    );
}

#[test]
fn a_write_past_its_deadline_names_the_operation_and_the_fix() {
    let err = keychain_write_with(
        config_dir(),
        &creds(),
        Duration::from_millis(20),
        |_, _, _| {
            std::thread::sleep(Duration::from_secs(1));
            Ok(())
        },
    )
    .expect_err("a write past its deadline must fail");
    let message = format!("{err:#}");
    assert!(
        message.contains("`security add-generic-password`"),
        "{message}"
    );
    assert!(message.contains("gave no answer"), "{message}");
    assert!(message.contains("Fix:"), "{message}");
}

#[test]
fn a_worker_that_dies_without_an_answer_says_the_write_may_not_be_stored() {
    // The cause of the flake is not confirmed (#3483). The worker's panic hook runs before its
    // sender drops, and under load that hook may outlast a short deadline. The long deadline
    // here keeps a slow hook from reporting a timeout.
    let err = keychain_write_with(
        config_dir(),
        &creds(),
        Duration::from_secs(60),
        |_, _, _| panic!("the setter dies before it answers"),
    )
    .expect_err("a worker that ends without an answer must fail the write");
    let message = format!("{err:#}");
    assert!(message.contains("ended without an answer"), "{message}");
    assert!(message.contains("Fix:"), "{message}");
}
