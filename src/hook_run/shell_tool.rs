//! The tool names that run a shell command. A guard that reads the command checks this list.

/// Returns true when `tool` runs a shell command.
///
/// Claude Code 2.1.314 sends a command that the Monitor tool starts with `tool_name` `Monitor`.
/// A guard that matched only `Bash` skipped those commands (#3354).
pub(crate) fn is_shell_tool(tool: &str) -> bool {
    matches!(tool, "Bash" | "PowerShell" | "Monitor")
}

#[cfg(test)]
mod tests {
    use super::is_shell_tool;

    #[test]
    fn bash_powershell_and_monitor_run_shell_commands() {
        for tool in ["Bash", "PowerShell", "Monitor"] {
            assert!(is_shell_tool(tool), "{tool}");
        }
    }

    #[test]
    fn other_tools_and_near_misses_are_not_shell_tools() {
        for tool in ["Edit", "Read", "Skill", "", "bash", "MonitorX"] {
            assert!(!is_shell_tool(tool), "{tool}");
        }
    }
}
