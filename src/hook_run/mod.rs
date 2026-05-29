//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.
//!
//! `run(event)` is the CLI entry. It resolves the active config, finds the
//! memory backend's HTTP URL, runs the actions configured for `event`, and
//! prints the adapter-formatted context to stdout. Every failure degrades to a
//! one-line stderr warning and exit 0 — lifecycle hooks run on the agent's hot
//! path and must never block it.

mod action;
mod mcp_client;

use std::str::FromStr;
use std::time::Duration;

use action::Action;
use mcp_client::McpHttpClient;

use crate::adapter::AgentAdapter;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::mcp::resolve::{MEMORY_MCP_NAME, ResolvedKind, resolve_mcps};

/// Per-call network timeout. Lifecycle hooks run on startup and every prompt, so
/// a slow/dead remote ICM must not stall the agent. 2s balances real round-trips
/// against not hanging the prompt.
const HOOK_TIMEOUT: Duration = Duration::from_secs(2);

/// An engine-neutral lifecycle event. Adapters translate these to native hook
/// names when wiring them into agent config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Session begins (Claude Code: `SessionStart`).
    SessionStart,
    /// A user prompt/turn begins (Claude Code: `UserPromptSubmit`).
    TurnStart,
    /// Session ends (Claude Code: `SessionEnd`).
    SessionEnd,
}

impl FromStr for HookEvent {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "session_start" => Ok(HookEvent::SessionStart),
            "turn_start" => Ok(HookEvent::TurnStart),
            "session_end" => Ok(HookEvent::SessionEnd),
            other => Err(anyhow::anyhow!(
                "unknown hook event '{other}' (expected session_start|turn_start|session_end)"
            )),
        }
    }
}

/// The ordered actions to run for an event. One per event today; the Vec leaves
/// room for an event to gain more actions later.
fn dispatch(event: HookEvent) -> Vec<Action> {
    match event {
        HookEvent::SessionStart => vec![Action::WakeUp],
        HookEvent::TurnStart => vec![Action::Recall],
        HookEvent::SessionEnd => vec![Action::Store],
    }
}

/// CLI entry. Fail-soft: a warning + empty stdout + exit 0 on any error. Returns
/// `Ok(())` even when the backend is unreachable.
pub fn run(event: &str) -> anyhow::Result<()> {
    let parsed = match HookEvent::from_str(event) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("llmenv: {e}");
            return Ok(());
        }
    };
    match run_inner(parsed) {
        Ok(text) => {
            let out = ClaudeCodeAdapter.emit_hook_context(&text);
            if !out.is_empty() {
                println!("{out}");
            }
        }
        Err(e) => {
            eprintln!("llmenv: memory {event} skipped: {e}");
        }
    }
    Ok(())
}

/// Resolve config, find the memory URL, run the event's actions, and return the
/// concatenated result text. Errors here are caught and warned by `run`.
fn run_inner(event: HookEvent) -> anyhow::Result<String> {
    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let url = memory_url(&config, &active)
        .ok_or_else(|| anyhow::anyhow!("no memory backend active for this scope"))?;

    // Recall query: the sorted active tags. Store content: the llmenv context
    // chunk (tags/bundles/project). Bundles aren't needed for the query.
    let mut tags = active.tags.iter().cloned().collect::<Vec<_>>();
    tags.sort();
    // Validate tags to prevent query injection before building the recall query.
    for tag in &tags {
        validate_tag(tag)?;
    }
    let query = tags.join(", ");
    let chunk = crate::icm::generate_context_chunk(&active, &[]);

    let client = McpHttpClient::new(url, HOOK_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut out = String::new();
        for action in dispatch(event) {
            let text = action.run(&client, &query, &chunk).await?;
            if !text.is_empty() {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&text);
            }
        }
        Ok::<String, anyhow::Error>(out)
    })
}

/// Find the resolved memory backend's HTTP URL for the active tags, if any.
fn memory_url(
    config: &crate::config::Config,
    active: &crate::scope::ActiveScopes,
) -> Option<String> {
    let resolved = resolve_mcps(config, &active.tags).ok()?;
    resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => Some(url),
        _ => None,
    })
}

/// Validate a tag to prevent query injection. Tags must be alphanumeric with
/// hyphens and underscores only (same as bundle/scope naming).
fn validate_tag(tag: &str) -> anyhow::Result<()> {
    if tag.is_empty() {
        return Err(anyhow::anyhow!("empty tag in recall query"));
    }
    if !tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow::anyhow!(
            "tag '{}' contains invalid characters (only alphanumeric, -, _ allowed)",
            tag
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_neutral_event_names() {
        assert_eq!(
            "session_start".parse::<HookEvent>().unwrap(),
            HookEvent::SessionStart
        );
        assert_eq!(
            "turn_start".parse::<HookEvent>().unwrap(),
            HookEvent::TurnStart
        );
        assert_eq!(
            "session_end".parse::<HookEvent>().unwrap(),
            HookEvent::SessionEnd
        );
    }

    #[test]
    fn rejects_unknown_event() {
        assert!("nope".parse::<HookEvent>().is_err());
    }

    #[test]
    fn dispatch_maps_events_to_actions() {
        assert_eq!(dispatch(HookEvent::SessionStart), vec![Action::WakeUp]);
        assert_eq!(dispatch(HookEvent::TurnStart), vec![Action::Recall]);
        assert_eq!(dispatch(HookEvent::SessionEnd), vec![Action::Store]);
    }

    #[test]
    fn validate_tag_accepts_valid_tags() {
        assert!(validate_tag("base").is_ok());
        assert!(validate_tag("rust-lang").is_ok());
        assert!(validate_tag("work_project").is_ok());
        assert!(validate_tag("tag123").is_ok());
        assert!(validate_tag("my-tag_123").is_ok());
    }

    #[test]
    fn validate_tag_rejects_empty() {
        assert!(validate_tag("").is_err());
    }

    #[test]
    fn validate_tag_rejects_special_characters() {
        assert!(validate_tag("tag:space").is_err());
        assert!(validate_tag("tag space").is_err());
        assert!(validate_tag("tag/path").is_err());
        assert!(validate_tag("tag.dot").is_err());
        assert!(validate_tag("tag@at").is_err());
        assert!(validate_tag("tag#hash").is_err());
        assert!(validate_tag("tag$dollar").is_err());
        assert!(validate_tag("tag\"quote").is_err());
    }

    #[test]
    fn validate_tag_rejects_query_injection_attempts() {
        // Attempts to inject ICM query syntax
        assert!(validate_tag("tag,malicious").is_err());
        assert!(validate_tag("tag OR other").is_err());
        assert!(validate_tag("tag AND other").is_err());
    }
}
