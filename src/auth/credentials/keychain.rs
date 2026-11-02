//! The macOS keychain store of the credential, through `/usr/bin/security` and the Security
//! framework.

use std::path::Path;
#[cfg(any(target_os = "macos", test))]
use std::time::Duration;

/// The longest `security delete-generic-password` may run. It shows no prompt for an item that
/// llmenv created, so a stall is a locked keychain or a stuck `securityd` (#2892).
#[cfg(target_os = "macos")]
const FORGET_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest `security find-generic-password` may run. It can show the macOS keychain access
/// prompt, and the user needs time to answer it (#2892).
#[cfg(target_os = "macos")]
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest the Security framework write may run. Like the read, it can wait on the macOS
/// keychain prompt, so the user needs time to answer it (#2892).
#[cfg(target_os = "macos")]
const WRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// Run one `security` call with a deadline. A timeout names the operation, the keychain item, and
/// the fix, because a locked keychain blocks the call with no output.
#[cfg(any(target_os = "macos", test))]
fn run_security(
    cmd: std::process::Command,
    timeout: Duration,
    operation: &str,
    service: &str,
) -> anyhow::Result<std::process::Output> {
    crate::process::run_traced("keychain:security", cmd, timeout).map_err(|e| {
        if e.kind() == std::io::ErrorKind::TimedOut {
            timeout_error(operation, service, timeout)
        } else {
            anyhow::anyhow!("running `security {operation}` for keychain item {service}: {e}")
        }
    })
}

/// The error for a keychain item that holds no usable credential. The fix names the delete
/// command, because a stale item from an older install is the usual cause.
#[cfg(any(target_os = "macos", test))]
fn bad_item_error(service: &str, reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "keychain item {service} is not a usable credential: {reason}. Fix: delete the item with \
         `security delete-generic-password -s {service}` and log in again."
    )
}

/// Turn the stdout of `security find-generic-password -w` into a credential.
///
/// A present item that does not parse is an error, not an absent item. `None` makes the caller
/// read the file credential, so the user would never learn that the keychain item is bad (#3067).
/// The error never quotes the item, because the item holds the token.
#[cfg(any(target_os = "macos", test))]
fn parse_keychain_blob(service: &str, raw: &str) -> anyhow::Result<super::Credentials> {
    let value: serde_json::Value = serde_json::from_str(raw.trim()).map_err(|e| {
        let reason = format!("not JSON (line {}, column {})", e.line(), e.column());
        bad_item_error(service, &reason)
    })?;
    super::Credentials::from_json(value)
        .ok_or_else(|| bad_item_error(service, "the JSON has no OAuth credential object"))
}

#[cfg(any(target_os = "macos", test))]
mod deadline;
#[cfg(any(target_os = "macos", test))]
#[cfg(all(test, target_os = "macos"))]
mod write_tests;
#[cfg(any(target_os = "macos", test))]
use deadline::{NoAnswer, timeout_error, with_deadline};
mod outcome;
#[cfg(any(target_os = "macos", test))]
use outcome::forget_outcome;

/// Drop the keychain credential belonging to a config dir that no longer exists.
///
/// Returns `true` when an item was deleted. No-op off macOS, where the
/// credential lives inside the folder and dies with it.
///
/// # Errors
/// Returns an error when `security` cannot be run, gives no answer within 10 s, or fails for a
/// reason other than "item not found" (for example, a locked keychain).
#[cfg(target_os = "macos")]
pub(crate) fn forget(config_dir: &Path) -> anyhow::Result<bool> {
    let service = super::keychain_service(config_dir);
    let account = keychain_account()?;
    // The exit status carries the answer: 44 is "not found", and `forget_outcome` maps every other
    // failure to an error. stderr is shown in that error, but never matched, because `security`
    // localizes its text (#1092).
    let mut cmd = std::process::Command::new(security_bin()?);
    cmd.args(["delete-generic-password", "-s", &service, "-a", &account])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let out = run_security(cmd, FORGET_TIMEOUT, "delete-generic-password", &service)?;
    forget_outcome(&service, &out.status, &String::from_utf8_lossy(&out.stderr))
}

/// Drop the keychain credential belonging to a config dir that no longer exists.
///
/// Returns `true` when an item was deleted. No-op off macOS, where the
/// credential lives inside the folder and dies with it.
///
/// # Errors
/// Never fails on this platform.
#[cfg(not(target_os = "macos"))]
pub(crate) fn forget(_config_dir: &Path) -> anyhow::Result<bool> {
    Ok(false)
}

/// Absolute path so a hijacked `PATH` cannot substitute the keychain tool.
#[cfg(any(target_os = "macos", test))]
const SECURITY_BIN: &str = "/usr/bin/security";

/// Overrides the `security` path. Only a `keychain-stub` build reads it, so a measurement can
/// point the read and delete at a stub (#3030). The write path uses the Security framework and
/// ignores it.
#[cfg(any(test, all(target_os = "macos", feature = "keychain-stub")))]
const SECURITY_BIN_ENV: &str = "LLMENV_SECURITY_BIN";

/// Picks the `security` binary: the override when set, else [`SECURITY_BIN`].
///
/// # Errors
/// Returns an error when the override is empty or is not an absolute path.
#[cfg(any(test, all(target_os = "macos", feature = "keychain-stub")))]
fn resolve_security_bin(override_path: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    match override_path {
        None => Ok(std::path::PathBuf::from(SECURITY_BIN)),
        Some("") => {
            anyhow::bail!("{SECURITY_BIN_ENV} is empty. Set it to an absolute path, or unset it")
        }
        Some(p) if Path::new(p).is_absolute() => Ok(std::path::PathBuf::from(p)),
        Some(p) => anyhow::bail!(
            "{SECURITY_BIN_ENV} is the relative path {p}; the keychain tool must be an absolute \
             path. Set {SECURITY_BIN_ENV} to an absolute path, or unset it"
        ),
    }
}

/// The `security` binary for the read and delete. A release build always uses [`SECURITY_BIN`].
#[cfg(all(target_os = "macos", not(feature = "keychain-stub")))]
fn security_bin() -> anyhow::Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(SECURITY_BIN))
}

/// Reads the environment override, then picks the `security` binary.
///
/// # Errors
/// Returns an error when the variable is not UTF-8 or the override is not absolute.
#[cfg(all(target_os = "macos", feature = "keychain-stub"))]
fn security_bin() -> anyhow::Result<std::path::PathBuf> {
    resolve_security_bin(crate::env_var::utf8_var(SECURITY_BIN_ENV)?.as_deref())
}

#[cfg(target_os = "macos")]
fn keychain_account() -> anyhow::Result<String> {
    crate::env_var::utf8_var("USER")?.ok_or_else(|| {
        anyhow::anyhow!("USER is unset; cannot address the keychain credential item")
    })
}

/// `security find-generic-password`'s exit code for "no matching item"
/// (errSecItemNotFound, OSStatus -25300) — verified empirically, since
/// `security` does not document its exit codes. Classifying on this rather
/// than on stderr text matters (#1092): `security` localizes its error
/// strings via `SecCopyErrorMessageString`, so on a non-English system a
/// string match would silently miss every failure and collapse back to the
/// same "absent" outcome the classification exists to avoid.
#[cfg(any(target_os = "macos", test))]
const SECURITY_ITEM_NOT_FOUND_EXIT_CODE: i32 = 44;

/// Why `security find-generic-password` failed — distinguishing these matters
/// because "genuinely absent" and "everything else" call for different
/// user-facing responses (#1092): a real problem (locked keychain,
/// permission error, …) should surface as an error, not collapse into the
/// same "no credential stored" outcome as a missing item.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeychainReadFailure {
    /// No matching item exists — the common, expected case
    /// ([`SECURITY_ITEM_NOT_FOUND_EXIT_CODE`]).
    Absent,
    /// Any other failure: locked keychain, permission error, I/O error, etc.
    /// Treated as a real problem rather than "no credential stored".
    Other,
}

/// Classify a failed `security find-generic-password` invocation from its
/// exit status. Pure function so it's testable without a real keychain.
#[cfg(target_os = "macos")]
fn classify_keychain_read_failure(status: &std::process::ExitStatus) -> KeychainReadFailure {
    if status.code() == Some(SECURITY_ITEM_NOT_FOUND_EXIT_CODE) {
        KeychainReadFailure::Absent
    } else {
        KeychainReadFailure::Other
    }
}

#[cfg(target_os = "macos")]
pub(super) fn keychain_read(config_dir: &Path) -> anyhow::Result<Option<super::Credentials>> {
    let service = super::keychain_service(config_dir);
    let account = keychain_account()?;
    let mut cmd = std::process::Command::new(security_bin()?);
    cmd.args([
        "find-generic-password",
        "-w",
        "-s",
        &service,
        "-a",
        &account,
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    // The call can wait on a macOS access prompt, so a person at the terminal is told what it
    // waits for (#2892). A hook has no reader for the line, so it is not printed there.
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        eprintln!(
            "llmenv: reading keychain item {service}. If macOS asks for keychain access, answer \
             the prompt."
        );
    } else {
        tracing::info!(
            "reading keychain item {service}, for at most {} s",
            READ_TIMEOUT.as_secs()
        );
    }
    let out = run_security(cmd, READ_TIMEOUT, "find-generic-password", &service)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return match classify_keychain_read_failure(&out.status) {
            KeychainReadFailure::Absent => {
                tracing::debug!("keychain item {service} not found: {}", stderr.trim());
                Ok(None)
            }
            KeychainReadFailure::Other => Err(anyhow::anyhow!(
                "keychain lookup failed for service {service} (status {}): {}. If the \
                 keychain is locked, unlock it (e.g. `security unlock-keychain`) and retry.",
                out.status,
                stderr.trim()
            )),
        };
    }
    // Deliberately not logged or surfaced — stdout is the token itself.
    let raw = String::from_utf8_lossy(&out.stdout);
    parse_keychain_blob(&service, &raw).map(Some)
}

/// Store a credential in the keychain via the Security framework directly
/// (#1061) — never through the `security` CLI's argv.
///
/// `security_framework::passwords::set_generic_password` wraps
/// `SecItemAdd`/`SecItemUpdate` (falling back to update on `errSecDuplicateItem`,
/// the same "create or update in place" semantics the old `-U` flag gave), with
/// the secret passed as an in-memory buffer. That closes both problems the CLI
/// approach had: the blob no longer sits in a child's argv (readable via `ps` by
/// other processes of the same user for the child's lifetime), and there's no
/// length limit to trip — the CLI's stdin form truncates at exactly 128 bytes
/// (measured), far short of a real ~510-byte credential blob, so it would have
/// silently stored a corrupt token with no error.
#[cfg(target_os = "macos")]
pub(super) fn keychain_write(config_dir: &Path, creds: &super::Credentials) -> anyhow::Result<()> {
    keychain_write_with(config_dir, creds, WRITE_TIMEOUT, set_generic_password)
}

/// The Security framework write for one item. The seam in [`keychain_write_with`] takes it as a
/// parameter, so a test can run the write logic without a real keychain.
#[cfg(target_os = "macos")]
fn set_generic_password(item: &str, account: &str, blob: &[u8]) -> Result<(), String> {
    security_framework::passwords::set_generic_password(item, account, blob)
        .map_err(|e| e.to_string())
}

/// Store `creds` with `set`, and wait at most `timeout` for it. A locked keychain blocks the write,
/// as it blocks the read, and export runs this (#2892).
#[cfg(target_os = "macos")]
fn keychain_write_with(
    config_dir: &Path,
    creds: &super::Credentials,
    timeout: Duration,
    set: fn(&str, &str, &[u8]) -> Result<(), String>,
) -> anyhow::Result<()> {
    let service = super::keychain_service(config_dir);
    let account = keychain_account()?;
    let blob = creds.to_blob()?;
    let item = service.clone();
    let answer = with_deadline(timeout, move || set(&item, &account, blob.as_bytes()));
    let written = match answer {
        Ok(written) => written,
        Err(NoAnswer::TimedOut) => {
            return Err(timeout_error("add-generic-password", &service, timeout));
        }
        Err(NoAnswer::WorkerLost) => {
            anyhow::bail!(
                "the keychain write for service {service} ended without an answer, so it may not \
                 be stored. Fix: run the command again."
            );
        }
    };
    written.map_err(|e| anyhow::anyhow!("writing keychain item for service {service}: {e}"))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_security_call_past_its_deadline_names_the_item_and_the_fix() {
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg("5");
        let err = run_security(
            cmd,
            Duration::from_millis(50),
            "find-generic-password",
            "svc-x",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("find-generic-password") && err.contains("svc-x"),
            "{err}"
        );
        assert!(err.contains("Fix: unlock the login keychain"), "{err}");
    }

    #[test]
    fn a_call_past_its_deadline_gives_no_answer_and_a_quick_one_does() {
        let slow = with_deadline(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(5));
            1
        });
        assert_eq!(slow, Err(NoAnswer::TimedOut));
        assert_eq!(with_deadline(Duration::from_secs(5), || 2), Ok(2));
    }

    #[test]
    #[expect(clippy::panic, reason = "test code: the worker must panic")]
    fn a_worker_that_panics_is_not_reported_as_a_timeout() {
        // The panic hook prints a backtrace before the sender drops. On a loaded machine that
        // can take longer than a short deadline, so the deadline must not race it.
        let lost = with_deadline(Duration::from_secs(60), || -> u8 {
            panic!("worker failed")
        });
        assert_eq!(lost, Err(NoAnswer::WorkerLost));
    }

    #[test]
    fn a_security_call_that_cannot_start_names_the_operation() {
        let cmd = std::process::Command::new("/nonexistent/security");
        let err = run_security(
            cmd,
            Duration::from_secs(1),
            "delete-generic-password",
            "svc-y",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("delete-generic-password") && err.contains("svc-y"),
            "{err}"
        );
    }

    // -- keychain_read failure classification (#1092) --

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(code << 8)
    }

    // -- an unreadable keychain item is an error, not an absent one (#3067) --

    #[test]
    fn an_item_that_is_not_json_is_an_error_naming_the_item_and_the_fix() {
        let err = parse_keychain_blob("svc-z", "not json")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("svc-z") && err.contains("line 1"), "{err}");
        assert!(err.contains("Fix: delete the item"), "{err}");
        assert!(err.contains("delete-generic-password -s svc-z"), "{err}");
    }

    #[test]
    fn an_item_that_is_json_without_an_oauth_object_is_an_error() {
        let err = parse_keychain_blob("svc-z", r#"{"other":1}"#)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("svc-z") && err.contains("OAuth"), "{err}");
    }

    #[test]
    fn an_item_with_an_oauth_object_parses_as_a_credential() {
        assert!(parse_keychain_blob("svc-z", r#"{"claudeAiOauth":{}}"#).is_ok());
    }

    // -- forget reports a failed delete as a failure, not as not found (#3068) --

    #[test]
    fn forget_exit_zero_is_deleted_and_exit_44_is_not_found() {
        assert!(forget_outcome("svc-x", &exit_status(0), "").unwrap());
        assert!(
            !forget_outcome("svc-x", &exit_status(SECURITY_ITEM_NOT_FOUND_EXIT_CODE), "").unwrap()
        );
    }

    #[test]
    fn forget_other_exit_codes_are_errors_naming_the_item_and_the_unlock_fix() {
        let err = forget_outcome(
            "svc-x",
            &exit_status(36),
            "User interaction is not allowed.",
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(
            err.contains("svc-x") && err.contains("unlock-keychain"),
            "{err}"
        );
        assert!(
            err.contains("User interaction is not allowed."),
            "stderr missing: {err}"
        );
        let silent = forget_outcome("svc-x", &exit_status(36), "")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            !silent.contains(": ."),
            "an empty stderr left a dangling detail: {silent}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classifies_item_not_found_exit_code_as_absent() {
        assert_eq!(
            classify_keychain_read_failure(&exit_status(SECURITY_ITEM_NOT_FOUND_EXIT_CODE)),
            KeychainReadFailure::Absent
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classifies_any_other_exit_code_as_other() {
        for code in [1, 36, 51, 2] {
            assert_eq!(
                classify_keychain_read_failure(&exit_status(code)),
                KeychainReadFailure::Other,
                "exit code {code} should classify as Other"
            );
        }
    }

    // -- security binary override (test seam for #3030) --

    #[test]
    fn the_security_binary_defaults_to_the_absolute_system_path() {
        assert_eq!(
            resolve_security_bin(None).unwrap(),
            Path::new("/usr/bin/security")
        );
    }

    #[test]
    fn an_absolute_security_override_replaces_the_default() {
        assert_eq!(
            resolve_security_bin(Some("/tmp/stub-security")).unwrap(),
            Path::new("/tmp/stub-security")
        );
    }

    #[test]
    fn a_relative_or_empty_security_override_is_rejected_with_the_fix() {
        for bad in ["security", "bin/security", ""] {
            let err = resolve_security_bin(Some(bad)).unwrap_err().to_string();
            assert!(
                err.contains("LLMENV_SECURITY_BIN") && err.contains("absolute"),
                "{err}"
            );
        }
    }
}

#[cfg(test)]
mod secret_tests {
    use super::parse_keychain_blob;
    use proptest::prelude::*;

    proptest! {
        /// An error from an unusable keychain item never quotes the token it was given.
        #[test]
        fn a_bad_item_error_never_quotes_the_token(
            token in "[A-Za-z0-9]{12,40}",
            cut in 0usize..200,
        ) {
            let full = format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}"}}}}"#);
            // A truncated item is not valid JSON, so it must fail.
            let raw: String = full.chars().take(cut).collect();
            if let Err(err) = parse_keychain_blob("svc-z", &raw) {
                prop_assert!(!format!("{err:#}").contains(&token), "{err:#}");
            }
        }

        /// Arbitrary item text never panics.
        #[test]
        fn arbitrary_item_text_never_panics(raw in ".{0,120}") {
            let _ = parse_keychain_blob("svc-z", &raw);
        }
    }
}
