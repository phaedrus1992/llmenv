//! Runs the real plugin under `node` with a fake opencode context, so the matcher, the deny
//! paths, the name mapping, and the context injection are tested as opencode would run them.

use super::*;

mod bus_events;
mod gates;

/// A fake opencode 2.x plugin context. It records the hook callbacks, sends one
/// `session.created` event, and then drives the callbacks the way opencode does.
const DRIVER: &str = r#"
const plugin = (await import(process.argv[1])).default;
const hooks = {};
const ctx = {
  location: { directory: process.argv[2] },
  tool: { hook: async (name, cb) => { hooks["tool." + name] = cb; } },
  session: { hook: async (name, cb) => { hooks["session." + name] = cb; } },
  event: { subscribe: () => (async function* () { yield { type: "session.created", data: { sessionID: "ses_1" } }; })() },
};
await plugin.setup(ctx);
const call = async (tool, input) => {
  const e = { tool, sessionID: "ses_1", agent: "build", messageID: "m", id: "c", input };
  try { await hooks["tool.execute.before"](e); return { ok: true, input: e.input }; }
  catch (err) { return { ok: false, reason: err.message }; }
};
await new Promise((r) => setTimeout(r, 300));
const out = {};
out.denied = await call("shell", { command: "drop x" });
out.allowed = await call("shell", { command: "ls" });
out.exit2 = await call("read", { path: "/etc/hosts" });
out.edit = await call("edit", { path: "/a", oldString: "x", newString: "y" });
await hooks["tool.execute.after"]({ tool: "codebase-memory-mcp_index_repository", sessionID: "ses_1", agent: "build", messageID: "m", id: "c2", input: { repo_path: "/r" }, status: "completed", result: { content: "ok" } });
await hooks["tool.execute.after"]({ tool: "codebase-memory-mcp_index_repository", sessionID: "ses_1", agent: "build", messageID: "m", id: "c3", input: { repo_path: "/r" }, status: "error", error: new Error("boom") });
const prompt = { sessionID: "ses_1", messageID: "m", prompt: { text: "hi" }, delivery: "immediate" };
await hooks["session.prompt"](prompt);
const context = { sessionID: "ses_1", system: [] };
await hooks["session.context"](context);
out.system = context.system.map((p) => p.text);
console.log(JSON.stringify(out));
"#;

fn hook(event: &str, matcher: Option<&str>, command: &str) -> crate::config::Hook {
    crate::config::Hook {
        event: event.into(),
        matcher: matcher.map(Into::into),
        handler: crate::config::HookHandler {
            kind: crate::config::HookHandlerKind::Command,
            command: Some(command.into()),
            tool: None,
            on_failure: None,
        },
        bundle_origin: None,
    }
}

fn run_node(script: &str, args: &[&std::path::Path]) -> serde_json::Value {
    let output = std::process::Command::new("node")
        .args(["--input-type=module", "-e", script])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("this test needs `node` on PATH: {e}"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "node failed: {stderr}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write_plugin(dir: &std::path::Path, table: &serde_json::Value) -> std::path::PathBuf {
    let plugin = dir.join("llmenv.js");
    std::fs::write(&plugin, PLUGIN_JS).unwrap();
    std::fs::write(dir.join("llmenv-hooks.json"), table.to_string()).unwrap();
    plugin
}

#[test]
fn the_plugin_runs_hooks_as_opencode_would() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("payloads.jsonl");
    let log_s = log.display();
    let failures = dir.path().join("failures.jsonl");
    let fail_s = failures.display();
    let deny = r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"no drop"}}"#;
    let hooks = vec![
        hook(
            "PreToolUse",
            Some("Bash"),
            &format!(
                "p=$(cat); echo \"$p\" >> {log_s}; case \"$p\" in *drop*) echo '{deny}';; esac"
            ),
        ),
        hook(
            "PreToolUse",
            Some("^Read$"),
            "echo 'read is blocked' >&2; exit 2",
        ),
        hook(
            "PostToolUse",
            None,
            &format!("cat >> {log_s}; echo >> {log_s}"),
        ),
        hook(
            "PostToolUseFailure",
            None,
            &format!("cat >> {fail_s}; echo >> {fail_s}"),
        ),
        hook(
            "UserPromptSubmit",
            None,
            r#"echo '{"hookSpecificOutput":{"additionalContext":"turn note"}}'"#,
        ),
        hook("SessionStart", None, "echo start note"),
    ];
    let mut table = hook_table(&hooks, &[], &["codebase-memory-mcp"]).unwrap();
    // Keep only the test hooks: the built-in hooks call an installed `llmenv`.
    table["hooks"]
        .as_array_mut()
        .unwrap()
        .retain(|h| !h["command"].as_str().unwrap().starts_with("llmenv "));
    let plugin = write_plugin(dir.path(), &table);
    let out = run_node(DRIVER, &[&plugin, dir.path()]);

    assert_eq!(out["denied"]["ok"], false);
    assert_eq!(out["denied"]["reason"], "no drop");
    assert_eq!(out["allowed"]["ok"], true);
    assert_eq!(out["exit2"]["reason"], "read is blocked");
    assert_eq!(out["edit"]["ok"], true, "no hook selects Edit");
    // A failed call goes to PostToolUseFailure only, as Claude Code sends it.
    let failed = std::fs::read_to_string(&failures).unwrap();
    assert!(
        failed.contains("\"hook_event_name\":\"PostToolUseFailure\""),
        "{failed}"
    );
    assert!(failed.contains("\"error\":\"boom\""), "{failed}");
    let posted = std::fs::read_to_string(&log).unwrap();
    assert!(
        !posted.contains("boom"),
        "a failed call reached PostToolUse: {posted}"
    );
    let system: Vec<&str> = out["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(system.contains(&"start note"), "{system:?}");
    assert!(system.contains(&"turn note"), "{system:?}");

    let payloads: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let first = &payloads[0];
    assert_eq!(first["tool_name"], "Bash");
    assert_eq!(first["session_id"], "ses_1");
    assert_eq!(first["cwd"], dir.path().display().to_string());
    assert!(
        payloads.iter().all(|p| p["tool_name"] != "Edit"),
        "the Bash matcher must not select Edit: {payloads:?}"
    );
    let post = payloads
        .iter()
        .find(|p| p["hook_event_name"] == "PostToolUse")
        .unwrap();
    assert_eq!(
        post["tool_name"],
        "mcp__codebase-memory-mcp__index_repository"
    );
}

#[test]
fn the_plugin_maps_file_arguments_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = write_plugin(dir.path(), &hook_table(&[], &[], &[]).unwrap());
    let driver = r#"
const m = await import(process.argv[1]);
const n = m.toNeutral("edit", { path: "/a", oldString: "x", newString: "y", other: 1 });
const back = m.fromNeutral("edit", { file_path: "/b", old_string: "p" });
console.log(JSON.stringify({ n, back, any: m.matches(undefined, "X"), star: m.matches("*", "X"), anchored: m.matches("Bash", "Bashful") }));
"#;
    let out = run_node(driver, &[&plugin]);
    assert_eq!(out["n"]["name"], "Edit");
    assert_eq!(
        out["n"]["input"],
        serde_json::json!({ "file_path": "/a", "old_string": "x", "new_string": "y", "other": 1 })
    );
    assert_eq!(
        out["back"],
        serde_json::json!({ "path": "/b", "oldString": "p" })
    );
    assert_eq!(
        (out["any"].as_bool(), out["star"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(out["anchored"], false, "a matcher selects the whole name");
}

#[test]
fn a_hook_that_cannot_start_is_reported_to_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let table = serde_json::json!({
        "hooks": [{ "event": "PreToolUse", "matcher": null, "command": "exit 7", "timeout": 5000 }],
        "tools": {},
        "mcpServers": {},
    });
    let plugin = write_plugin(dir.path(), &table);
    let driver = r#"
const m = await import(process.argv[1]);
const out = await m.runHooks("PreToolUse", "ses_1", {}, "Bash");
console.log(JSON.stringify(out));
"#;
    let out = run_node(driver, &[&plugin]);
    assert_eq!(out["block"], serde_json::Value::Null);
    assert!(
        out["errors"][0].as_str().unwrap().contains("exited with 7"),
        "{out}"
    );
}

fn table(hooks: serde_json::Value, servers: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "hooks": hooks, "tools": {}, "mcpServers": servers })
}

/// Server `a_b` is not read as server `a` with a tool named `b_x`.
#[test]
fn the_longest_mcp_namespace_wins() {
    let dir = tempfile::tempdir().unwrap();
    let servers = serde_json::json!({ "a": "a", "a_b": "a.b" });
    let plugin = write_plugin(dir.path(), &table(serde_json::json!([]), servers));
    let driver = r#"
const m = await import(process.argv[1]);
console.log(JSON.stringify({ long: m.toNeutral("a_b_x", {}).name, short: m.toNeutral("a_y", {}).name }));
"#;
    let out = run_node(driver, &[&plugin]);
    assert_eq!(out["long"], "mcp__a.b__x");
    assert_eq!(out["short"], "mcp__a__y");
}

/// A hook after a rewrite sees the rewritten input, so a guard checks what the tool will get.
#[test]
fn a_later_hook_sees_the_updated_input() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("seen.json");
    let rewrite = r#"echo '{"hookSpecificOutput":{"updatedInput":{"command":"rewritten"}}}'"#;
    let hooks = serde_json::json!([
        { "event": "PreToolUse", "matcher": null, "command": rewrite, "timeout": 5000 },
        { "event": "PreToolUse", "matcher": null, "command": format!("cat > {}", log.display()), "timeout": 5000 },
    ]);
    let plugin = write_plugin(dir.path(), &table(hooks, serde_json::json!({})));
    let driver = r#"
const m = await import(process.argv[1]);
const out = await m.runHooks("PreToolUse", "s", { tool_name: "Bash", tool_input: { command: "orig" } }, "Bash");
console.log(JSON.stringify(out));
"#;
    let out = run_node(driver, &[&plugin]);
    assert_eq!(out["updatedInput"]["command"], "rewritten");
    let seen: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
    assert_eq!(seen["tool_input"]["command"], "rewritten");
}

/// A gate that times out blocks the call. A hook whose child keeps the pipes open still ends at
/// its timeout, because the whole process group is stopped.
#[test]
fn a_timeout_stops_the_hook_and_a_gate_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = serde_json::json!([
        { "event": "PreToolUse", "matcher": "Bash", "command": "sleep 30 & sleep 30", "timeout": 300, "failClosed": true },
        { "event": "PostToolUse", "matcher": null, "command": "sleep 30 & sleep 30", "timeout": 300 },
    ]);
    let plugin = write_plugin(dir.path(), &table(hooks, serde_json::json!({})));
    let driver = r#"
const m = await import(process.argv[1]);
const started = Date.now();
const gate = await m.runHooks("PreToolUse", "s", {}, "Bash");
const post = await m.runHooks("PostToolUse", "s", {}, "Bash");
console.log(JSON.stringify({ gate, post, ms: Date.now() - started }));
"#;
    let out = run_node(driver, &[&plugin]);
    assert!(
        out["gate"]["block"].as_str().unwrap().contains("timed out"),
        "{out}"
    );
    assert_eq!(out["post"]["block"], serde_json::Value::Null);
    assert!(
        out["post"]["errors"][0]
            .as_str()
            .unwrap()
            .contains("timed out"),
        "{out}"
    );
    assert!(out["ms"].as_u64().unwrap() < 10_000, "{out}");
}

/// With no hook table, every request says that no llmenv hook runs, and names the fix.
#[test]
fn a_missing_hook_table_is_named_on_every_request() {
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("llmenv.js");
    std::fs::write(&plugin, PLUGIN_JS).unwrap();
    let driver = r#"
const m = await import(process.argv[1]);
console.log(JSON.stringify({ a: m.systemParts("s1"), b: m.systemParts("s1") }));
"#;
    let out = run_node(driver, &[&plugin]);
    for key in ["a", "b"] {
        let text = out[key][0]["text"].as_str().unwrap();
        assert!(
            text.contains("cannot be read") && text.contains("llmenv materialize"),
            "{out}"
        );
    }
}
