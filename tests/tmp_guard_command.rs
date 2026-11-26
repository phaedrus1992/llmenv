#![expect(clippy::expect_used, reason = "test scaffolding")]
//! `llmenv tmp-guard` denies a tool call that writes under `/tmp` (#4045).

mod support;

use support::isolated_llmenv_cmd;

const RULE: &str = "Use \\\"$TMPDIR\\\" for scratch files. /tmp is not cleaned up.";

fn run(payload: &str) -> (bool, String, String) {
    run_for("claude_code", payload)
}

fn run_for(engine: &str, payload: &str) -> (bool, String, String) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let output = isolated_llmenv_cmd(dir.path())
        .args(["tmp-guard", "--engine", engine])
        .write_stdin(payload.to_string())
        .output()
        .expect("run llmenv");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_bash_write_to_tmp_prints_a_deny_envelope_and_exits_zero() {
    let (ok, stdout, stderr) =
        run(r#"{"tool_name":"Bash","tool_input":{"command":"echo hi > /tmp/out.txt"}}"#);
    assert!(ok, "tmp-guard failed: {stderr}");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "{stdout}"
    );
    assert!(stdout.contains(RULE), "{stdout}");
}

#[test]
fn the_same_command_with_tmpdir_prints_nothing() {
    let (ok, stdout, stderr) =
        run(r#"{"tool_name":"Bash","tool_input":{"command":"echo hi > \"$TMPDIR/out.txt\""}}"#);
    assert!(ok, "tmp-guard failed: {stderr}");
    assert_eq!(stdout, "", "{stderr}");
}

#[test]
fn a_write_tool_call_for_a_tmp_path_is_denied() {
    let (ok, stdout, _) = run(r#"{"tool_name":"Write","tool_input":{"file_path":"/tmp/a.md"}}"#);
    assert!(ok);
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "{stdout}"
    );
}

#[test]
fn a_read_of_tmp_prints_nothing() {
    let (ok, stdout, _) = run(r#"{"tool_name":"Read","tool_input":{"file_path":"/tmp/a.md"}}"#);
    assert!(ok);
    assert_eq!(stdout, "");
    let (ok, stdout, _) = run(r#"{"tool_name":"Bash","tool_input":{"command":"cat /tmp/a.md"}}"#);
    assert!(ok);
    assert_eq!(stdout, "");
}

#[test]
fn a_payload_that_is_not_json_is_denied_with_the_cause() {
    let (ok, stdout, stderr) = run("not json");
    assert!(ok, "tmp-guard failed: {stderr}");
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "{stdout}"
    );
    assert!(stdout.contains("the hook payload is not JSON"), "{stdout}");
}

#[test]
fn each_engine_denies_a_tmp_write_in_its_own_tool_names() {
    let shell = r#"{"tool_name":"Bash","tool_input":{"command":"echo hi > /tmp/x"}}"#;
    let (ok, stdout, _) = run_for("opencode", shell);
    assert!(ok);
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "{stdout}"
    );

    let crush = r#"{"tool_name":"bash","tool_input":{"command":"echo hi > /tmp/x"}}"#;
    let (ok, stdout, _) = run_for("crush", crush);
    assert!(ok);
    assert!(
        stdout.contains(r#""permissionDecision":"deny""#),
        "{stdout}"
    );

    let allowed = r#"{"tool_name":"bash","tool_input":{"command":"echo hi > \"$TMPDIR/x\""}}"#;
    let (ok, stdout, _) = run_for("crush", allowed);
    assert!(ok);
    assert_eq!(stdout, "");
}
