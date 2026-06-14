#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
#![expect(clippy::panic, reason = "test scaffolding")]
//! Tests for #338: shell hook guards — non-interactive skip and already-active skip.
//!
//! The `llmenv hook --shell <zsh|bash>` command emits shell function code that is eval'd
//! into the user's shell.  Two early-return guards must appear inside each hook function:
//!
//! 1. **Non-interactive guard**: `[[ $- != *i* ]] && return` — skips render entirely when
//!    the shell is non-interactive (e.g. Claude Code's Bash tool subshells).
//! 2. **Already-active guard**: `[[ -n "$LLMENV_STATE_DIR" ]] && return` — skips render
//!    when the environment is already active in the parent shell.
//!
//! Both guards must appear *inside* the function body, *before* the `source <(llmenv export)`
//! line, so they short-circuit before any render work is done.

use assert_cmd::Command;

/// The non-interactive guard must appear inside the zsh hook function.
#[test]
fn zsh_hook_has_non_interactive_guard() {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    let output = cmd.args(["hook", "zsh"]).output().unwrap();

    assert!(output.status.success(), "llmenv hook zsh should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // The guard must be inside the function body, before the source line.
    let fn_body = extract_function_body(&stdout, "__llmenv_precmd");
    assert!(
        fn_body.contains("[[ $- != *i* ]] && return"),
        "zsh hook missing non-interactive guard in __llmenv_precmd body.\nGot:\n{fn_body}"
    );
}

/// The already-active guard must appear inside the zsh hook function.
#[test]
fn zsh_hook_has_already_active_guard() {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    let output = cmd.args(["hook", "zsh"]).output().unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_precmd");

    assert!(
        fn_body.contains("[[ -n \"$LLMENV_STATE_DIR\" ]] && return"),
        "zsh hook missing already-active guard in __llmenv_precmd body.\nGot:\n{fn_body}"
    );
}

/// Guards must appear before `source <(llmenv export)` in the zsh hook.
#[test]
fn zsh_hook_guards_precede_source() {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    let output = cmd.args(["hook", "zsh"]).output().unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_precmd");

    let non_interactive_pos = fn_body.find("[[ $- != *i* ]]");
    let already_active_pos = fn_body.find("[[ -n \"$LLMENV_STATE_DIR\" ]]");
    let source_pos = fn_body.find("source <(llmenv export)");

    let ni = non_interactive_pos.expect("non-interactive guard not found in zsh function body");
    let aa = already_active_pos.expect("already-active guard not found in zsh function body");
    let src = source_pos.expect("source line not found in zsh function body");

    assert!(
        ni < src,
        "non-interactive guard must come before source line in zsh hook"
    );
    assert!(
        aa < src,
        "already-active guard must come before source line in zsh hook"
    );
}

/// The non-interactive guard must appear inside the bash hook function.
#[test]
fn bash_hook_has_non_interactive_guard() {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    let output = cmd.args(["hook", "bash"]).output().unwrap();

    assert!(output.status.success(), "llmenv hook bash should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_prompt");

    assert!(
        fn_body.contains("[[ $- != *i* ]] && return"),
        "bash hook missing non-interactive guard in __llmenv_prompt body.\nGot:\n{fn_body}"
    );
}

/// The already-active guard must appear inside the bash hook function.
#[test]
fn bash_hook_has_already_active_guard() {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    let output = cmd.args(["hook", "bash"]).output().unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_prompt");

    assert!(
        fn_body.contains("[[ -n \"$LLMENV_STATE_DIR\" ]] && return"),
        "bash hook missing already-active guard in __llmenv_prompt body.\nGot:\n{fn_body}"
    );
}

/// Guards must appear before `source <(llmenv export)` in the bash hook.
#[test]
fn bash_hook_guards_precede_source() {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    let output = cmd.args(["hook", "bash"]).output().unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_prompt");

    let ni = fn_body
        .find("[[ $- != *i* ]]")
        .expect("non-interactive guard not found in bash function body");
    let aa = fn_body
        .find("[[ -n \"$LLMENV_STATE_DIR\" ]]")
        .expect("already-active guard not found in bash function body");
    let src = fn_body
        .find("source <(llmenv export)")
        .expect("source line not found in bash function body");

    assert!(
        ni < src,
        "non-interactive guard must come before source line in bash hook"
    );
    assert!(
        aa < src,
        "already-active guard must come before source line in bash hook"
    );
}

/// Interactive shells (no sentinel var set) must still wire up the hook registration
/// — i.e. the `precmd_functions` / `PROMPT_COMMAND` wiring is still emitted.
#[test]
fn zsh_hook_still_registers_precmd_function() {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    let output = cmd.args(["hook", "zsh"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("precmd_functions+=(\"__llmenv_precmd\")"),
        "zsh hook should still register __llmenv_precmd in precmd_functions"
    );
}

/// Interactive shells (no sentinel var set) must still wire up PROMPT_COMMAND.
#[test]
fn bash_hook_still_registers_prompt_command() {
    let mut cmd = Command::cargo_bin("llmenv").unwrap();
    let output = cmd.args(["hook", "bash"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("PROMPT_COMMAND=\"__llmenv_prompt;$PROMPT_COMMAND\""),
        "bash hook should still register __llmenv_prompt in PROMPT_COMMAND"
    );
}

/// Extract the body of a shell function `fn_name() { ... }` from the given output.
/// Returns only the lines between the opening `{` and closing `}`.
fn extract_function_body(output: &str, fn_name: &str) -> String {
    let start_marker = format!("{fn_name}() {{");
    let mut in_body = false;
    let mut body_lines: Vec<&str> = Vec::new();

    for line in output.lines() {
        if !in_body {
            if line.trim_start().starts_with(&start_marker) || line.trim_start() == start_marker {
                in_body = true;
            }
            continue;
        }
        // Closing brace on its own line ends the function.
        if line.trim() == "}" {
            break;
        }
        body_lines.push(line);
    }

    body_lines.join("\n")
}
