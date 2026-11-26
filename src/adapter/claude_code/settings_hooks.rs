//! The `hooks` object of the Claude Code `settings.json` (#90, #121, #289, #317, #382, #499, #985,
//! #2249, #2456).
//!
//! The user hooks come first, then the built-in hooks, in the order that the registrations are
//! written below. An event keeps its entries in that order.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use crate::adapter::claude_code::commands::{
    config_context_command, hook_run_command, throttle_command,
};
use crate::adapter::claude_code::{
    ADAPTIVE_RECALL_HOOK_EVENTS, BASELINE_HOOK_EVENTS, SESSION_LOG_HOOK_EVENTS, dedup_hooks_doc,
    guard_hooks, hook_json, overlay_native, tools_matcher, unrouted_tools, version,
};
use crate::config::SlippageControl;
use crate::merge::MergedManifest;

type HooksByEvent = BTreeMap<String, Vec<serde_json::Value>>;

fn push(hooks: &mut HooksByEvent, event: &str, entry: serde_json::Value) {
    hooks.entry(event.to_string()).or_default().push(entry);
}

/// An entry with no matcher, which runs `command` for every call of the event.
fn command_entry(command: &str) -> serde_json::Value {
    json!({ "hooks": [{ "type": "command", "command": command }] })
}

/// An entry that runs `command` for the tools that `matcher` selects.
fn matched_entry(matcher: &str, command: &str) -> serde_json::Value {
    json!({ "matcher": matcher, "hooks": [{ "type": "command", "command": command }] })
}

/// The `pre_tool_use` command of `hook-run`.
fn pre_tool_use_command() -> anyhow::Result<String> {
    Ok(format!("{} pre_tool_use", hook_run_command()?))
}

/// One registration step: it adds its entries to the hooks of the event.
type Step = fn(&MergedManifest, &mut HooksByEvent) -> anyhow::Result<()>;

/// The built-in registrations, in the order that an event keeps its entries. A table keeps the
/// function flat, because each `?` would count as a branch.
const STEPS: [Step; 9] = [
    session_context_hook,
    register_guards,
    slippage_hooks,
    pre_tool_use_matcher_hooks,
    tool_routing_hooks,
    lifecycle_hooks,
    prompt_hooks,
    session_log_hooks,
    // After every other registration, so it can see them.
    task_nudge_hooks,
];

/// The hooks object that `settings.json` holds: the user hooks and the built-in hooks, with the
/// `native_hooks` overlay of the engine on top, and no entry twice.
///
/// # Errors
/// Fails when a hook command cannot be resolved, or the overlay is not a mapping.
pub(super) fn render_hooks(
    out: &Path,
    manifest: &MergedManifest,
) -> anyhow::Result<serde_json::Value> {
    let mut hooks = HooksByEvent::new();
    user_hooks(out, manifest, &mut hooks)?;
    for step in STEPS {
        step(manifest, &mut hooks)?;
    }
    finish(manifest, hooks)
}

/// #289: inject source config paths at session start, so the agent edits llmenv config and does
/// not touch the managed cache files. The drift check of #85 runs in `hook-run session_start`
/// (#741).
fn session_context_hook(_: &MergedManifest, hooks: &mut HooksByEvent) -> anyhow::Result<()> {
    push(
        hooks,
        "SessionStart",
        command_entry(&config_context_command()?),
    );
    Ok(())
}

/// The config guard (#289) and the /tmp guard (#4045).
fn register_guards(_: &MergedManifest, hooks: &mut HooksByEvent) -> anyhow::Result<()> {
    guard_hooks::register(hooks)
}

/// The hooks that the config declares. A bundle-relative command path resolves against the cache
/// directory, so the command names the materialized file, not the bundle (#162).
fn user_hooks(
    out: &Path,
    manifest: &MergedManifest,
    hooks: &mut HooksByEvent,
) -> anyhow::Result<()> {
    let mut warned_hook_paths = std::collections::BTreeSet::new();
    let write_on_failure =
        hook_json::on_failure_allowed(&manifest.capabilities.hooks, version::detected);
    for hook in &manifest.capabilities.hooks {
        // Two passes: a clean relative path (`bash hooks/guard.sh`) joins directly, and a shell
        // variable or absolute prefix (`bash ${HOME}/.../hooks/guard.sh`) matches the end of a
        // file that the caller copied into `out`.
        let resolved_command = hook
            .handler
            .command
            .as_deref()
            .map(|cmd| {
                crate::adapter::hook_command::resolve_hook_command(
                    cmd,
                    hook.bundle_origin.as_deref(),
                    out,
                    &manifest.files,
                    &mut warned_hook_paths,
                )
            })
            .transpose()?;
        let handler = hook_json::hook_handler_value(hook, resolved_command, write_on_failure);
        let mut entry = serde_json::Map::new();
        if let Some(matcher) = &hook.matcher {
            entry.insert("matcher".into(), json!(matcher));
        }
        entry.insert("hooks".into(), json!([handler]));
        push(hooks, &hook.event, serde_json::Value::Object(entry));
    }
    Ok(())
}

/// The slippage settings, when the feature is on and `pick` accepts them.
fn slippage_on(manifest: &MergedManifest, pick: impl Fn(&SlippageControl) -> bool) -> bool {
    manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.slippage.as_ref())
        .is_some_and(|s| s.enabled && pick(s))
}

/// The hooks of the slippage layers (#317). The metrics layer counts on PostToolUse, which only
/// session logging registers otherwise. Without it the counter never runs, and the session-end
/// summary is empty. The layers that read tool calls join the `PreToolUse` matchers below.
fn slippage_hooks(manifest: &MergedManifest, hooks: &mut HooksByEvent) -> anyhow::Result<()> {
    if slippage_on(manifest, |s| s.metrics) && !manifest.session_log.any_sink_enabled() {
        let command = format!("{} post_tool_use", hook_run_command()?);
        push(hooks, "PostToolUse", command_entry(&command));
    }
    Ok(())
}

/// The tool-name matchers that have to reach `hook-run pre_tool_use`, in registration order.
///
/// - #318: read-once file dedup. It is always in the set. The `run_inner` handler checks
///   `features.read_once.enabled`, so the match is the only cost when the feature is off.
/// - #317: the transcript-scan layers judge Bash commands, so the shell tools have to reach
///   hook-run. The write guard needs to see `Write`. The `^(Write|Edit|MultiEdit)$` entry runs
///   the config guard, a different command.
/// - #1331: the project-name override of `index_repository`, when the MCP is wired.
/// - #985: the built-in task tools, which redirect to the `llmenv task` tracker.
///   `block_engine_task_tools` (#980) turns the redirect off and keeps the tracker.
fn pre_tool_use_matchers(manifest: &MergedManifest) -> Vec<String> {
    let cbm = crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME;
    let redirect = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .is_some_and(|t| t.enabled && t.block_engine_task_tools);
    let shell = crate::hook_run::shell_tool::SHELL_TOOLS.join("|");
    [
        (true, "^Read$".to_string()),
        (
            slippage_on(manifest, |s| s.answer_before_act || s.explain_before_act),
            format!("^({shell})$"),
        ),
        (
            slippage_on(manifest, |s| s.read_before_edit),
            "^Write$".to_string(),
        ),
        (
            manifest.mcps.iter().any(|m| m.name == cbm),
            format!("^{}$", llmenv_mcp::resolve::INDEX_REPOSITORY_TOOL),
        ),
        (redirect, "^(TaskCreate|TaskList|TaskUpdate)$".to_string()),
    ]
    .into_iter()
    .filter_map(|(on, matcher)| on.then_some(matcher))
    .collect()
}

/// The matcher groups of [`pre_tool_use_matchers`] (#1442). Session logging registers an unmatched
/// `hook-run pre_tool_use`, and that one group already runs for every tool that the matchers name,
/// because `hook-run` dispatches on `tool_name` itself. Both groups would make a `Read` reach the
/// hook twice, which halves the threshold of `repeat_detect` and makes `read_once` see its own
/// first entry.
fn pre_tool_use_matcher_hooks(
    manifest: &MergedManifest,
    hooks: &mut HooksByEvent,
) -> anyhow::Result<()> {
    let session_log_includes_pre_tool_use = SESSION_LOG_HOOK_EVENTS
        .iter()
        .any(|(neutral, _)| *neutral == "pre_tool_use");
    if crate::adapter::unmatched_pre_tool_use_registered(
        manifest,
        session_log_includes_pre_tool_use,
    ) {
        return Ok(());
    }
    let command = pre_tool_use_command()?;
    for matcher in pre_tool_use_matchers(manifest) {
        push(hooks, "PreToolUse", matched_entry(&matcher, &command));
    }
    Ok(())
}

/// The throttle hooks.
fn tool_routing_hooks(manifest: &MergedManifest, hooks: &mut HooksByEvent) -> anyhow::Result<()> {
    // Throttle: poll the usage backend and sleep an adaptive delay, to avoid rate limits.
    if manifest.throttle.is_some() {
        let throttle = throttle_command()?;
        push(
            hooks,
            "PreToolUse",
            command_entry(&format!("{throttle} pre-tool")),
        );
        push(
            hooks,
            "UserPromptSubmit",
            command_entry(&format!("{throttle} prompt")),
        );
    }
    Ok(())
}

/// The baseline lifecycle hooks (#382): memory wake-up and store, and the session-log lifecycle
/// and scope events. They are always registered, and `hook-run` does nothing when no feature
/// is on.
fn lifecycle_hooks(manifest: &MergedManifest, hooks: &mut HooksByEvent) -> anyhow::Result<()> {
    let run = hook_run_command()?;
    for (neutral_event, native_event) in BASELINE_HOOK_EVENTS {
        push(
            hooks,
            native_event,
            command_entry(&format!("{run} {neutral_event}")),
        );
    }
    // #317: the rules digest runs on UserPromptSubmit, so turning the layer on has to register
    // the event. The recall gate below and session logging are the only other things that do.
    if crate::adapter::slippage_rule_reinjection_enabled(manifest)
        && !manifest.session_log.any_sink_enabled()
        && !crate::adapter::lifecycle_event_registered(manifest, "turn_start")
    {
        push(
            hooks,
            "UserPromptSubmit",
            command_entry(&format!("{run} user_prompt_submit")),
        );
    }
    Ok(())
}

/// The recall hooks of the prompt and of the tools (#499, #2249). They run on every prompt or
/// tool call, so they follow the gate that `doctor` reads from [`lifecycle_hook_registrations`]
/// (#741), and not the always-on baseline.
fn prompt_hooks(manifest: &MergedManifest, hooks: &mut HooksByEvent) -> anyhow::Result<()> {
    let run = hook_run_command()?;
    if crate::adapter::lifecycle_event_registered(manifest, "turn_start") {
        push(
            hooks,
            "UserPromptSubmit",
            command_entry(&format!("{run} turn_start")),
        );
    }
    // Adaptive recall records tool activity, injects on failures and subagent starts, and queues
    // subagent tasks. Only this adapter registers them.
    let registered = crate::adapter::adaptive_recall_hook_registrations(manifest);
    let is_on = |name: &str| {
        registered
            .iter()
            .any(|(event, on, _)| *event == name && *on)
    };
    for (neutral_event, native_event) in ADAPTIVE_RECALL_HOOK_EVENTS {
        if is_on(neutral_event) {
            push(
                hooks,
                native_event,
                command_entry(&format!("{run} {neutral_event}")),
            );
        }
    }
    if is_on("subagent_start") {
        push(
            hooks,
            "PreToolUse",
            matched_entry("^Agent$", &format!("{run} subagent_task")),
        );
    }
    Ok(())
}

/// The session-log turn hooks (#382), registered when any sink is on. Otherwise the task
/// tracker, and the self-critique layer of #317, need their own `Stop` registration (#231). It
/// is in the `else` branch, so a session-log setup does not run `hook-run` twice on one event.
fn session_log_hooks(manifest: &MergedManifest, hooks: &mut HooksByEvent) -> anyhow::Result<()> {
    let run = hook_run_command()?;
    if manifest.session_log.any_sink_enabled() {
        for (neutral_event, native_event) in SESSION_LOG_HOOK_EVENTS {
            push(
                hooks,
                native_event,
                command_entry(&format!("{run} {neutral_event}")),
            );
        }
        return Ok(());
    }
    if crate::adapter::lifecycle_event_registered(manifest, "stop") {
        push(hooks, "Stop", command_entry(&format!("{run} stop")));
    }
    Ok(())
}

/// The task nudges and the commit deny-once (#2456). An entry is skipped when another feature
/// already routes that event and tool to hook-run, so one tool call does not run the handler
/// twice.
fn task_nudge_hooks(manifest: &MergedManifest, hooks: &mut HooksByEvent) -> anyhow::Result<()> {
    let tracker = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .filter(|t| t.enabled);
    let Some(tracker) = tracker else {
        return Ok(());
    };
    let post = format!("{} post_tool_use", hook_run_command()?);
    let pre = pre_tool_use_command()?;
    // The shell tools come from one list, so a Monitor command gets the same reminder as Bash.
    let post_candidates: Vec<&str> = ["Skill", "Edit", "Write", "MultiEdit", "AskUserQuestion"]
        .into_iter()
        .chain(crate::hook_run::shell_tool::SHELL_TOOLS)
        .collect();
    let post_tools = unrouted_tools(hooks.get("PostToolUse"), &post, &post_candidates);
    if tracker.nudges && !post_tools.is_empty() {
        push(
            hooks,
            "PostToolUse",
            matched_entry(&tools_matcher(&post_tools), &post),
        );
    }
    let pre_tools = unrouted_tools(
        hooks.get("PreToolUse"),
        &pre,
        &crate::hook_run::shell_tool::SHELL_TOOLS,
    );
    if tracker.enforce_commit && !pre_tools.is_empty() {
        push(
            hooks,
            "PreToolUse",
            matched_entry(&tools_matcher(&pre_tools), &pre),
        );
    }
    Ok(())
}

/// The hooks object from the entries: the `native_hooks` overlay of the engine on top (#97), and
/// no entry twice (#977). `reconcile_settings` removes a duplicate only when a settings file
/// exists, so a first render, or an overlay that repeats a typed hook, would write each entry
/// twice and run each guard twice for an event.
fn finish(manifest: &MergedManifest, hooks: HooksByEvent) -> anyhow::Result<serde_json::Value> {
    let mut object = serde_json::Map::new();
    for (event, entries) in hooks {
        object.insert(event, json!(entries));
    }
    let mut value = serde_json::Value::Object(object);
    overlay_native(
        &mut value,
        manifest.capabilities.native_hooks.get("claude_code"),
    )?;
    dedup_hooks_doc(&mut value);
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
