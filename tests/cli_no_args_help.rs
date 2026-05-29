#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Test for #180: llmenv with no args should show full help (#180)

use std::process::Command;

#[test]
fn test_llmenv_no_args_shows_help() {
    // Run `llmenv` with no subcommand
    // Use the compiled binary directly instead of cargo run to get cleaner output
    let binary_path = env!("CARGO_BIN_EXE_llmenv");
    let output = Command::new(binary_path)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run llmenv");

    // Should exit with non-zero (clap convention for help-like output when arg required)
    // Exit code 2 is clap's standard for arg_required_else_help
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 for no-args"
    );

    // Output (stderr or stdout) should contain the help text with command list
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stderr, stdout);

    // Should contain the usage line
    assert!(
        combined.contains("Usage:"),
        "help output should contain 'Usage:' line"
    );

    // Should list some commands (at least 'doctor' or 'bundle-ls')
    assert!(
        combined.contains("doctor")
            || combined.contains("bundle-ls")
            || combined.contains("scope-ls"),
        "help output should list available commands"
    );

    // Should NOT be the minimal stub we're replacing
    assert!(
        combined.contains("Commands:")
            && !combined.contains("Run 'llmenv --help' for more information."),
        "help output should be full help, not the minimal stub"
    );
}

#[test]
fn test_llmenv_help_flag_shows_help() {
    // Baseline: --help should work and show full help
    // Use the compiled binary directly instead of cargo run to get cleaner output
    let binary_path = env!("CARGO_BIN_EXE_llmenv");
    let output = Command::new(binary_path)
        .args(["--help"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run llmenv --help");

    // --help exits 0
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit code 0 for --help"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stderr, stdout);

    // Should contain help markers
    assert!(
        combined.contains("Usage:") && combined.contains("Commands:"),
        "help output should contain full command list"
    );
}
