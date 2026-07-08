//! Engine-neutral agent lifecycle hooks that inject ICM memory context over MCP.
//!
//! `run(event)` is the CLI entry. It resolves the active config, finds the
//! memory backend's HTTP URL, runs the actions configured for `event`, and
//! prints the adapter-formatted context to stdout. Every failure degrades to a
//! one-line stderr warning and exit 0 — lifecycle hooks run on the agent's hot
//! path and must never block it.

mod action;
pub(crate) mod mcp_client;

use std::io::Write;
use std::str::FromStr;
use std::time::Duration;

use action::Action;
use mcp_client::McpHttpClient;
use tracing::{debug, warn};

use crate::adapter::AgentAdapter;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::mcp::resolve::MEMORY_MCP_NAME;
use crate::mcp::resolve::{ResolvedKind, resolve_mcps};

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
    if tags.is_empty() {
        debug!("no tags configured for recall");
        return Ok(Vec::new());
    }
    debug!(tag_count = tags.len(), "building tag recall queries");
    tags.iter()
        .map(|tag| {
            validate_tag(tag).map_err(|e| {
                warn!(tag = %tag, error = %e, "tag name validation failed");
                e
            })?;
            debug!(tag = %tag, "tag recall query created");
            Ok(TagRecallQuery {
                tag: tag.clone(),
                keyword: action::tag_keyword(tag),
            })
        })
        .collect()
}

/// A single cross-project, bundle-scoped recall the TurnStart hook issues
/// against ICM. Mirrors [`TagRecallQuery`] for bundles (#228): each query is
/// **project-unfiltered** (`project: ""`) and keyed on
/// `llmenv-bundle:<bundle>`, so memory stored under that bundle in any project
/// surfaces when the bundle activates here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRecallQuery {
    /// The active bundle this recall targets.
    pub bundle: String,
    /// The `llmenv-bundle:<bundle>` keyword the recall is keyed on.
    pub keyword: String,
}

/// Build the cross-project bundle recall queries for a set of active bundles.
/// One query per bundle, in input order. Bundle names are validated first; an
/// invalid name aborts the whole set so a malformed bundle can't inject recall
/// metacharacters.
///
/// # Errors
/// Returns an error if any bundle name fails [`validate_bundle`].
pub fn bundle_recall_queries(bundles: &[String]) -> anyhow::Result<Vec<BundleRecallQuery>> {
    if bundles.is_empty() {
        debug!("no bundles configured for recall");
        return Ok(Vec::new());
    }
    debug!(
        bundle_count = bundles.len(),
        "building bundle recall queries"
    );
    bundles
        .iter()
        .map(|bundle| {
            validate_bundle(bundle).map_err(|e| {
                warn!(
                    bundle = %bundle,
                    error = %e,
                    "bundle name validation failed"
                );
                e
            })?;
            debug!(bundle = %bundle, "bundle recall query created");
            Ok(BundleRecallQuery {
                bundle: bundle.clone(),
                keyword: action::bundle_keyword(bundle),
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

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            HookEvent::SessionStart => "session_start",
            HookEvent::TurnStart => "turn_start",
            HookEvent::SessionEnd => "session_end",
        };
        f.write_str(s)
    }
}

/// The ordered actions to run for an event, given the active tags' and bundles'
/// recall queries (built by [`tag_recall_queries`] and [`bundle_recall_queries`],
/// the single sources of tag→recall and bundle→recall expansion).
///
/// `TurnStart` runs the project-scoped natural-language `Recall` first, then one
/// project-unfiltered `RecallTag` per active tag (#197), then one
/// project-unfiltered `RecallBundle` per active bundle (#228).
fn dispatch(
    event: HookEvent,
    tag_queries: &[TagRecallQuery],
    bundle_queries: &[BundleRecallQuery],
) -> Vec<Action> {
    match event {
        HookEvent::SessionStart => vec![Action::WakeUp],
        HookEvent::TurnStart => {
            let mut actions = vec![Action::Recall];
            actions.extend(tag_queries.iter().cloned().map(Action::RecallTag));
            actions.extend(bundle_queries.iter().cloned().map(Action::RecallBundle));
            actions
        }
        HookEvent::SessionEnd => vec![Action::Store],
    }
}

/// Detect which adapter is running this hook by checking each registered
/// adapter's environment signal. Falls back to Claude Code when no signal
/// is found (backward-compatible default).
fn active_adapter() -> Box<dyn AgentAdapter> {
    crate::adapter::registered_adapters()
        .into_iter()
        .find(|a| match a.name() {
            "claude-code" => std::env::var("CLAUDE_CONFIG_DIR").is_ok(),
            "crush" => std::env::var("CRUSH_GLOBAL_CONFIG").is_ok(),
            _ => false,
        })
        .unwrap_or_else(|| Box::new(ClaudeCodeAdapter))
}

/// CLI entry. Fail-soft: a warning + empty stdout + exit 0 on any error. Returns
/// `Ok(())` even when the backend is unreachable.
pub fn run(event: &str) -> anyhow::Result<()> {
    use std::io::Read;

    let mut stdin_buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        eprintln!("llmenv hook-run: failed to read stdin: {e}");
    }
    let hook_event_name = serde_json::from_str::<serde_json::Value>(&stdin_buf)
        .ok()
        .and_then(|v| v["hook_event_name"].as_str().map(str::to_owned))
        .unwrap_or_default();

    let parsed = match HookEvent::from_str(event) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("llmenv: {e}");
            return Ok(());
        }
    };
    let adapter = active_adapter();
    match run_inner(parsed) {
        Ok(text) => {
            let out = adapter.emit_hook_context(&hook_event_name, &text);
            if !out.is_empty()
                && let Err(e) = writeln!(std::io::stdout(), "{out}")
                && e.kind() != std::io::ErrorKind::BrokenPipe
            {
                eprintln!("llmenv: failed to write hook output: {e}");
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

    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let url = memory_url(&config, config_dir, &active)?
        .ok_or_else(|| anyhow::anyhow!("no memory backend active for this scope"))?;

    // Recall query: the sorted active tags. Store content: the llmenv context
    // chunk (tags/bundles/project).
    let mut tags = active.tags.iter().cloned().collect::<Vec<_>>();
    tags.sort();
    // Bundles: collect from all active scopes, deduplicate, sort.
    let bundles: Vec<String> = {
        let mut set = std::collections::BTreeSet::new();
        for scope in &active.scopes {
            for b in &scope.enable_bundles {
                set.insert(b.clone());
            }
        }
        set.into_iter().collect()
    };
    // Build per-tag and per-bundle recall queries. Validation rejects query
    // injection; these are the single sources of the tag/bundle→keyword encoding.
    let tag_queries = tag_recall_queries(&tags)?;
    let bundle_queries = bundle_recall_queries(&bundles)?;
    let query = tags.join(", ");
    let chunk = crate::icm::generate_context_chunk(&active, &bundles);

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
        for action in dispatch(event, &tag_queries, &bundle_queries) {
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
///
/// Mirrors the `build_manifest` merge strategy: top-level config memory is
/// combined with bundle-contributed memory entries so a daemon declared only
/// in a `bundle.yaml` is reachable from lifecycle hooks.
fn memory_url(
    config: &crate::config::Config,
    config_dir: &std::path::Path,
    active: &crate::scope::ActiveScopes,
) -> anyhow::Result<Option<String>> {
    let top_memory = config
        .features
        .as_ref()
        .map(|f| f.memory.as_slice())
        .unwrap_or_default();

    // Collect bundle-contributed memory and host entries.
    let manually_enabled: std::collections::BTreeSet<&str> = active
        .scopes
        .iter()
        .flat_map(|s| s.enable_bundles.iter().map(String::as_str))
        .collect();
    let firing: Vec<&crate::config::Bundle> = config
        .bundle
        .iter()
        .filter(|b| {
            b.when.iter().any(|bt| active.tags.contains(bt))
                || manually_enabled.contains(b.name.as_str())
        })
        .collect();

    let bundle_refs = build_hook_bundle_refs(config_dir, &firing);
    let (bundle_memory, bundle_host) = if bundle_refs.is_empty() {
        (Vec::new(), std::collections::BTreeMap::new())
    } else {
        let merged = crate::merge::merge(&config.capabilities, &config.native, &bundle_refs)
            .unwrap_or_default();
        let mem = merged
            .capabilities
            .features
            .map(|f| f.memory)
            .unwrap_or_default();
        (mem, merged.capabilities.host)
    };

    let mut all_memory: Vec<crate::config::Memory> = top_memory
        .iter()
        .chain(bundle_memory.iter())
        .cloned()
        .collect();
    crate::util::dedup(&mut all_memory);

    // Merged host: bundle contributions first, top-level overwrites (same as build_manifest).
    let mut all_host = bundle_host;
    for (k, v) in &config.host {
        all_host.insert(k.clone(), v.clone());
    }

    let resolved = resolve_mcps(&config.mcp, &all_memory, &all_host, &active.tags)
        .map_err(|e| anyhow::anyhow!("failed to resolve MCP servers: {e}"))?;
    Ok(resolved.into_iter().find_map(|m| match m.kind {
        ResolvedKind::Remote { url, .. } if m.name == MEMORY_MCP_NAME => Some(url),
        _ => None,
    }))
}

/// Build lightweight `BundleRef`s for the bundles firing in hook context.
/// All refs get precedence 1 (approximate) — sufficient for memory concat+dedup;
/// callers that need exact scalar precedence use the full `build_bundle_refs` in cli.
fn build_hook_bundle_refs(
    config_dir: &std::path::Path,
    firing: &[&crate::config::Bundle],
) -> Vec<crate::merge::BundleRef> {
    let bundles_dir = config_dir.join("bundles");
    firing
        .iter()
        .filter_map(|b| {
            let path = bundles_dir.join(&b.name);
            path.exists().then_some(crate::merge::BundleRef {
                name: b.name.clone(),
                path,
                precedence: 1,
            })
        })
        .collect()
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

/// Validate a bundle name to prevent query injection. Bundle names follow the
/// same character rules as tags: alphanumeric, hyphens, and underscores only.
fn validate_bundle(bundle: &str) -> anyhow::Result<()> {
    if bundle.is_empty() {
        return Err(anyhow::anyhow!("empty bundle name in recall query"));
    }
    if !bundle
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow::anyhow!(
            "bundle '{}' contains invalid characters (only alphanumeric, -, _ allowed)",
            bundle
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
        assert_eq!(
            dispatch(HookEvent::SessionStart, &[], &[]),
            vec![Action::WakeUp]
        );
        assert_eq!(
            dispatch(HookEvent::TurnStart, &[], &[]),
            vec![Action::Recall]
        );
        assert_eq!(
            dispatch(HookEvent::SessionEnd, &[], &[]),
            vec![Action::Store]
        );
    }

    #[test]
    fn turn_start_expands_one_recall_tag_per_active_tag() {
        let tags = vec!["rust".to_string(), "work-vpn".to_string()];
        let queries = tag_recall_queries(&tags).expect("valid tags");
        let actions = dispatch(HookEvent::TurnStart, &queries, &[]);
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
    fn turn_start_expands_one_recall_bundle_per_active_bundle() {
        let bundles = vec!["base".to_string(), "rust-defaults".to_string()];
        let queries = bundle_recall_queries(&bundles).expect("valid bundles");
        let actions = dispatch(HookEvent::TurnStart, &[], &queries);
        assert_eq!(
            actions,
            vec![
                Action::Recall,
                Action::RecallBundle(BundleRecallQuery {
                    bundle: "base".to_string(),
                    keyword: "llmenv-bundle:base".to_string(),
                }),
                Action::RecallBundle(BundleRecallQuery {
                    bundle: "rust-defaults".to_string(),
                    keyword: "llmenv-bundle:rust-defaults".to_string(),
                }),
            ],
            "TurnStart must emit one bundle recall per active bundle"
        );
    }

    #[test]
    fn turn_start_interleaves_tag_and_bundle_recalls() {
        let tag_qs = tag_recall_queries(&["rust".to_string()]).expect("valid");
        let bundle_qs = bundle_recall_queries(&["base".to_string()]).expect("valid");
        let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs);
        // Order: project recall, then tag recalls, then bundle recalls.
        assert_eq!(actions[0], Action::Recall);
        assert!(matches!(actions[1], Action::RecallTag(_)));
        assert!(matches!(actions[2], Action::RecallBundle(_)));
        assert_eq!(actions.len(), 3);
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

    #[test]
    fn dispatch_tag_and_bundle_with_same_name_produce_distinct_recalls() {
        // A name valid as both a tag and a bundle must produce two separate
        // recalls keyed on different prefixes — no cross-contamination.
        let tag_qs = tag_recall_queries(&["foo".to_string()]).expect("valid");
        let bundle_qs = bundle_recall_queries(&["foo".to_string()]).expect("valid");
        let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs);
        assert_eq!(actions.len(), 3);
        match &actions[1] {
            Action::RecallTag(q) => assert_eq!(q.keyword, "llmenv-tag:foo"),
            other => panic!("expected RecallTag, got {other:?}"),
        }
        match &actions[2] {
            Action::RecallBundle(q) => assert_eq!(q.keyword, "llmenv-bundle:foo"),
            other => panic!("expected RecallBundle, got {other:?}"),
        }
    }

    #[test]
    fn bundle_recall_queries_validates_bundle_names() {
        assert!(bundle_recall_queries(&["".to_string()]).is_err());
        assert!(bundle_recall_queries(&["bundle:invalid".to_string()]).is_err());
        assert!(bundle_recall_queries(&["bundle space".to_string()]).is_err());
        assert!(bundle_recall_queries(&["bundle/path".to_string()]).is_err());
    }

    #[test]
    fn validate_bundle_rejects_empty() {
        assert!(validate_bundle("").is_err());
    }

    #[test]
    fn validate_bundle_rejects_special_characters() {
        assert!(validate_bundle("bundle:invalid").is_err());
        assert!(validate_bundle("bundle space").is_err());
        assert!(validate_bundle("bundle/path").is_err());
        assert!(validate_bundle("bundle.dot").is_err());
    }

    #[test]
    fn validate_bundle_rejects_query_injection_attempts() {
        assert!(validate_bundle("bundle,malicious").is_err());
        assert!(validate_bundle("bundle OR other").is_err());
        assert!(validate_bundle("bundle AND other").is_err());
    }

    use proptest::prelude::*;

    fn valid_name() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{1,24}"
    }

    proptest! {
        // dispatch(TurnStart) always produces [Recall, N×RecallTag, M×RecallBundle]
        // regardless of N and M. This is the ordering invariant.
        #[test]
        fn prop_dispatch_turn_start_ordering(
            tags in proptest::collection::vec(valid_name(), 0..8),
            bundles in proptest::collection::vec(valid_name(), 0..8),
        ) {
            let tag_qs = tag_recall_queries(&tags).expect("valid tags");
            let bundle_qs = bundle_recall_queries(&bundles).expect("valid bundles");
            let actions = dispatch(HookEvent::TurnStart, &tag_qs, &bundle_qs);

            prop_assert_eq!(actions.len(), 1 + tags.len() + bundles.len());
            prop_assert!(matches!(actions[0], Action::Recall));
            for a in &actions[1..=tags.len()] {
                prop_assert!(matches!(a, Action::RecallTag(_)), "expected RecallTag, got {a:?}");
            }
            for a in &actions[1 + tags.len()..] {
                prop_assert!(
                    matches!(a, Action::RecallBundle(_)),
                    "expected RecallBundle, got {a:?}"
                );
            }
        }
    }
}
