use serde_json::json;

use super::{deny_reason, guarded_tools, is_tmp_path};

const RULE: &str = "Use \"$TMPDIR\" for scratch files. /tmp is not cleaned up.";

fn bash(command: &str) -> Option<String> {
    deny_reason(&json!({"tool_name": "Bash", "tool_input": {"command": command}}))
}

fn file_tool(tool: &str, key: &str, path: &str) -> Option<String> {
    deny_reason(&json!({"tool_name": tool, "tool_input": {key: path}}))
}

#[test]
fn a_bash_write_to_tmp_is_denied_with_the_tmpdir_rule() {
    let denied = [
        "echo hi > /tmp/out.txt",
        "echo hi >/tmp/out.txt",
        "echo hi >> /tmp/out.txt",
        "cmd 2>/tmp/err.log",
        "cmd &>/tmp/all.log",
        "cmd >| /tmp/force.txt",
        "echo hi | tee /tmp/out.txt",
        "echo hi | tee -a /tmp/out.txt",
        "cp build.log /tmp/build.log",
        "cp -r a b /tmp/dest",
        "cp -t /tmp a b",
        "cp --target-directory=/tmp a",
        "mv out.txt /tmp/out.txt",
        "install -m 644 a /tmp/a",
        "install -d /tmp/dir",
        "mkdir -p /tmp/work/sub",
        "touch /tmp/flag",
        "mktemp /tmp/scratch.XXXXXX",
        "mktemp -d -p /tmp",
        "mktemp --tmpdir=/tmp",
        "touch -r ref.txt /tmp/flag",
        "sudo tee /tmp/out.txt",
        "env FOO=1 cp a /tmp/a",
        "bash -c 'echo hi > /tmp/out.txt'",
        "echo $(touch /tmp/inner)",
        "cd repo && make 2> /private/tmp/make.log",
        "touch /var/folders/ab/cd/T/x",
        "touch /private/var/folders/ab/cd/T/x",
        "touch /tmp/../tmp/x",
        "touch /home/user/../../tmp/x",
        "touch //tmp//x",
        "touch /TMP/x",
        "touch '/tmp/with space'",
        "mkdir /tmp",
        "unzip a.zip -d /tmp",
        "unzip -qd /tmp/x a.zip",
        "for f in a b; do cp $f /tmp/; done",
        "if true; then touch /tmp/x; fi",
        "if false; then :; else touch /tmp/x; fi",
        "until false; do touch /tmp/x; done",
        "while true; do touch /tmp/x; break; done",
        "! touch /tmp/x",
        "case $x in a) touch /tmp/x ;; esac",
    ];
    for command in denied {
        let reason = bash(command).unwrap_or_else(|| panic!("allowed: {command}"));
        assert!(reason.contains(RULE), "{command}: {reason}");
    }
}

#[test]
fn the_reason_names_the_path_that_the_command_writes() {
    let reason = bash("cp a.txt /tmp/b.txt").unwrap();
    assert_eq!(
        reason,
        "llmenv blocked this call, because it writes to /tmp/b.txt. Use \"$TMPDIR\" for scratch \
         files. /tmp is not cleaned up."
    );
}

#[test]
fn a_bash_command_that_uses_tmpdir_or_reads_tmp_is_allowed() {
    let allowed = [
        "echo hi > \"$TMPDIR/out.txt\"",
        "echo hi > $TMPDIR/out.txt",
        "echo hi > \"${TMPDIR:-.}/out.txt\"",
        "cp a \"$TMPDIR/a\"",
        "mkdir -p \"$TMPDIR/work\"",
        "touch \"$TMPDIR/flag\"",
        "mktemp",
        "mktemp -t scratch.XXXXXX",
        "mktemp -d",
        "cat /tmp/out.txt",
        "cat /tmp/out.txt > result.txt",
        "cp /tmp/out.txt ./out.txt",
        "cp -r /tmp/a /tmp/b ./dest",
        "cp -t ./dest /tmp/a",
        "mv /tmp/out.txt ./out.txt",
        "touch -r /tmp/ref.txt ./flag",
        "ls /tmp",
        "cd /tmp",
        "grep -r needle /tmp/logs",
        "diff /tmp/a /tmp/b",
        "echo hi > /dev/null",
        "cmd 2>&1",
        "cmd 2>&1 | tee out.log",
        "echo hi > /tmpfoo/x",
        "echo hi > /var/log/x",
        "git commit -m \"docs: stop writing to >/tmp/x\"",
        "echo \"cp a /tmp/b\"",
        "cat <<EOF\nuse /tmp\nEOF",
    ];
    for command in allowed {
        assert_eq!(bash(command), None, "denied: {command}");
    }
}

#[test]
fn the_file_tools_are_denied_for_a_tmp_path_and_allowed_elsewhere() {
    for (tool, key) in [
        ("Write", "file_path"),
        ("Edit", "file_path"),
        ("MultiEdit", "file_path"),
        ("NotebookEdit", "notebook_path"),
    ] {
        let reason = file_tool(tool, key, "/tmp/notes.md").unwrap();
        assert!(reason.contains(RULE), "{tool}: {reason}");
        assert!(file_tool(tool, key, "/private/tmp/notes.md").is_some());
        assert!(file_tool(tool, key, "/var/folders/aa/bb/T/notes.md").is_some());
        assert_eq!(file_tool(tool, key, "/home/user/notes.md"), None);
        assert_eq!(file_tool(tool, key, "/tmpfoo/notes.md"), None);
        assert_eq!(file_tool(tool, key, "relative/notes.md"), None);
    }
}

#[test]
fn a_read_tool_and_an_unrelated_tool_are_allowed() {
    for tool in ["Read", "Grep", "Glob", "Agent", "mcp__icm__icm_wake_up"] {
        let payload = json!({"tool_name": tool, "tool_input": {"file_path": "/tmp/x"}});
        assert_eq!(deny_reason(&payload), None, "{tool}");
    }
}

#[test]
fn a_monitor_command_is_checked_like_a_bash_command() {
    let payload = json!({"tool_name": "Monitor", "tool_input": {"command": "echo x > /tmp/y"}});
    assert!(deny_reason(&payload).is_some());
}

#[test]
fn a_shell_call_with_no_command_string_is_denied() {
    for input in [json!({}), json!({"command": 7}), json!({"command": null})] {
        let reason = deny_reason(&json!({"tool_name": "Bash", "tool_input": input}))
            .unwrap_or_else(|| panic!("allowed: {input}"));
        assert!(reason.contains("missing from the call"), "{reason}");
    }
    let no_input = json!({"tool_name": "Bash"});
    assert!(deny_reason(&no_input).is_some());
}

#[test]
fn a_command_nested_past_the_limit_is_denied_with_the_fix() {
    let command = format!("{}touch x{}", "echo $(".repeat(40), ")".repeat(40));
    let reason = bash(&command).expect("a deeply nested command is denied");
    assert!(reason.contains("cannot be checked"), "{reason}");
}

#[test]
fn a_payload_with_no_tool_name_is_denied_with_the_cause() {
    for payload in [
        json!({}),
        json!({"tool_name": 3}),
        json!({"tool_name": null}),
    ] {
        let reason = deny_reason(&payload).unwrap_or_else(|| panic!("allowed: {payload}"));
        assert!(reason.contains("the call has no tool name"), "{reason}");
    }
}

#[test]
fn a_file_tool_call_with_no_path_string_is_denied() {
    for input in [json!({}), json!({"file_path": 7}), json!({"content": "x"})] {
        let reason = deny_reason(&json!({"tool_name": "Write", "tool_input": input}))
            .unwrap_or_else(|| panic!("allowed: {input}"));
        assert!(reason.contains("the file path is missing"), "{reason}");
    }
    assert!(deny_reason(&json!({"tool_name": "Edit"})).is_some());
}

#[test]
fn a_redirect_joined_to_the_word_before_it_is_denied() {
    let denied = [
        "echo hi>/tmp/x",
        "echo hi>>/tmp/x",
        "ls>/tmp/x",
        "cmd 2>/tmp/err",
        "cmd>|/tmp/x",
        "exec 3<>/tmp/x",
        "exec {fd}>/tmp/x",
        "echo \"a b\">/tmp/x",
        "echo a>b>/tmp/x",
        "(cd repo;ls>/tmp/x)",
    ];
    for command in denied {
        let reason = bash(command).unwrap_or_else(|| panic!("allowed: {command}"));
        assert!(reason.contains(RULE), "{command}: {reason}");
    }
}

#[test]
fn a_quoted_or_relative_redirect_sign_is_not_a_redirect_to_tmp() {
    let allowed = [
        "echo \"a>/tmp/x\"",
        "echo 'a>/tmp/x'",
        "echo a\\>/tmp/x",
        "echo hi>out.txt",
        "echo hi>\"$TMPDIR/x\"",
        "cmd 2>&1",
        "cmd >&2",
        "[ 3 -gt 2 ] && echo ok",
    ];
    for command in allowed {
        assert_eq!(bash(command), None, "denied: {command}");
    }
}

#[test]
fn the_matcher_names_the_shell_tools_and_the_file_tools() {
    let tools = guarded_tools();
    for tool in [
        "Bash",
        "Monitor",
        "Write",
        "Edit",
        "MultiEdit",
        "NotebookEdit",
    ] {
        assert!(tools.contains(&tool), "{tool}");
    }
}

#[test]
fn a_tmp_path_is_judged_on_its_resolved_components() {
    for path in [
        "/tmp",
        "/tmp/",
        "/tmp/a",
        "/a/../tmp/b",
        "/private/tmp/a",
        "/var/folders",
        "/var/tmp",
        "/var/tmp/a",
        "/private/var/tmp/a",
        "/dev/shm",
        "/dev/shm/seg",
    ] {
        assert!(is_tmp_path(path), "{path}");
    }
    for path in [
        "",
        "tmp/a",
        "./tmp/a",
        "/",
        "/tmp/../etc/x",
        "/tmpx",
        "/var",
        "/var/tmpx",
        "/dev",
        "/dev/null",
        "/dev/shmx",
    ] {
        assert!(!is_tmp_path(path), "{path}");
    }
}

#[test]
fn the_option_writers_are_denied_for_a_temp_target_and_allowed_for_other_targets() {
    let denied = [
        "git clone https://host/repo.git /tmp/repo",
        "git clone --depth 1 -b main https://host/repo.git /tmp/repo",
        "git -C /tmp clone https://host/repo.git",
        "git clone --separate-git-dir /tmp/g https://host/repo.git repo",
        "curl -o /tmp/x https://host/x",
        "curl -sSo /tmp/x https://host/x",
        "curl -o/tmp/x https://host/x",
        "curl --output /tmp/x https://host/x",
        "curl --output=/tmp/x https://host/x",
        "curl --output-dir /tmp -O https://host/x",
        "wget -O /tmp/x https://host/x",
        "wget -O/tmp/x https://host/x",
        "wget -P /tmp https://host/x",
        "wget --output-document=/tmp/x https://host/x",
        "wget --directory-prefix /tmp https://host/x",
        "dd if=/dev/zero of=/tmp/x bs=1 count=1",
        "ln -s ./target /tmp/link",
        "ln -sf a b /tmp",
        "rsync -a src/ /tmp/dest/",
        "rsync -a --delete src /var/tmp/dest",
        "tar -xzf a.tgz -C /tmp",
        "tar xzf a.tgz -C /tmp",
        "tar -xf a.tar --directory=/tmp",
        "tar --extract --directory /tmp -f a.tar",
        "touch /var/tmp/x",
        "echo hi > /dev/shm/x",
    ];
    for command in denied {
        let reason = bash(command).unwrap_or_else(|| panic!("allowed: {command}"));
        assert!(reason.contains(RULE), "{command}: {reason}");
    }
    let allowed = [
        "git clone https://host/repo.git",
        "git clone https://host/repo.git ./repo",
        "git clone --depth 1 https://host/tmp/repo.git repo",
        "git -C /tmp status",
        "git -C /tmp log",
        "curl -o ./x https://host/tmp/x",
        "curl -O https://host/x",
        "curl https://host/x -H 'X: /tmp/y'",
        "curl -d @/tmp/body https://host/x",
        "wget -O ./x https://host/x",
        "wget -qO- https://host/x",
        "wget -P ./dir https://host/x",
        "dd if=/tmp/x of=./y",
        "ln -s /tmp/x ./link",
        "rsync -a /tmp/src ./dest",
        "rsync -a src host:/tmp/dest",
        "tar -cf out.tar -C /tmp x",
        "tar -czf out.tgz -C /tmp x",
        "tar -tf /tmp/a.tar",
        "tar -xzf /tmp/a.tgz -C ./dir",
        "tar xzf /tmp/a.tgz",
    ];
    for command in allowed {
        assert_eq!(bash(command), None, "denied: {command}");
    }
}

// The review of #4053 reproduced each bypass in this list.
#[test]
fn the_reviewed_bypasses_are_denied_and_their_near_misses_are_allowed() {
    let denied = [
        "rsync -t src /tmp/dest",
        "cp src -t/tmp/x",
        "cp -rt /tmp/x src",
        "install -t/tmp/d a",
        "mv --target-dir=/tmp a",
        "git clone --upload-pack x url /tmp/dst",
        "git clone --server-option x url /tmp/d",
        "git clone --bundle-uri b url /tmp/r",
        "git clone --ref-format files url /tmp/r",
        "git -C /tmp -C . clone url",
        "git -C /home/u -C ../../tmp clone url",
        "git --config-env a=b clone url /tmp/x",
        "git clone --sep=/tmp/g url r",
        "wget --output-doc=/tmp/x url",
        "wget --direc=/tmp url",
        "wget -o /tmp/log url",
        "wget -a /tmp/log url",
        "curl -D /tmp/h url",
        "curl -c /tmp/jar url",
        "curl --dump-header=/tmp/h url",
        "curl --cookie-j /tmp/jar url",
        "tar czf /tmp/b.tgz dir",
        "tar xfC a.tar /tmp",
        "tar xCf /tmp/dir a.tar",
        "tar -xf a --direc=/tmp",
        "tar cf /tmp/a.tar x",
        "tar --create --file=/tmp/a.tar x",
        "tar -rf /tmp/a.tar x",
    ];
    for command in denied {
        let reason = bash(command).unwrap_or_else(|| panic!("allowed: {command}"));
        assert!(reason.contains(RULE), "{command}: {reason}");
    }
    let allowed = [
        "rsync -t /tmp/s ./d",
        "rsync -t src ./dst",
        "curl -uroot:pw https://h/x",
        "curl -A ua https://h/x",
        "curl -Hfoo https://h/x",
        "curl -XPOST -d @/tmp/b https://h/x",
        "git clone --upload-pack x url ./dst",
        "git -C /tmp clone url /home/u/x",
        "git clone /tmp/src ./dst",
        "tar tf /tmp/a.tar",
        "tar xzf /tmp/a.tgz -C ./d",
        "tar cf out.tar -C /tmp x",
        "tar -cf out.tar /tmp/x",
        "tar xf /tmp/a.tar",
    ];
    for command in allowed {
        assert_eq!(bash(command), None, "denied: {command}");
    }
}

#[test]
fn an_opencode_patch_is_denied_when_its_patch_text_names_a_temp_file() {
    let patch =
        |text: &str| deny_reason(&json!({"tool_name": "patch", "tool_input": {"patchText": text}}));
    for header in ["Add File", "Update File", "Delete File", "Move to"] {
        let text = format!("*** Begin Patch\n*** {header}: /tmp/x.txt\n*** End Patch");
        let reason = patch(&text).unwrap_or_else(|| panic!("allowed: {header}"));
        assert!(reason.contains(RULE), "{header}: {reason}");
    }
    assert_eq!(
        patch(
            "*** Begin Patch\n*** Add File: src/a.rs\n*** Update File: /home/u/b.rs\n*** End Patch"
        ),
        None
    );
    let missing = deny_reason(&json!({"tool_name": "patch", "tool_input": {}})).unwrap();
    assert!(missing.contains("the patch text is missing"), "{missing}");
}

#[test]
fn only_the_opencode_matcher_names_the_patch_tool() {
    assert!(!guarded_tools().contains(&"patch"));
    assert!(super::opencode_tools().contains(&"patch"));
}
