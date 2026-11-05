//! The hook entries of a config or a bundle.

use serde::{Deserialize, Serialize};

/// A hook registration. `command` paths in `handler` are bundle-relative when
/// declared in a `bundle.yaml`, resolved at materialize time.
#[derive(Debug, Clone, Deserialize, Serialize, Eq)]
pub struct Hook {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub handler: HookHandler,
    #[serde(skip)]
    pub bundle_origin: Option<std::path::PathBuf>,
}

impl PartialEq for Hook {
    fn eq(&self, other: &Self) -> bool {
        self.event == other.event && self.matcher == other.matcher && self.handler == other.handler
    }
}

/// A hook handler. `type` selects the mechanism; `command` is set for
/// `command`-type handlers, `tool` for `mcp_tool`-type.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HookHandler {
    #[serde(rename = "type")]
    pub kind: HookHandlerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// What the engine does when the hook cannot start, times out, or exits with an unexpected
    /// code. Unset means the engine default, which lets the action through (#3101).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<HookOnFailure>,
}

/// The engine action when a hook fails to run. Only Claude Code 2.1.295 and later read it (#3101).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookOnFailure {
    /// Block the action, so a guard that cannot run fails closed.
    Block,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookHandlerKind {
    Command,
    McpTool,
}
