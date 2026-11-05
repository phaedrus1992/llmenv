//! Checks of the hook entries: each handler has the field its type needs.

use super::ValidateError;
use crate::{Hook, HookHandlerKind};

/// A hook whose `kind` doesn't match its `command`/`tool` field is schema-valid but produces a
/// silent no-op in the generated engine config (Claude Code emits `{"type":"command"}` with no
/// command key and runs nothing; Crush rejects it at load time). Fail fast here instead,
/// mirroring the `McpStdioMissingCommand` check of `servers::validate_transport`.
///
/// `on_failure` is read for a command hook only, so an `mcp_tool` hook that sets it is an error
/// instead of a key the engine ignores (#3101).
///
/// # Errors
/// The first hook whose handler lacks the field its type needs, or that sets `on_failure` on a
/// hook that is not a command.
pub fn validate_hooks(hooks: &[Hook]) -> Result<(), ValidateError> {
    let is_missing = |field: &Option<String>| field.as_deref().unwrap_or("").is_empty();
    for hook in hooks {
        let event = || hook.event.clone();
        match hook.handler.kind {
            HookHandlerKind::Command if is_missing(&hook.handler.command) => {
                return Err(ValidateError::HookCommandMissingCommand { event: event() });
            }
            HookHandlerKind::McpTool if is_missing(&hook.handler.tool) => {
                return Err(ValidateError::HookMcpToolMissingTool { event: event() });
            }
            HookHandlerKind::McpTool if hook.handler.on_failure.is_some() => {
                return Err(ValidateError::HookOnFailureNeedsCommand { event: event() });
            }
            HookHandlerKind::Command | HookHandlerKind::McpTool => {}
        }
    }
    Ok(())
}
