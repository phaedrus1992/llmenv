//! Post-session reflective memory consolidation (R5).
//!
//! When enabled, the post_session lifecycle hook calls `run()` to distill
//! episodic session memories into durable semantic rules. The LLM
//! integration (actual summarization) is a future enhancement — this
//! module provides the infrastructure and a diagnostic message.

use crate::hook_run::mcp_client::McpHttpClient;

/// Run post-session consolidation if enabled by the active memory config.
///
/// Recalls recent episodic memories from the ICM backend and stores a
/// consolidation result. Currently a diagnostic stub — the LLM
/// summarization pipeline will be added in a follow-up.
///
/// # Errors
/// Propagates MCP client errors from the ICM backend.
pub async fn run(
    config: &crate::config::Config,
    _client: &McpHttpClient,
) -> anyhow::Result<String> {
    // Find the active memory config and check if consolidation is enabled.
    let Some(cc) = config
        .features
        .as_ref()
        .and_then(|f| f.memory.iter().find_map(|m| m.consolidation.as_ref()))
        .filter(|c| c.enabled)
    else {
        return Ok(String::new());
    };

    tracing::info!(
        max_rules = cc.max_rules_per_session,
        "running post-session consolidation"
    );

    // ponytail: LLM integration deferred to follow-up.
    let msg = format!(
        "llmenv consolidation: post-session distillation not yet implemented. \
         LLM integration is a future enhancement \
         (max_rules_per_session: {})",
        cc.max_rules_per_session,
    );
    tracing::debug!("{msg}");
    Ok(msg)
}
