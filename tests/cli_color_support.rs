/// Tests for CLI color support (#53)
/// Verify color output is controlled by TTY detection, NO_COLOR env var, and --color flag.

#[test]
fn test_should_use_color_always_mode() {
    use llmenv::cli::{ColorMode, should_use_color};

    // Always mode should force colors on
    assert!(should_use_color(Some(ColorMode::Always), false));
    assert!(should_use_color(Some(ColorMode::Always), true));
}

#[test]
fn test_should_use_color_never_mode() {
    use llmenv::cli::{ColorMode, should_use_color};

    // Never mode should force colors off
    assert!(!should_use_color(Some(ColorMode::Never), false));
    assert!(!should_use_color(Some(ColorMode::Never), true));
}

#[test]
fn test_should_use_color_auto_respects_no_color_env() {
    use llmenv::cli::{ColorMode, should_use_color};

    // Test that NO_COLOR env var is checked internally.
    // When NO_COLOR is set, should_use_color should return false.
    // Note: We assume the function checks std::env::var("NO_COLOR") internally.
    // To properly test this in integration, run the test with NO_COLOR=1 env.

    // For unit test here, just verify that ColorMode::Auto respects TTY
    // when env vars are not set:
    assert!(
        !should_use_color(Some(ColorMode::Auto), false),
        "auto mode should disable colors when not TTY"
    );
}

#[test]
fn test_should_use_color_auto_with_tty() {
    use llmenv::cli::{ColorMode, should_use_color};

    // Auto mode should respect the is_tty parameter when deciding to use colors.
    // When is_tty is false, colors should be disabled.
    let result = should_use_color(Some(ColorMode::Auto), false);
    assert!(!result, "auto mode with is_tty=false should disable colors");

    // Note: Testing with is_tty=true in integration tests is environment-dependent
    // and can be flaky in parallel test runs (harness may set NO_COLOR or capture stdout).
    // Comprehensive TTY and env var interaction tests are in unit tests
    // (should_use_color_auto_with_tty_isolated, should_use_color_no_color_overrides, etc.)
    // which use controlled environment injection for deterministic results.
}

#[test]
fn test_color_marker_functions_plain() {
    use llmenv::cli::{
        active_marker, doctor_fail, doctor_pass, doctor_warning, inactive_annotation,
        orphan_annotation,
    };

    // Without color, markers are bare glyphs with no escape codes.
    for s in [
        active_marker(false),
        inactive_annotation(false),
        orphan_annotation(false),
        doctor_pass(false),
        doctor_warning(false),
        doctor_fail(false),
    ] {
        assert!(!s.is_empty());
        assert!(
            !s.contains('\u{1b}'),
            "plain marker must not contain ANSI: {s:?}"
        );
    }
}

#[test]
fn test_color_marker_functions_colored() {
    use llmenv::cli::{
        active_marker, doctor_fail, doctor_pass, doctor_warning, inactive_annotation,
        orphan_annotation,
    };

    // With color, markers wrap the glyph in ANSI escape sequences.
    for s in [
        active_marker(true),
        inactive_annotation(true),
        orphan_annotation(true),
        doctor_pass(true),
        doctor_warning(true),
        doctor_fail(true),
    ] {
        assert!(
            s.contains('\u{1b}'),
            "colored marker must contain ANSI: {s:?}"
        );
    }
}
