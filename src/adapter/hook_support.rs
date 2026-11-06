//! Which bundle hooks an engine other than Claude Code can run.

use crate::config::{Hook, HookHandlerKind};

/// Whether the `engine` adapter can run `hook`. A bundle shared across engines commonly declares
/// hooks that only Claude Code supports, which is a compatibility gap and not an authoring
/// mistake, so an unsupported hook is skipped with a warning and the rest of the config still
/// renders (#543). `on_failure` is Claude Code only: the hook still runs, and the warning says
/// that it fails open (#3101).
pub(crate) fn engine_can_run_hook(engine: &str, supported_events: &[&str], hook: &Hook) -> bool {
    if !supported_events.contains(&hook.event.as_str()) {
        eprintln!(
            "warning: {engine} adapter does not support hook event '{}' \u{2014} skipping this \
             hook. Supported events: {}. Remove or move this hook to a claude_code-only bundle to \
             silence this warning.",
            hook.event,
            supported_events.join(", ")
        );
        return false;
    }
    if matches!(hook.handler.kind, HookHandlerKind::McpTool) {
        eprintln!(
            "warning: {engine} adapter does not support mcp_tool hooks (hook event '{}', tool \
             '{}') \u{2014} skipping this hook. Use a command hook instead.",
            hook.event,
            hook.handler.tool.as_deref().unwrap_or("<unknown>")
        );
        return false;
    }
    if hook.handler.on_failure.is_some() {
        eprintln!(
            "warning: {engine} adapter does not read on_failure (hook event '{}'), so this hook \
             fails open when it cannot run. Only Claude Code blocks on a hook failure.",
            hook.event
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HookHandler, HookOnFailure};

    fn hook(event: &str, kind: HookHandlerKind, on_failure: Option<HookOnFailure>) -> Hook {
        Hook {
            event: event.into(),
            matcher: None,
            handler: HookHandler {
                kind,
                command: Some("guard.sh".into()),
                tool: Some("srv:tool".into()),
                on_failure,
            },
            bundle_origin: None,
        }
    }

    #[test]
    fn an_engine_skips_an_unsupported_event_and_an_mcp_tool_hook() {
        let supported = ["PreToolUse"];
        assert!(!engine_can_run_hook(
            "Crush",
            &supported,
            &hook("Stop", HookHandlerKind::Command, None)
        ));
        assert!(!engine_can_run_hook(
            "Crush",
            &supported,
            &hook("PreToolUse", HookHandlerKind::McpTool, None)
        ));
        assert!(engine_can_run_hook(
            "Crush",
            &supported,
            &hook("PreToolUse", HookHandlerKind::Command, None)
        ));
    }

    // #3101: on_failure does not drop the hook on another engine; the hook still runs.
    #[test]
    fn a_hook_with_on_failure_still_runs_on_another_engine() {
        let blocking = hook(
            "PreToolUse",
            HookHandlerKind::Command,
            Some(HookOnFailure::Block),
        );
        assert!(engine_can_run_hook("opencode", &["PreToolUse"], &blocking));
    }
}
