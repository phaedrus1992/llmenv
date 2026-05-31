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

/// A single cross-project, tag-scoped recall the TurnStart hook issues against
/// ICM. Exposes the recall contract (#197) so it is testable without a live
/// MCP backend: each query is **project-unfiltered** (`project: ""`) and keyed
/// on `llmenv-tag:<tag>`, so memory stored under that tag in any project
/// surfaces when the tag activates here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRecallQuery {
    /// The active tag this recall targets.
    pub tag: String,
    /// The `llmenv-tag:<tag>` keyword the recall is keyed on.
    pub keyword: String,
}

/// Build the cross-project tag recall queries for a set of active tags.
/// One query per tag, in input order. Tags are validated first; an invalid tag
/// aborts the whole set so a malformed scope can't inject recall metacharacters.
///
/// # Errors
/// Returns an error if any tag fails [`validate_tag`].
pub fn tag_recall_queries(tags: &[String]) -> anyhow::Result<Vec<TagRecallQuery>> {
    tags.iter()
        .map(|tag| {
            validate_tag(tag)?;
            Ok(TagRecallQuery {
                tag: tag.clone(),
                keyword: action::tag_keyword(tag),
            })
        })
        .collect()
}

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

/// The ordered actions to run for an event, given the active tags' recall
/// queries (built by [`tag_recall_queries`], the single source of tag→recall
/// expansion).
///
/// `TurnStart` runs the project-scoped natural-language `Recall` first, then one
/// project-unfiltered `RecallTag` per active tag so tag-scoped memory written in
/// any project surfaces here (#197).
fn dispatch(event: HookEvent, tag_queries: &[TagRecallQuery]) -> Vec<Action> {
    match event {
        HookEvent::SessionStart => vec![Action::WakeUp],
        HookEvent::TurnStart => {
            let mut actions = vec![Action::Recall];
            actions.extend(tag_queries.iter().cloned().map(Action::RecallTag));
            actions
        }
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
    // Build per-tag recall queries. This validates every tag (rejecting query
    // injection) and is the single source of the tag→keyword encoding.
    let tag_queries = tag_recall_queries(&tags)?;
    let query = tags.join(", ");
    let chunk = crate::icm::generate_context_chunk(&active, &[]);

    let client = McpHttpClient::new(url, HOOK_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("invalid memory backend URL: {e}"))?;
    // Current-thread runtime: lifecycle hooks run on the agent's hot path (session
    // start + every prompt turn) and only need to `block_on` a short sequence of
    // HTTP round-trips. A multi-threaded runtime would spin up a worker thread pool
    // that's pure overhead for this single sequential await. (#186)
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut out = String::new();
        for action in dispatch(event, &tag_queries) {
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
        assert_eq!(dispatch(HookEvent::SessionStart, &[]), vec![Action::WakeUp]);
        assert_eq!(dispatch(HookEvent::TurnStart, &[]), vec![Action::Recall]);
        assert_eq!(dispatch(HookEvent::SessionEnd, &[]), vec![Action::Store]);
    }

    #[test]
    fn turn_start_expands_one_recall_tag_per_active_tag() {
        let tags = vec!["rust".to_string(), "work-vpn".to_string()];
        let queries = tag_recall_queries(&tags).expect("valid tags");
        let actions = dispatch(HookEvent::TurnStart, &queries);
        assert_eq!(
            actions,
            vec![
                Action::Recall,
                Action::RecallTag(TagRecallQuery {
                    tag: "rust".to_string(),
                    keyword: "llmenv-tag:rust".to_string(),
                }),
                Action::RecallTag(TagRecallQuery {
                    tag: "work-vpn".to_string(),
                    keyword: "llmenv-tag:work-vpn".to_string(),
                }),
            ],
            "TurnStart must run project recall then one tag recall per active tag"
        );
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
