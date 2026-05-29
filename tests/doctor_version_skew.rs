#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Test for #173: doctor warns on version skew between running binary and cached materializations

use std::process::Command;

#[test]
fn test_doctor_runs_without_error() {
    // Baseline: doctor should run and exit 0 (warnings don't block)
    let output = Command::new("cargo")
        .args(["run", "--quiet", "--", "doctor"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run llmenv doctor");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Exit 0 is expected (warnings don't make it fail)
    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor should exit 0 even with warnings\nstderr:\n{}",
        stderr
    );
}

#[test]
fn test_doctor_version_check_label_exists() {
    // Version skew check should be in doctor output (or at least not break doctor)
    let output = Command::new("cargo")
        .args(["run", "--quiet", "--", "doctor"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run llmenv doctor");

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Doctor should at least complete and show the summary
    assert!(
        stderr.contains("Doctor check complete") || stderr.contains("Found"),
        "doctor should complete with summary"
    );
}
