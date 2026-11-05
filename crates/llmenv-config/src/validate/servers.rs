//! Checks of the MCP servers, the host table, and the feature entries that name a server (#3322).

use std::collections::{BTreeMap, HashSet};

use super::{ValidateError, allowed_root_shape_is_valid, is_valid_hostname, is_valid_var_name};
use crate::{
    CodebaseMemory, Features, HostEntry, McpServer, McpTransport, Memory, TaskTracker, Throttle,
};

/// Each MCP server: tags, a free name, the field its transport needs, and its env keys.
pub(super) fn validate_mcp_servers(mcps: &[McpServer]) -> Result<(), ValidateError> {
    let mut seen = HashSet::new();
    for m in mcps {
        if m.when.is_empty() {
            return Err(ValidateError::McpNoTags(m.name.clone()));
        }
        if m.name == crate::MEMORY_MCP_NAME {
            return Err(ValidateError::McpReservedName(m.name.clone()));
        }
        if !seen.insert(&m.name) {
            return Err(ValidateError::DuplicateMcpName(m.name.clone()));
        }
        validate_transport(m)?;
        validate_mcp_env(m)?;
    }
    Ok(())
}

fn validate_transport(m: &McpServer) -> Result<(), ValidateError> {
    match m.transport {
        McpTransport::Stdio if m.command.is_none() => {
            Err(ValidateError::McpStdioMissingCommand(m.name.clone()))
        }
        McpTransport::Http | McpTransport::Sse if m.url.is_none() => {
            Err(ValidateError::McpRemoteMissingUrl(
                m.name.clone(),
                format!("{:?}", m.transport).to_lowercase(),
            ))
        }
        _ => Ok(()),
    }
}

fn validate_mcp_env(m: &McpServer) -> Result<(), ValidateError> {
    for key in m.env.keys() {
        let (mcp, key) = (m.name.clone(), key.clone());
        if crate::RESERVED_STATE_ENV_VARS.contains(&key.as_str()) {
            return Err(ValidateError::McpReservedEnvKey { mcp, key });
        }
        if key.starts_with("LLMENV_") {
            return Err(ValidateError::McpLlmenvPrefixEnvKey { mcp, key });
        }
        if !is_valid_var_name(&key) {
            return Err(ValidateError::McpInvalidEnvKey { mcp, key });
        }
    }
    Ok(())
}

/// Each address in the host table is a valid hostname or an IP literal.
pub(super) fn validate_hosts(host: &BTreeMap<String, HostEntry>) -> Result<(), ValidateError> {
    for (name, entry) in host {
        if entry.addr.parse::<std::net::IpAddr>().is_err() && !is_valid_hostname(&entry.addr) {
            return Err(ValidateError::InvalidHostAddr(
                name.clone(),
                entry.addr.clone(),
            ));
        }
    }
    Ok(())
}

/// The memory, throttle, task tracker, and codebase-memory entries of `features`.
pub(super) fn validate_features(
    features: &Features,
    host: &BTreeMap<String, HostEntry>,
) -> Result<(), ValidateError> {
    for mem in &features.memory {
        validate_memory(mem, host)?;
    }
    for th in &features.throttle {
        validate_throttle(th)?;
    }
    if let Some(tracker) = &features.task_tracker {
        validate_task_tracker(tracker)?;
    }
    for cm in &features.codebase_memory {
        validate_codebase_memory(cm)?;
    }
    Ok(())
}

fn validate_memory(mem: &Memory, host: &BTreeMap<String, HostEntry>) -> Result<(), ValidateError> {
    if mem.when.is_empty() {
        return Err(ValidateError::MemoryNoTags);
    }
    if !host.contains_key(&mem.server_host) {
        return Err(ValidateError::MemoryUnknownServerHost(
            mem.server_host.clone(),
        ));
    }
    if mem.listen_host.parse::<std::net::IpAddr>().is_err() {
        return Err(ValidateError::MemoryInvalidListenHost(
            mem.listen_host.clone(),
        ));
    }
    match mem.wakeup_max_tokens {
        Some(n) if !crate::WAKEUP_MAX_TOKENS_RANGE.contains(&n) => {
            Err(ValidateError::MemoryWakeupMaxTokensInvalid(n))
        }
        _ => Ok(()),
    }
}

fn validate_throttle(th: &Throttle) -> Result<(), ValidateError> {
    if th.when.is_empty() {
        return Err(ValidateError::ThrottleNoTags(th.backend.clone()));
    }
    if th.backend.is_empty() {
        return Err(ValidateError::ThrottleEmptyBackend);
    }
    Ok(())
}

fn validate_task_tracker(tracker: &TaskTracker) -> Result<(), ValidateError> {
    for (name, value) in [
        ("nudge_after", tracker.nudge_after),
        ("nudge_every", tracker.nudge_every),
        ("stop_nudge_every", tracker.stop_nudge_every),
    ] {
        if value == Some(0) {
            return Err(ValidateError::TaskTrackerNudgeCountInvalid(name));
        }
    }
    if tracker
        .workflow_skills
        .iter()
        .flatten()
        .any(|s| s.trim().is_empty())
    {
        return Err(ValidateError::TaskTrackerSkillEmpty);
    }
    Ok(())
}

/// The budget range in MiB that codebase-memory-mcp accepts: 1 MiB to 1 TiB.
const MEM_BUDGET_MB_RANGE: std::ops::RangeInclusive<u32> = 1..=1_048_576;

fn validate_codebase_memory(cm: &CodebaseMemory) -> Result<(), ValidateError> {
    if cm.when.is_empty() {
        return Err(ValidateError::CodebaseMemoryNoTags);
    }
    if let Some(budget) = cm.mem_budget_mb
        && !MEM_BUDGET_MB_RANGE.contains(&budget)
    {
        return Err(ValidateError::CodebaseMemoryBudgetInvalid(budget));
    }
    // The shape only: a variable may expand to an absolute path at session start.
    match cm
        .allowed_roots
        .iter()
        .find(|r| !allowed_root_shape_is_valid(r))
    {
        Some(bad) => Err(ValidateError::CodebaseMemoryRootInvalid(bad.clone())),
        None => Ok(()),
    }
}
