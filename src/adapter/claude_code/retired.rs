//! Retired Claude Code settings keys, environment variables, permission tools and MCP server
//! types, and a scan of the rendered config files for them (#2145).
//!
//! Design: docs/design/issue-2145-retired-claude-keys.md

use crate::util::escape_control;
use RetiredKind::{EnvVar, McpType, PermissionTool, SettingsKey};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RetiredKind {
    SettingsKey,
    EnvVar,
    PermissionTool,
    McpType,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Retired {
    pub kind: RetiredKind,
    /// Exact name as Claude Code spells it.
    pub name: &'static str,
    /// `true` when Claude Code ignores it; `false` when it still reads it but it is deprecated.
    pub no_effect: bool,
    /// Claude Code version, or `None` when the docs give none.
    pub since: Option<&'static str>,
    /// The setting or value to use instead, or `None`.
    pub replacement: Option<&'static str>,
    /// Extra advice for a row with no replacement, or `None`.
    pub note: Option<&'static str>,
}

const fn row(
    kind: RetiredKind,
    name: &'static str,
    no_effect: bool,
    since: Option<&'static str>,
    replacement: Option<&'static str>,
) -> Retired {
    Retired {
        kind,
        name,
        no_effect,
        since,
        replacement,
        note: None,
    }
}

const fn with_note(mut r: Retired, note: &'static str) -> Retired {
    r.note = Some(note);
    r
}

/// Every row comes from Claude Code's settings reference, its environment-variable reference, or
/// its changelog (verified 2026-09-26). Add a new row at the top of its kind's group.
const RETIRED: &[Retired] = &[
    with_note(
        row(
            SettingsKey,
            "taskOutputMaxChars",
            true,
            Some("2.1.277"),
            None,
        ),
        "Claude reads a background task's output file with Read",
    ),
    row(
        SettingsKey,
        "permissionExplainerEnabled",
        true,
        Some("2.1.257"),
        None,
    ),
    with_note(
        row(
            SettingsKey,
            "teammateDefaultModel",
            true,
            Some("2.1.234"),
            None,
        ),
        "see Claude Code's agent-teams docs",
    ),
    with_note(
        row(SettingsKey, "keybindingFlavor", true, Some("2.1.261"), None),
        "word-editing keys always follow readline",
    ),
    row(
        SettingsKey,
        "includeCoAuthoredBy",
        false,
        Some("2.0.62"),
        Some("attribution"),
    ),
    row(
        SettingsKey,
        "disableArtifact",
        false,
        None,
        Some("enableArtifact: false"),
    ),
    row(
        SettingsKey,
        "voiceEnabled",
        false,
        Some("2.1.92"),
        Some("voice.enabled"),
    ),
    row(
        EnvVar,
        "TASK_MAX_OUTPUT_LENGTH",
        true,
        Some("2.1.277"),
        None,
    ),
    row(
        EnvVar,
        "CLAUDE_SUBAGENT_BG_SHELL_MAX_MS",
        true,
        Some("2.1.260"),
        None,
    ),
    row(
        EnvVar,
        "CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION",
        true,
        Some("2.1.224"),
        None,
    ),
    row(
        EnvVar,
        "CLAUDE_CODE_CONNECT_TIMEOUT_MS",
        true,
        Some("2.1.186"),
        Some("API_TIMEOUT_MS"),
    ),
    row(
        EnvVar,
        "CLAUDE_CODE_OPUS_4_6_FAST_MODE_OVERRIDE",
        true,
        Some("2.1.160"),
        None,
    ),
    row(
        EnvVar,
        "CLAUDE_CODE_ENABLE_OPUS_4_7_FAST_MODE",
        true,
        Some("2.1.142"),
        None,
    ),
    row(
        EnvVar,
        "ANTHROPIC_SMALL_FAST_MODEL",
        false,
        None,
        Some("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
    ),
    row(
        EnvVar,
        "ENABLE_PROMPT_CACHING_1H_BEDROCK",
        false,
        None,
        Some("ENABLE_PROMPT_CACHING_1H"),
    ),
    row(PermissionTool, "TaskOutput", true, Some("2.1.277"), None),
    row(
        McpType,
        "sdk",
        true,
        Some("2.1.274"),
        Some("type stdio or http"),
    ),
];

#[derive(Debug, Clone)]
pub(crate) struct RetiredHit {
    pub entry: &'static Retired,
    /// Where the entry was found, such as `settings.json env` or
    /// `settings.json permissions.deny[3]`.
    pub location: String,
}

impl RetiredHit {
    /// The doctor line for this hit, without the status prefix.
    #[must_use]
    pub(crate) fn message(&self) -> String {
        let Retired {
            name,
            no_effect,
            since,
            replacement,
            note,
            ..
        } = *self.entry;
        let location = &self.location;
        if no_effect {
            let since = since.map_or(String::new(), |v| format!(" since Claude Code {v}"));
            let advice = match (replacement, note) {
                (Some(r), _) => format!("Use {r} instead."),
                (None, Some(n)) => format!("Remove it; {n}."),
                (None, None) => "Remove it.".to_string(),
            };
            format!("{location}: {name} has no effect{since}. {advice}")
        } else {
            let advice = replacement.map_or(String::new(), |r| format!(" Use {r}."));
            format!("{location}: {name} is deprecated.{advice}")
        }
    }
}

fn lookup(kind: RetiredKind, name: &str) -> Option<&'static Retired> {
    RETIRED.iter().find(|r| r.kind == kind && r.name == name)
}

/// Add a hit for each server in `holder["mcpServers"]` whose `type` is retired. Server names
/// come from user and third-party config, so control characters are escaped before printing.
fn scan_mcp_servers(holder: &Value, prefix: &str, hits: &mut Vec<RetiredHit>) {
    let Some(servers) = holder.get("mcpServers").and_then(Value::as_object) else {
        return;
    };
    for (server, config) in servers {
        let Some(kind) = config.get("type").and_then(Value::as_str) else {
            continue;
        };
        if let Some(entry) = lookup(McpType, kind) {
            hits.push(RetiredHit {
                entry,
                location: format!("{prefix} mcpServers.{}", escape_control(server)),
            });
        }
    }
}

/// Scan a rendered `settings.json` and `.claude.json` for retired entries.
///
/// Output order: settings keys, env vars, permission rules, MCP servers. Object keys come in
/// `serde_json`'s map order (sorted), and permission rules in array order. A part that is
/// missing or has the wrong JSON type is skipped. The function does no file I/O.
#[must_use]
pub(crate) fn scan(settings: &Value, claude_json: &Value) -> Vec<RetiredHit> {
    let mut hits = Vec::new();
    let hit = |entry, location: String| RetiredHit { entry, location };

    if let Some(obj) = settings.as_object() {
        hits.extend(
            obj.keys()
                .filter_map(|k| lookup(SettingsKey, k))
                .map(|e| hit(e, "settings.json".to_string())),
        );
        if let Some(env) = obj.get("env").and_then(Value::as_object) {
            hits.extend(
                env.keys()
                    .filter_map(|k| lookup(EnvVar, k))
                    .map(|e| hit(e, "settings.json env".to_string())),
            );
        }
        if let Some(perms) = obj.get("permissions").and_then(Value::as_object) {
            for tier in ["allow", "ask", "deny"] {
                let Some(rules) = perms.get(tier).and_then(Value::as_array) else {
                    continue;
                };
                for (idx, rule) in rules.iter().enumerate() {
                    let Some(rule) = rule.as_str() else { continue };
                    let tool = rule.split('(').next().unwrap_or(rule);
                    if let Some(e) = lookup(PermissionTool, tool) {
                        hits.push(hit(e, format!("settings.json permissions.{tier}[{idx}]")));
                    }
                }
            }
        }
    }

    scan_mcp_servers(claude_json, ".claude.json", &mut hits);
    // Claude Code also keeps per-project MCP servers under `projects.<path>.mcpServers`.
    if let Some(projects) = claude_json.get("projects").and_then(Value::as_object) {
        for (project, entry) in projects {
            let prefix = format!(".claude.json projects.{}", escape_control(project));
            scan_mcp_servers(entry, &prefix, &mut hits);
        }
    }

    hits
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};

    fn names(hits: &[RetiredHit]) -> Vec<&str> {
        hits.iter().map(|h| h.entry.name).collect()
    }

    #[test]
    fn every_row_is_found_by_its_own_kind() {
        for r in RETIRED {
            let (settings, claude) = match r.kind {
                SettingsKey => (json!({ r.name: 1 }), json!({})),
                EnvVar => (json!({ "env": { r.name: "1" } }), json!({})),
                PermissionTool => (json!({ "permissions": { "deny": [r.name] } }), json!({})),
                McpType => (
                    json!({}),
                    json!({ "mcpServers": { "s": { "type": r.name } } }),
                ),
            };
            let hits = scan(&settings, &claude);
            assert_eq!(names(&hits), vec![r.name], "row {} {:?}", r.name, r.kind);
        }
    }

    #[test]
    fn locations_name_the_part_that_holds_the_entry() {
        let settings = json!({
            "voiceEnabled": true,
            "env": { "TASK_MAX_OUTPUT_LENGTH": "1" },
            "permissions": { "allow": ["Bash", "TaskOutput"] },
        });
        let claude = json!({ "mcpServers": { "inproc": { "type": "sdk" } } });
        let locations: Vec<String> = scan(&settings, &claude)
            .into_iter()
            .map(|h| h.location)
            .collect();
        assert_eq!(
            locations,
            [
                "settings.json",
                "settings.json env",
                "settings.json permissions.allow[1]",
                ".claude.json mcpServers.inproc",
            ]
        );
    }

    #[test]
    fn permission_match_uses_the_exact_tool_name() {
        let settings = json!({ "permissions": { "ask": [
            "TaskOutputFoo", "Bash(TaskOutput)", "TaskOutput", "TaskOutput(*)"
        ] } });
        let hits = scan(&settings, &json!({}));
        let locations: Vec<&str> = hits.iter().map(|h| h.location.as_str()).collect();
        assert_eq!(
            locations,
            [
                "settings.json permissions.ask[2]",
                "settings.json permissions.ask[3]"
            ]
        );
    }

    #[test]
    fn per_project_mcp_servers_are_scanned_and_names_are_escaped() {
        let claude = json!({
            "projects": {
                "/repo": { "mcpServers": { "evil\u{1b}[2J": { "type": "sdk" } } },
                "/other": { "mcpServers": { "ok": { "type": "stdio" } } },
            }
        });
        let hits = scan(&json!({}), &claude);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].location,
            r".claude.json projects./repo mcpServers.evil\u{1b}[2J"
        );
        assert!(!hits[0].message().contains('\u{1b}'));
    }

    #[test]
    fn wrong_json_types_give_no_hits() {
        for settings in [
            json!({}),
            json!([]),
            json!({ "env": ["TASK_MAX_OUTPUT_LENGTH"] }),
            json!({ "permissions": "TaskOutput" }),
            json!({ "permissions": { "deny": "TaskOutput" } }),
            json!({ "permissions": { "deny": [42] } }),
        ] {
            assert!(scan(&settings, &json!({})).is_empty(), "{settings}");
        }
        assert!(scan(&json!({}), &json!({ "mcpServers": [] })).is_empty());
        assert!(scan(&json!({}), &json!({ "mcpServers": { "s": { "type": 1 } } })).is_empty());
    }

    #[test]
    fn message_formats() {
        let msg = |kind, name| {
            RetiredHit {
                entry: lookup(kind, name).unwrap(),
                location: "loc".to_string(),
            }
            .message()
        };
        assert_eq!(
            msg(EnvVar, "TASK_MAX_OUTPUT_LENGTH"),
            "loc: TASK_MAX_OUTPUT_LENGTH has no effect since Claude Code 2.1.277. Remove it."
        );
        assert_eq!(
            msg(EnvVar, "CLAUDE_CODE_CONNECT_TIMEOUT_MS"),
            "loc: CLAUDE_CODE_CONNECT_TIMEOUT_MS has no effect since Claude Code 2.1.186. \
             Use API_TIMEOUT_MS instead."
        );
        assert_eq!(
            msg(SettingsKey, "keybindingFlavor"),
            "loc: keybindingFlavor has no effect since Claude Code 2.1.261. \
             Remove it; word-editing keys always follow readline."
        );
        assert_eq!(
            msg(SettingsKey, "voiceEnabled"),
            "loc: voiceEnabled is deprecated. Use voice.enabled."
        );
    }

    #[test]
    fn table_shape() {
        let mut seen: HashMap<RetiredKind, HashSet<&str>> = HashMap::new();
        for r in RETIRED {
            assert!(
                seen.entry(r.kind).or_default().insert(r.name),
                "duplicate {:?} {}",
                r.kind,
                r.name
            );
            if !r.no_effect {
                assert!(
                    r.replacement.is_some(),
                    "deprecated {} needs a replacement",
                    r.name
                );
            }
            assert!(
                r.replacement.is_none() || r.note.is_none(),
                "{} has both a replacement and a note",
                r.name
            );
        }
    }
}
