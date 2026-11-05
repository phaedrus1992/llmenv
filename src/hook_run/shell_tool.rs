//! The tool names that run a shell command. A guard that reads the command checks this list, and
//! the adapter builds its matchers from it, so the two cannot drift apart.

/// Claude Code tools whose `tool_input.command` is a shell command.
///
/// Claude Code 2.1.314 runs a command that the Monitor tool starts under `tool_name` `Monitor`.
/// A guard that matched only `Bash` skipped those commands (#3354).
pub(crate) const SHELL_TOOLS: [&str; 2] = ["Bash", "Monitor"];

/// Returns true when `tool` runs a shell command.
pub(crate) fn is_shell_tool(tool: &str) -> bool {
    SHELL_TOOLS.contains(&tool)
}

#[cfg(test)]
mod tests {
    use super::{SHELL_TOOLS, is_shell_tool};

    #[test]
    fn bash_and_monitor_run_shell_commands() {
        for tool in SHELL_TOOLS {
            assert!(is_shell_tool(tool), "{tool}");
        }
        assert!(is_shell_tool("Monitor"));
    }

    #[test]
    fn other_tools_and_near_misses_are_not_shell_tools() {
        for tool in [
            "Edit",
            "Read",
            "Skill",
            "",
            "bash",
            "MonitorX",
            "PowerShell",
        ] {
            assert!(!is_shell_tool(tool), "{tool}");
        }
    }
}
