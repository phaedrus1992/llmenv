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

/// The field that the transport needs. The match names every transport, so a new transport
/// fails to compile here until it states what it needs.
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
        McpTransport::Stdio | McpTransport::Http | McpTransport::Sse => Ok(()),
    }
}

fn validate_mcp_env(m: &McpServer) -> Result<(), ValidateError> {
    let fields = |key: &String| (m.name.clone(), key.clone());
    for key in m.env.keys() {
        if crate::RESERVED_STATE_ENV_VARS.contains(&key.as_str()) {
            let (mcp, key) = fields(key);
            return Err(ValidateError::McpReservedEnvKey { mcp, key });
        }
        if key.starts_with("LLMENV_") {
            let (mcp, key) = fields(key);
            return Err(ValidateError::McpLlmenvPrefixEnvKey { mcp, key });
        }
        if !is_valid_var_name(key) {
            let (mcp, key) = fields(key);
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

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn server(name: &str) -> McpServer {
        McpServer {
            name: name.to_string(),
            when: vec!["t".to_string()],
            command: Some("echo".to_string()),
            ..McpServer::default()
        }
    }

    fn hosts(addr: &str) -> BTreeMap<String, HostEntry> {
        BTreeMap::from([(
            "h".to_string(),
            HostEntry {
                addr: addr.to_string(),
            },
        )])
    }

    fn transport() -> impl Strategy<Value = McpTransport> {
        prop_oneof![
            Just(McpTransport::Stdio),
            Just(McpTransport::Http),
            Just(McpTransport::Sse)
        ]
    }

    proptest! {
        /// A transport is accepted iff the field it needs is set.
        #[test]
        fn a_transport_needs_exactly_its_own_field(
            transport in transport(),
            command in proptest::option::of("[a-z]{1,8}"),
            url in proptest::option::of("https://[a-z]{1,8}"),
        ) {
            let m = McpServer { transport, command: command.clone(), url: url.clone(), ..server("a") };
            let needed = match transport {
                McpTransport::Stdio => command.is_some(),
                McpTransport::Http | McpTransport::Sse => url.is_some(),
            };
            prop_assert_eq!(validate_transport(&m).is_ok(), needed);
        }

        /// An env key is accepted iff it is not reserved, has no LLMENV_ prefix, and is a
        /// variable name.
        #[test]
        fn an_env_key_is_accepted_by_the_three_rules(
            key in prop_oneof![
                "[A-Za-z_][A-Za-z0-9_]{0,10}",
                "LLMENV_[A-Z]{0,6}",
                "[0-9 -][A-Za-z0-9_ -]{0,6}",
                prop::sample::select(crate::RESERVED_STATE_ENV_VARS.to_vec()).prop_map(str::to_string),
            ],
        ) {
            let mut m = server("a");
            m.env.insert(key.clone(), "v".to_string());
            let accepted = !crate::RESERVED_STATE_ENV_VARS.contains(&key.as_str())
                && !key.starts_with("LLMENV_")
                && is_valid_var_name(&key);
            prop_assert_eq!(validate_mcp_env(&m).is_ok(), accepted);
        }

        /// A list is refused with DuplicateMcpName iff a name repeats.
        #[test]
        fn a_repeated_name_is_the_duplicate_error(names in prop::collection::vec("[a-c]", 0..5)) {
            let servers: Vec<McpServer> = names.iter().map(|n| server(n)).collect();
            let unique: HashSet<&String> = names.iter().collect();
            let result = validate_mcp_servers(&servers);
            if unique.len() == names.len() {
                prop_assert!(result.is_ok(), "{:?}", result);
            } else {
                prop_assert!(matches!(result, Err(ValidateError::DuplicateMcpName(_))), "{:?}", result);
            }
        }

        /// A host address is accepted iff it is an IP literal or a hostname.
        #[test]
        fn a_host_address_is_an_ip_or_a_hostname(
            addr in prop_oneof![
                any::<std::net::Ipv4Addr>().prop_map(|a| a.to_string()),
                any::<std::net::Ipv6Addr>().prop_map(|a| a.to_string()),
                "[a-z0-9.-]{0,20}",
                "\\PC{0,12}",
            ],
        ) {
            let accepted = addr.parse::<std::net::IpAddr>().is_ok() || is_valid_hostname(&addr);
            prop_assert_eq!(validate_hosts(&hosts(&addr)).is_ok(), accepted);
        }

        /// A memory listen host is accepted iff it is an IP literal.
        #[test]
        fn a_listen_host_must_be_an_ip(
            listen in prop_oneof![
                any::<std::net::IpAddr>().prop_map(|a| a.to_string()),
                "\\PC{0,12}",
            ],
        ) {
            let mem = Memory {
                server_host: "h".to_string(),
                listen_host: listen.clone(),
                when: vec!["t".to_string()],
                ..Memory::default()
            };
            let result = validate_memory(&mem, &hosts("127.0.0.1"));
            prop_assert_eq!(result.is_ok(), listen.parse::<std::net::IpAddr>().is_ok());
        }

        /// A nudge count is refused iff it is zero, and a blank skill is always refused.
        #[test]
        fn task_tracker_counts_and_skills(
            count in any::<u32>(),
            field in 0usize..3,
            blank in "[ \t\n]{0,4}",
        ) {
            let mut tracker = TaskTracker::default();
            match field {
                0 => tracker.nudge_after = Some(count),
                1 => tracker.nudge_every = Some(count),
                _ => tracker.stop_nudge_every = Some(count),
            }
            prop_assert_eq!(validate_task_tracker(&tracker).is_ok(), count != 0);
            let skills = TaskTracker {
                workflow_skills: Some(vec!["ship".to_string(), blank]),
                ..TaskTracker::default()
            };
            prop_assert!(matches!(
                validate_task_tracker(&skills),
                Err(ValidateError::TaskTrackerSkillEmpty)
            ));
        }

        /// A codebase-memory budget is accepted iff it is in the range.
        #[test]
        fn a_budget_is_accepted_iff_in_range(budget in any::<u32>()) {
            let cm = CodebaseMemory {
                when: vec!["t".to_string()],
                mem_budget_mb: Some(budget),
                ..CodebaseMemory::default()
            };
            prop_assert_eq!(
                validate_codebase_memory(&cm).is_ok(),
                MEM_BUDGET_MB_RANGE.contains(&budget)
            );
        }
    }

    #[test]
    fn every_ip_form_is_a_listen_host() {
        let mem = Memory {
            server_host: "h".to_string(),
            listen_host: "::1".to_string(),
            when: vec!["t".to_string()],
            ..Memory::default()
        };
        validate_memory(&mem, &hosts("127.0.0.1")).unwrap();
    }
}
