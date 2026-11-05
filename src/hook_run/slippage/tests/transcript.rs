use super::*;

#[test]
fn answer_before_act_denies_while_a_question_is_open() {
    let file = transcript_with(&[user_line("should we roll this back?")]);
    let out = handle_transcript_scan(
        Some(&scan_cfg(true, false)),
        &bash_payload(&file, "git status"),
    );
    assert!(out.starts_with("__DENY__:"), "{out}");
    assert!(out.contains("answered"), "{out}");
}

#[test]
fn answer_before_act_allows_once_the_question_is_answered() {
    let file = transcript_with(&[
        user_line("should we roll this back?"),
        assistant_line("No — the failure is unrelated."),
    ]);
    assert_eq!(
        handle_transcript_scan(
            Some(&scan_cfg(true, false)),
            &bash_payload(&file, "git status")
        ),
        ""
    );
}

#[test]
fn explain_before_act_only_fires_for_modifying_commands() {
    let file = transcript_with(&[user_line("clean up the branch")]);
    assert_eq!(
        handle_transcript_scan(Some(&scan_cfg(false, true)), &bash_payload(&file, "ls -la")),
        "",
        "a read-only command needs no narration"
    );
    assert!(
        handle_transcript_scan(
            Some(&scan_cfg(false, true)),
            &bash_payload(&file, "chmod 777 build")
        )
        .starts_with("__DENY__:")
    );
}

#[test]
fn modifying_detection_looks_at_every_segment_and_through_sudo_and_git() {
    assert!(
        is_modifying("ls && truncate -s 0 log"),
        "a later segment counts"
    );
    assert!(is_modifying("sudo systemctl restart nginx"));
    assert!(is_modifying("git push --force"));
    assert!(
        is_modifying("/bin/chmod 600 file"),
        "an absolute path is the same command"
    );
    assert!(
        is_modifying("FOO=1 docker run x"),
        "env prefixes don't hide it"
    );
    assert!(
        !is_modifying("git status"),
        "read-only git is not modifying"
    );
    assert!(
        !is_modifying("grep -r chmod ."),
        "a command name as an argument is not a command"
    );
}

#[test]
fn modifying_detection_sees_writes_inside_substitutions_groups_and_subshells() {
    for command in [
        "echo $(rm -rf build)",
        "echo `rm -rf build`",
        "cat <(chmod 600 key)",
        "(rm -rf build)",
        "{ rm -rf build; }",
        "ls & rm -rf build",
        "echo \"$(truncate -s 0 log)\"",
    ] {
        assert!(is_modifying(command), "missed a write in {command:?}");
    }
}

#[test]
fn modifying_detection_fails_closed_on_nesting_past_the_bound() {
    let nested = format!("{}ls{}", "$(".repeat(40), ")".repeat(40));
    assert!(is_modifying(&nested));
}

#[test]
fn modifying_detection_sees_writes_behind_eval_and_timeout_wrappers() {
    for command in [
        "eval \"rm build\"",
        "timeout 5 rm build",
        "timeout -s KILL 5 rm build",
        "nice -n 5 rm build",
    ] {
        assert!(is_modifying(command), "missed a write in {command:?}");
    }
    assert!(!is_modifying("eval \"ls build\""));
    assert!(!is_modifying("timeout 5 ls build"));
    assert!(is_modifying("git --no-pager push"));
    assert!(is_modifying("git -C repo reset --hard"));
    assert!(!is_modifying("git -C repo status"));
}

#[test]
fn modifying_detection_reads_a_shell_script_passed_with_c() {
    assert!(is_modifying("bash -c 'cd out && rm -rf build'"));
    assert!(is_modifying("sh -c 'x; chmod 600 key'"));
    assert!(
        is_modifying("env rm x"),
        "a wrapper does not hide the command"
    );
}

#[test]
fn modifying_detection_sees_through_a_quoted_close_paren() {
    assert!(is_modifying(r#"echo "$(echo ")"; chmod 600 key)""#));
}

#[test]
fn transcript_layers_fail_open_without_a_usable_transcript() {
    let no_path = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": "chmod 777 x" },
    });
    assert_eq!(
        handle_transcript_scan(Some(&scan_cfg(true, true)), &no_path),
        ""
    );

    let missing = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": "chmod 777 x" },
        "transcript_path": "/nonexistent/llmenv/t.jsonl",
    });
    assert_eq!(
        handle_transcript_scan(Some(&scan_cfg(true, true)), &missing),
        ""
    );
}

#[test]
fn transcript_layers_are_off_by_default() {
    let file = transcript_with(&[user_line("are you sure?")]);
    assert_eq!(
        handle_transcript_scan(
            Some(&SlippageControl {
                enabled: true,
                ..Default::default()
            }),
            &bash_payload(&file, "chmod 777 x")
        ),
        "",
        "both layers are opt-in even with the feature on"
    );
}
// `path_key` collapsing every path to one value would make the layer
// authorise any write after any read. The canonicalisation test can't see
// that — both sides map through the same function, so a constant still
// matches itself.
