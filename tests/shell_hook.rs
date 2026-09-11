#![expect(clippy::unwrap_used, reason = "test scaffolding")]
#![expect(clippy::expect_used, reason = "test scaffolding")]
//! Tests for #338/#1895: shell hook guards — non-interactive skip and
//! stale-scope skip.
//!
//! The `llmenv hook --shell <zsh|bash>` command emits shell function code that is eval'd
//! into the user's shell.  Two early-return guards must appear inside each hook function:
//!
//! 1. **Non-interactive guard**: `[[ $- != *i* ]] && return` — skips render entirely when
//!    the shell is non-interactive (e.g. Claude Code's Bash tool subshells).
//! 2. **Already-active guard**: skips render only while `$PWD` is still inside the
//!    project root the inherited environment was resolved for (or no project root was
//!    recorded at all). A child shell forked into a different project directory —
//!    terminal multiplexers like tmux/zellij, or a tool like herdr that forks panes off
//!    a parent shell — no longer matches on `$PWD` and falls through to re-export (#1895).
//!
//! Both guards must appear *inside* the function body, *before* the `source <(llmenv export)`
//! line, so they short-circuit before any render work is done.

use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

mod support;

/// The literal already-active guard line `emit_hook_guards` emits.
const ALREADY_ACTIVE_GUARD: &str = "[[ -n \"$LLMENV_STATE_DIR\" && ( -z \"$LLMENV_PROJECT_ROOT\" || \"$PWD\"/ == \"$LLMENV_PROJECT_ROOT\"/* ) ]] && return";

/// The non-interactive guard must appear inside the zsh hook function.
#[test]
fn zsh_hook_has_non_interactive_guard() {
    let home = TempDir::new().unwrap();
    let mut cmd = support::isolated_llmenv_cmd(home.path());
    cmd.timeout(Duration::from_secs(10));
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
    let home = TempDir::new().unwrap();
    let mut cmd = support::isolated_llmenv_cmd(home.path());
    cmd.timeout(Duration::from_secs(10));
    let output = cmd.args(["hook", "zsh"]).output().unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_precmd");

    assert!(
        fn_body.contains(ALREADY_ACTIVE_GUARD),
        "zsh hook missing already-active guard in __llmenv_precmd body.\nGot:\n{fn_body}"
    );
}

/// Guards must appear before `source <(llmenv export)` in the zsh hook.
#[test]
fn zsh_hook_guards_precede_source() {
    let home = TempDir::new().unwrap();
    let mut cmd = support::isolated_llmenv_cmd(home.path());
    cmd.timeout(Duration::from_secs(10));
    let output = cmd.args(["hook", "zsh"]).output().unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_precmd");

    let non_interactive_pos = fn_body.find("[[ $- != *i* ]]");
    let already_active_pos = fn_body.find(ALREADY_ACTIVE_GUARD);
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
    let home = TempDir::new().unwrap();
    let mut cmd = support::isolated_llmenv_cmd(home.path());
    cmd.timeout(Duration::from_secs(10));
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
    let home = TempDir::new().unwrap();
    let mut cmd = support::isolated_llmenv_cmd(home.path());
    cmd.timeout(Duration::from_secs(10));
    let output = cmd.args(["hook", "bash"]).output().unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_prompt");

    assert!(
        fn_body.contains(ALREADY_ACTIVE_GUARD),
        "bash hook missing already-active guard in __llmenv_prompt body.\nGot:\n{fn_body}"
    );
}

/// Guards must appear before `source <(llmenv export)` in the bash hook.
#[test]
fn bash_hook_guards_precede_source() {
    let home = TempDir::new().unwrap();
    let mut cmd = support::isolated_llmenv_cmd(home.path());
    cmd.timeout(Duration::from_secs(10));
    let output = cmd.args(["hook", "bash"]).output().unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let fn_body = extract_function_body(&stdout, "__llmenv_prompt");

    let ni = fn_body
        .find("[[ $- != *i* ]]")
        .expect("non-interactive guard not found in bash function body");
    let aa = fn_body
        .find(ALREADY_ACTIVE_GUARD)
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
    let home = TempDir::new().unwrap();
    let mut cmd = support::isolated_llmenv_cmd(home.path());
    cmd.timeout(Duration::from_secs(10));
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
    let home = TempDir::new().unwrap();
    let mut cmd = support::isolated_llmenv_cmd(home.path());
    cmd.timeout(Duration::from_secs(10));
    let output = cmd.args(["hook", "bash"]).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("PROMPT_COMMAND=\"__llmenv_prompt;$PROMPT_COMMAND\""),
        "bash hook should still register __llmenv_prompt in PROMPT_COMMAND"
    );
}

/// Runs the already-active guard inside a real bash function with the given
/// env vars and working directory, and reports whether it fell through
/// (`true`) or returned early on the fast path (`false`).
fn guard_falls_through(
    state_dir: Option<&str>,
    project_root: Option<&str>,
    cwd: &std::path::Path,
) -> bool {
    let script = format!("f() {{ {ALREADY_ACTIVE_GUARD}; echo FELL_THROUGH; }}; f");
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(&script).current_dir(cwd);
    cmd.env_remove("LLMENV_STATE_DIR")
        .env_remove("LLMENV_PROJECT_ROOT");
    if let Some(d) = state_dir {
        cmd.env("LLMENV_STATE_DIR", d);
    }
    if let Some(r) = project_root {
        cmd.env("LLMENV_PROJECT_ROOT", r);
    }
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).contains("FELL_THROUGH")
}

/// #1895 repro: a child shell that inherited `LLMENV_STATE_DIR` and
/// `LLMENV_PROJECT_ROOT` resolved for a *different* project directory must
/// fall through and re-export, not trust the stale environment.
#[test]
fn stale_guard_falls_through_when_pwd_leaves_the_recorded_project_root() {
    let project_dir = TempDir::new().unwrap();
    let other_dir = TempDir::new().unwrap();
    let project_root = std::fs::canonicalize(project_dir.path()).unwrap();
    let other_cwd = std::fs::canonicalize(other_dir.path()).unwrap();

    assert!(
        guard_falls_through(
            Some("/some/state"),
            Some(project_root.to_str().unwrap()),
            &other_cwd,
        ),
        "guard must fall through when $PWD left the recorded project root"
    );
}

/// The fast path is still taken when `$PWD` is still under the recorded
/// project root — this is the redundant-render avoidance #338 exists for.
#[test]
fn guard_skips_when_pwd_still_under_recorded_project_root() {
    let project_dir = TempDir::new().unwrap();
    let project_root = std::fs::canonicalize(project_dir.path()).unwrap();
    let subdir = project_root.join("nested");
    std::fs::create_dir(&subdir).unwrap();

    assert!(
        !guard_falls_through(
            Some("/some/state"),
            Some(project_root.to_str().unwrap()),
            &subdir,
        ),
        "guard must stay on the fast path while $PWD is still under the project root"
    );
}

/// `$PWD` sitting exactly at the recorded project root (no subdirectory) —
/// the most common real-world case — must also stay on the fast path.
#[test]
fn guard_skips_when_pwd_is_exactly_the_recorded_project_root() {
    let project_dir = TempDir::new().unwrap();
    let project_root = std::fs::canonicalize(project_dir.path()).unwrap();

    assert!(
        !guard_falls_through(
            Some("/some/state"),
            Some(project_root.to_str().unwrap()),
            &project_root,
        ),
        "guard must stay on the fast path when $PWD equals the project root exactly"
    );
}

/// No project scope was ever active (`$LLMENV_PROJECT_ROOT` unset) — nothing
/// to compare `$PWD` against, so the guard keeps the old presence-only
/// behavior and stays on the fast path.
#[test]
fn guard_skips_when_no_project_root_was_recorded() {
    let cwd = TempDir::new().unwrap();
    assert!(
        !guard_falls_through(Some("/some/state"), None, cwd.path()),
        "guard must stay on the fast path when no project root was ever recorded"
    );
}

/// `$LLMENV_STATE_DIR` unset entirely — the environment was never
/// activated, so the guard must fall through regardless of `$LLMENV_PROJECT_ROOT`.
#[test]
fn guard_falls_through_when_state_dir_unset() {
    let cwd = TempDir::new().unwrap();
    assert!(
        guard_falls_through(None, None, cwd.path()),
        "guard must fall through when LLMENV_STATE_DIR was never set"
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
