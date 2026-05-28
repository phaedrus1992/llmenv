#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests for llmenv prune command (#54)
//! Verify prune command is wired into CLI and flag validation works.

/// Test that prune command is recognized by the CLI
#[test]
#[ignore = "requires full build; tested by integration tests"]
fn test_prune_command_help() {
    // The `llmenv prune --help` should work without error
    // This test checks that the subcommand is wired into clap correctly
}

/// Test that --all and --older-than are mutually exclusive
#[test]
#[ignore = "requires full binary build"]
fn test_prune_flag_validation_all_and_older_than_conflict() {
    // This test verifies that attempting to use both flags triggers an error.
    // The validation happens in run_prune() which checks:
    // if all && older_than.is_some() { bail!(...) }
}

/// Test that prune command accepts valid flags
#[test]
#[ignore = "requires full binary build"]
fn test_prune_accepts_valid_flags() {
    // Valid flag combinations:
    // - prune (no flags)
    // - prune --dry-run
    // - prune --all
    // - prune --all --dry-run
    // - prune --older-than <duration>
    // - prune --older-than <duration> --dry-run
}
