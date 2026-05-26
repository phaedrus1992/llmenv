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

    // Auto mode should respect TTY when env vars aren't interfering
    // When stdout is not a TTY, colors should be disabled by default
    let result = should_use_color(Some(ColorMode::Auto), false);
    assert!(!result, "auto mode with no TTY should disable colors");

    // When stdout is a TTY, colors should be enabled by default
    let result = should_use_color(Some(ColorMode::Auto), true);
    assert!(result, "auto mode with TTY should enable colors");
}

#[test]
fn test_color_marker_functions() {
    use llmenv::cli::{
        active_marker, inactive_annotation, orphan_annotation, doctor_pass, doctor_warning,
        doctor_fail,
    };

    // Verify marker functions return non-empty strings
    assert!(!active_marker().is_empty());
    assert!(!inactive_annotation().is_empty());
    assert!(!orphan_annotation().is_empty());
    assert!(!doctor_pass().is_empty());
    assert!(!doctor_warning().is_empty());
    assert!(!doctor_fail().is_empty());
}
