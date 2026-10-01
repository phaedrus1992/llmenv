//! Retired Claude Code settings detection.
//!
//! Claude Code periodically retires settings keys, environment variables, permission tools, and
//! MCP server types. This module detects them in rendered configuration files and warns the user.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RetiredKind {
    SettingsKey,
    EnvVar,
    PermissionTool,
    McpType,
}

impl fmt::Display for RetiredKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SettingsKey => write!(f, "settings key"),
            Self::EnvVar => write!(f, "environment variable"),
            Self::PermissionTool => write!(f, "permission tool"),
            Self::McpType => write!(f, "MCP server type"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Retired {
    pub kind: RetiredKind,
    /// Exact name as Claude Code spells it.
    pub name: &'static str,
    /// `true` when Claude Code ignores it; `false` when it still reads it but is deprecated.
    pub no_effect: bool,
    /// Claude Code version, or `None` when the docs give none.
    pub since: Option<&'static str>,
    /// Replacement, or `None`.
    pub replacement: Option<&'static str>,
}

/// Retired Claude Code entries.
///
/// Sources: Claude Code settings reference and environment-variable reference
/// (code.claude.com/docs/en/settings-reference.md, env-vars.md), and the Claude Code changelog.
/// New rows should be added at the top of each section.
pub(crate) const RETIRED: &[Retired] = &[
    // Settings keys (removed or deprecated at top level of settings.json)
    Retired {
        kind: RetiredKind::SettingsKey,
        name: "taskOutputMaxChars",
        no_effect: true,
        since: Some("2.1.277"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::SettingsKey,
        name: "permissionExplainerEnabled",
        no_effect: true,
        since: Some("2.1.257"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::SettingsKey,
        name: "teammateDefaultModel",
        no_effect: true,
        since: Some("2.1.234"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::SettingsKey,
        name: "keybindingFlavor",
        no_effect: true,
        since: Some("2.1.261"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::SettingsKey,
        name: "includeCoAuthoredBy",
        no_effect: false,
        since: Some("2.0.62"),
        replacement: Some("attribution"),
    },
    Retired {
        kind: RetiredKind::SettingsKey,
        name: "disableArtifact",
        no_effect: false,
        since: None,
        replacement: Some("enableArtifact: false"),
    },
    Retired {
        kind: RetiredKind::SettingsKey,
        name: "voiceEnabled",
        no_effect: false,
        since: Some("2.1.92"),
        replacement: Some("voice.enabled"),
    },
    // Environment variables (in the env object of settings.json)
    Retired {
        kind: RetiredKind::EnvVar,
        name: "TASK_MAX_OUTPUT_LENGTH",
        no_effect: true,
        since: Some("2.1.277"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::EnvVar,
        name: "CLAUDE_SUBAGENT_BG_SHELL_MAX_MS",
        no_effect: true,
        since: Some("2.1.260"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::EnvVar,
        name: "CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION",
        no_effect: true,
        since: Some("2.1.224"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::EnvVar,
        name: "CLAUDE_CODE_CONNECT_TIMEOUT_MS",
        no_effect: true,
        since: Some("2.1.186"),
        replacement: Some("API_TIMEOUT_MS"),
    },
    Retired {
        kind: RetiredKind::EnvVar,
        name: "CLAUDE_CODE_OPUS_4_6_FAST_MODE_OVERRIDE",
        no_effect: true,
        since: Some("2.1.160"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::EnvVar,
        name: "CLAUDE_CODE_ENABLE_OPUS_4_7_FAST_MODE",
        no_effect: true,
        since: Some("2.1.142"),
        replacement: None,
    },
    Retired {
        kind: RetiredKind::EnvVar,
        name: "ANTHROPIC_SMALL_FAST_MODEL",
        no_effect: false,
        since: None,
        replacement: Some("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
    },
    Retired {
        kind: RetiredKind::EnvVar,
        name: "ENABLE_PROMPT_CACHING_1H_BEDROCK",
        no_effect: false,
        since: None,
        replacement: Some("ENABLE_PROMPT_CACHING_1H"),
    },
    // Permission tools (in permissions.allow/ask/deny)
    Retired {
        kind: RetiredKind::PermissionTool,
        name: "TaskOutput",
        no_effect: true,
        since: Some("2.1.277"),
        replacement: None,
    },
    // MCP server types (in mcpServers)
    Retired {
        kind: RetiredKind::McpType,
        name: "sdk",
        no_effect: true,
        since: Some("2.1.274"),
        replacement: Some(
            "use stdio or http; only an SDK host application can register in-process servers",
        ),
    },
];

#[derive(Debug, Clone)]
pub(crate) struct RetiredHit {
    pub entry: &'static Retired,
    /// Where it was found, such as `settings.json env` or `settings.json permissions.deny[3]`.
    pub location: String,
}

impl fmt::Display for RetiredHit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location, self.entry.name)
    }
}

/// Scan settings.json and .claude.json for retired Claude Code entries.
///
/// Returns a list of found entries, in file order. Non-object or missing parts are skipped.
pub(crate) fn scan(
    settings: &serde_json::Value,
    claude_json: &serde_json::Value,
) -> Vec<RetiredHit> {
    let mut hits = Vec::new();

    // Settings keys (top-level keys of settings.json)
    if let serde_json::Value::Object(obj) = settings {
        for retired in RETIRED {
            if retired.kind == RetiredKind::SettingsKey && obj.contains_key(retired.name) {
                hits.push(RetiredHit {
                    entry: retired,
                    location: format!("settings.json"),
                });
            }
        }

        // Env vars (keys of settings.env)
        if let Some(serde_json::Value::Object(env_obj)) = obj.get("env") {
            for retired in RETIRED {
                if retired.kind == RetiredKind::EnvVar && env_obj.contains_key(retired.name) {
                    hits.push(RetiredHit {
                        entry: retired,
                        location: format!("settings.json env"),
                    });
                }
            }
        }

        // Permission tools (strings in settings.permissions.allow/ask/deny)
        if let Some(perms) = obj.get("permissions") {
            if let serde_json::Value::Object(perms_obj) = perms {
                for perm_tier in &["allow", "ask", "deny"] {
                    if let Some(serde_json::Value::Array(rules)) = perms_obj.get(*perm_tier) {
                        for (idx, rule) in rules.iter().enumerate() {
                            if let serde_json::Value::String(tool_name) = rule {
                                // Extract the tool name before any '('
                                let base_tool = tool_name.split('(').next().unwrap_or(tool_name);
                                for retired in RETIRED {
                                    if retired.kind == RetiredKind::PermissionTool
                                        && retired.name == base_tool
                                    {
                                        hits.push(RetiredHit {
                                            entry: retired,
                                            location: format!(
                                                "settings.json permissions.{}[{}]",
                                                perm_tier, idx
                                            ),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // MCP server types (values in .claude.json mcpServers)
    if let Some(mcp_servers) = claude_json.get("mcpServers") {
        if let serde_json::Value::Object(servers_obj) = mcp_servers {
            for (idx, (_, server_config)) in servers_obj.iter().enumerate() {
                if let Some(serde_json::Value::String(mcp_type)) = server_config.get("type") {
                    for retired in RETIRED {
                        if retired.kind == RetiredKind::McpType && retired.name == mcp_type {
                            hits.push(RetiredHit {
                                entry: retired,
                                location: format!(".claude.json mcpServers[{}]", idx),
                            });
                        }
                    }
                }
            }
        }
    }

    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_settings_key() {
        let settings = serde_json::json!({ "taskOutputMaxChars": 1000 });
        let claude = serde_json::json!({});
        let hits = scan(&settings, &claude);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "taskOutputMaxChars");
        assert_eq!(hits[0].location, "settings.json");
    }

    #[test]
    fn scan_finds_env_var() {
        let settings = serde_json::json!({ "env": { "TASK_MAX_OUTPUT_LENGTH": "1000" } });
        let claude = serde_json::json!({});
        let hits = scan(&settings, &claude);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "TASK_MAX_OUTPUT_LENGTH");
        assert_eq!(hits[0].location, "settings.json env");
    }

    #[test]
    fn scan_finds_permission_tool() {
        let settings = serde_json::json!({
            "permissions": {
                "allow": ["Bash", "TaskOutput"]
            }
        });
        let claude = serde_json::json!({});
        let hits = scan(&settings, &claude);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "TaskOutput");
        assert_eq!(hits[0].location, "settings.json permissions.allow[1]");
    }

    #[test]
    fn scan_handles_tool_with_args() {
        let settings = serde_json::json!({
            "permissions": {
                "deny": ["Bash(rm)", "TaskOutput(*)"]
            }
        });
        let claude = serde_json::json!({});
        let hits = scan(&settings, &claude);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "TaskOutput");
    }

    #[test]
    fn scan_finds_mcp_type() {
        let settings = serde_json::json!({});
        let claude = serde_json::json!({
            "mcpServers": {
                "my_server": { "type": "sdk", "command": "..." }
            }
        });
        let hits = scan(&settings, &claude);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "sdk");
    }

    #[test]
    fn scan_handles_empty_or_malformed() {
        let hits = scan(&serde_json::json!({}), &serde_json::json!({}));
        assert_eq!(hits.len(), 0);

        let hits = scan(&serde_json::json!({ "env": [] }), &serde_json::json!({}));
        assert_eq!(hits.len(), 0);

        let hits = scan(
            &serde_json::json!({ "permissions": "invalid" }),
            &serde_json::json!({}),
        );
        assert_eq!(hits.len(), 0);
    }

    #[test]
    fn table_uniqueness_and_validity() {
        let mut seen_by_kind: std::collections::HashMap<
            RetiredKind,
            std::collections::HashSet<&str>,
        > = std::collections::HashMap::new();

        for retired in RETIRED {
            let names = seen_by_kind
                .entry(retired.kind)
                .or_insert_with(std::collections::HashSet::new);
            assert!(
                names.insert(retired.name),
                "Duplicate entry: {} {} in table",
                retired.kind,
                retired.name
            );

            // If no_effect is false, there must be a replacement
            if !retired.no_effect {
                assert!(
                    retired.replacement.is_some(),
                    "Deprecated (no_effect=false) {} {} has no replacement",
                    retired.kind,
                    retired.name
                );
            }
        }
    }
}
