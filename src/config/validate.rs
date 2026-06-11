use super::Config;
use thiserror::Error;

#[cfg(test)]
use super::{
    Bundle, Cache, Capabilities, Features, Hook, HookHandler, HookHandlerKind, HostEntry,
    HostMatch, HostScope, Marketplace, McpServer, McpTransport, Memory, NativePermissionRules,
    NetworkMatch, NetworkScope, PermissionMode, PermissionRule, Permissions, PluginCollection,
    Scopes, UserMatch, UserScope,
};

#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("duplicate scope id: {0}")]
    DuplicateScopeId(String),
    #[error("bundle {0} has no tags")]
    BundleNoTags(String),
    #[error("duplicate bundle name: {0}")]
    DuplicateBundleName(String),
    #[error("invalid CIDR notation: {0}")]
    InvalidCIDR(String),
    #[error("invalid MAC address: {0}")]
    InvalidMACAddress(String),
    #[error("invalid hostname: {0}")]
    InvalidHostname(String),
    #[error("bundle {0}: invalid variable name '{1}' (must match [A-Za-z_][A-Za-z0-9_]*)")]
    InvalidVarName(String, String),
    #[error("cache_dir contains path traversal components: {0}")]
    CacheDirTraversal(String),
    #[error("cache_retention_hours must be > 0")]
    CacheRetentionInvalid,
    #[error("duplicate mcp name: {0}")]
    DuplicateMcpName(String),
    #[error("mcp name '{0}' is reserved for the memory backend")]
    McpReservedName(String),
    #[error("mcp {0} has no tags")]
    McpNoTags(String),
    #[error("mcp {0}: stdio transport requires a `command`")]
    McpStdioMissingCommand(String),
    #[error("mcp {0}: {1} transport requires a `url`")]
    McpRemoteMissingUrl(String, String),
    #[error("memory: server_host '{0}' has no entry in the `host:` table")]
    MemoryUnknownServerHost(String),
    #[error("memory has no tags")]
    MemoryNoTags,
    #[error(
        "memory: listen_host '{0}' is not a valid IP address literal (hostnames not supported)"
    )]
    MemoryInvalidListenHost(String),
    #[error("duplicate marketplace name: {0}")]
    DuplicateMarketplaceName(String),
    #[error(
        "invalid marketplace name '{0}' (must match [A-Za-z0-9._-]+, not '.'/'..', no leading '-')"
    )]
    InvalidMarketplaceName(String),
    #[error("marketplace {0} has an empty source")]
    MarketplaceEmptySource(String),
    #[error(
        "marketplace '{name}' uses a name reserved for official Anthropic marketplaces, \
         which require a GitHub source under the '{owner}' org; got source '{got}'. \
         Fix: set source to 'https://github.com/{owner}/<repo>' (e.g. \
         'https://github.com/{owner}/claude-code'), or rename the marketplace."
    )]
    ReservedMarketplaceSource {
        name: String,
        got: String,
        owner: &'static str,
    },
    #[error(
        "state tool env '{0}' is not a valid environment variable name \
         (must match [A-Za-z_][A-Za-z0-9_]*)"
    )]
    StateInvalidEnvName(String),
    #[error(
        "state tool subdir '{0}' is not a safe single path component \
         (no '/', '\\', ':', NUL, '..', '.', or empty)"
    )]
    StateInvalidSubdir(String),
    #[error(
        "state tool env '{0}' is declared more than once \
         (or collides with an llmenv-emitted var: LLMENV_STATE_DIR, CLAUDE_CONFIG_DIR)"
    )]
    StateDuplicateEnv(String),
    #[error("duplicate plugin-collection name: {0}")]
    DuplicatePluginCollectionName(String),
    #[error("plugin-collection {0} has no tags")]
    PluginCollectionNoTags(String),
    #[error(
        "plugin-collection {collection}: invalid plugin '{plugin}' (must be '<marketplace>:<plugin>')"
    )]
    InvalidPluginRef { collection: String, plugin: String },
    #[error(
        "plugin-collection {collection}: plugin '{plugin}' references unknown marketplace '{marketplace}'"
    )]
    UnknownPluginMarketplace {
        collection: String,
        plugin: String,
        marketplace: String,
    },
    #[error(
        "conflicting scalar values for '{key}' in tags {tags}: {contributors}. \
         Same-precedence scopes may not define conflicting scalars (use native overrides or change tag sets)"
    )]
    TagConflictSameScopeScalar {
        key: String,
        tags: String,
        contributors: String,
    },
    #[error(
        "{context}: capabilities.env key '{key}' is reserved — it is emitted by the \
         adapter or state system and must not be overridden here. \
         Fix: remove this key from env:, or use bundle.vars for template variables."
    )]
    CapabilitiesReservedEnvKey { context: String, key: String },
    #[error(
        "{context}: capabilities.env key '{key}' uses the 'LLMENV_' prefix, which is \
         reserved for llmenv-internal variables. Fix: rename the key."
    )]
    CapabilitiesLlmenvPrefixEnvKey { context: String, key: String },
}

/// A marketplace name is safe to use as a single filesystem path component and
/// as a JSON key: non-empty, only `[A-Za-z0-9._-]`, never `.`/`..`, and no
/// leading `-` (which a CLI/git would treat as a flag).
fn is_valid_marketplace_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.starts_with('-') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A state-tool subdir must be a single safe path component: non-empty, not
/// `.`/`..`, and free of path separators, drive-letter colons, and NUL, so a
/// relocated tool's state can never escape the durable state directory (#175).
///
/// `\0` is rejected (matching [`is_safe_cache_dir`]) because path APIs handle it
/// inconsistently across platforms; `:` is rejected so a Windows drive-relative
/// component like `C:` cannot be joined into a path outside the state dir.
fn is_safe_state_subdir(subdir: &str) -> bool {
    if subdir.is_empty() || subdir == "." || subdir == ".." {
        return false;
    }
    !subdir.contains('/')
        && !subdir.contains('\\')
        && !subdir.contains(':')
        && !subdir.contains('\0')
}

fn is_valid_cidr(cidr: &str) -> bool {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return false;
    }
    let octets: Vec<&str> = parts[0].split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    for octet in octets {
        // Reject leading zeros ("01") which u8::parse would otherwise accept;
        // RFC 4632 dotted-decimal forbids them and they invite octal confusion.
        if (octet.len() > 1 && octet.starts_with('0')) || octet.parse::<u8>().is_err() {
            return false;
        }
    }
    matches!(parts[1].parse::<u8>(), Ok(n) if n <= 32)
}

fn is_valid_mac_address(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return false;
    }
    parts
        .iter()
        .all(|part| part.len() == 2 && u8::from_str_radix(part, 16).is_ok())
}

fn is_valid_hostname(hostname: &str) -> bool {
    // RFC 1123 §2.1 / RFC 952: total length <= 253 octets, each label
    // 1..=63 octets, labels are alphanumeric plus interior hyphens.
    if hostname.is_empty() || hostname.len() > 253 {
        return false;
    }
    hostname.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Validate a single `capabilities.env` key: not a reserved adapter/state var,
/// not in the LLMENV_* namespace. Returns an error with the given `context`
/// label (e.g. `"config.yaml: capabilities"` or `"bundle 'foo'"`) on failure.
fn validate_capabilities_env_key(context: &str, key: &str) -> Result<(), ValidateError> {
    if crate::materialize::state::RESERVED_STATE_ENV_VARS.contains(&key) {
        return Err(ValidateError::CapabilitiesReservedEnvKey {
            context: context.to_string(),
            key: key.to_string(),
        });
    }
    if key.starts_with("LLMENV_") {
        return Err(ValidateError::CapabilitiesLlmenvPrefixEnvKey {
            context: context.to_string(),
            key: key.to_string(),
        });
    }
    Ok(())
}

fn is_valid_var_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let first = name.as_bytes()[0] as char;
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    name.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_safe_cache_dir(dir: &str) -> bool {
    if dir.is_empty() || dir.len() > 4096 {
        return false;
    }
    // Parse components rather than substring-match so traversal can't slip
    // through as `foo/..` (no trailing slash) or via host-OS separators.
    !dir.contains('\0') && !crate::paths::has_parent_component(dir)
}

impl Config {
    pub fn validate(&self) -> Result<(), ValidateError> {
        if !is_safe_cache_dir(&self.cache.cache_dir) {
            return Err(ValidateError::CacheDirTraversal(
                self.cache.cache_dir.clone(),
            ));
        }
        if let Some(hours) = self.cache.cache_retention_hours
            && hours == 0
        {
            return Err(ValidateError::CacheRetentionInvalid);
        }
        let mut seen_scope_ids = std::collections::HashSet::new();
        let ids = self
            .scope
            .network
            .iter()
            .map(|s| &s.id)
            .chain(self.scope.host.iter().map(|s| &s.id))
            .chain(self.scope.user.iter().map(|s| &s.id));
        for id in ids {
            if !seen_scope_ids.insert(id) {
                return Err(ValidateError::DuplicateScopeId(id.clone()));
            }
        }
        for scope in &self.scope.network {
            if let Some(cidr) = &scope.r#match.cidr
                && !is_valid_cidr(cidr)
            {
                return Err(ValidateError::InvalidCIDR(cidr.clone()));
            }
            if let Some(mac) = &scope.r#match.gateway_mac
                && !is_valid_mac_address(mac)
            {
                return Err(ValidateError::InvalidMACAddress(mac.clone()));
            }
        }
        for scope in &self.scope.host {
            if let Some(hostname) = &scope.r#match.hostname
                && !is_valid_hostname(hostname)
            {
                return Err(ValidateError::InvalidHostname(hostname.clone()));
            }
        }
        let mut seen_bundle_names = std::collections::HashSet::new();
        for b in &self.bundle {
            if b.tags.is_empty() {
                return Err(ValidateError::BundleNoTags(b.name.clone()));
            }
            if !seen_bundle_names.insert(&b.name) {
                return Err(ValidateError::DuplicateBundleName(b.name.clone()));
            }
            for var_name in b.env.keys() {
                if !is_valid_var_name(var_name) {
                    return Err(ValidateError::InvalidVarName(
                        b.name.clone(),
                        var_name.clone(),
                    ));
                }
            }
        }
        for key in self.capabilities.env.keys() {
            validate_capabilities_env_key("config.yaml: capabilities", key)?;
        }
        self.validate_mcps()?;
        self.validate_plugins()?;
        self.validate_state()?;
        Ok(())
    }

    fn validate_plugins(&self) -> Result<(), ValidateError> {
        let mut seen_marketplace_names = std::collections::HashSet::new();
        let mut marketplace_names = std::collections::HashSet::new();
        for m in &self.marketplace {
            // The name is used both as a single path component for the cache
            // clone (`<cache>/marketplaces/<name>`) and as a JSON key in the
            // rendered `extraKnownMarketplaces` / `enabledPlugins`. Constraining
            // it to `[A-Za-z0-9._-]` (and rejecting `.`/`..`/leading-`-`) blocks
            // path traversal out of the cache dir and keeps the `plugin@market`
            // key unambiguous (no embedded `@`/`/`/control chars).
            if !is_valid_marketplace_name(&m.name) {
                return Err(ValidateError::InvalidMarketplaceName(m.name.clone()));
            }
            if m.source.is_empty() {
                return Err(ValidateError::MarketplaceEmptySource(m.name.clone()));
            }
            // A reserved official marketplace name is only accepted by Claude
            // Code when sourced from a github.com/anthropics repo (#190). Reject
            // any other source here with a fix hint rather than letting it fail
            // opaquely inside Claude Code at load time.
            if super::is_reserved_official_marketplace(&m.name) {
                let owner = super::OFFICIAL_MARKETPLACE_OWNER;
                let ok = super::github_owner_repo(&m.source)
                    .is_some_and(|(o, _)| o.eq_ignore_ascii_case(owner));
                if !ok {
                    return Err(ValidateError::ReservedMarketplaceSource {
                        name: m.name.clone(),
                        got: m.source.clone(),
                        owner,
                    });
                }
            }
            if !seen_marketplace_names.insert(&m.name) {
                return Err(ValidateError::DuplicateMarketplaceName(m.name.clone()));
            }
            marketplace_names.insert(m.name.as_str());
        }

        let mut seen_collection_names = std::collections::HashSet::new();
        for c in &self.plugin_collection {
            if c.tags.is_empty() {
                return Err(ValidateError::PluginCollectionNoTags(c.name.clone()));
            }
            if !seen_collection_names.insert(&c.name) {
                return Err(ValidateError::DuplicatePluginCollectionName(c.name.clone()));
            }
            for plugin in &c.plugins {
                let Some((marketplace, _)) = super::split_plugin_ref(plugin) else {
                    return Err(ValidateError::InvalidPluginRef {
                        collection: c.name.clone(),
                        plugin: plugin.clone(),
                    });
                };
                if !marketplace_names.contains(marketplace) {
                    return Err(ValidateError::UnknownPluginMarketplace {
                        collection: c.name.clone(),
                        plugin: plugin.clone(),
                        marketplace: marketplace.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate the durable-state block (#175): each tool's env var must be a
    /// valid shell variable name, its subdir a single safe path component, and
    /// no env var may repeat or collide with a var llmenv itself emits into the
    /// same set (`LLMENV_STATE_DIR`, `CLAUDE_CONFIG_DIR`).
    fn validate_state(&self) -> Result<(), ValidateError> {
        let mut seen_env = std::collections::HashSet::new();
        // Reserve llmenv/adapter-emitted vars so a tool can't shadow them and
        // produce a conflicting binding in the emitted env_vars set.
        for reserved in crate::materialize::state::RESERVED_STATE_ENV_VARS {
            seen_env.insert((*reserved).to_string());
        }
        for tool in &self.state.tools {
            if !is_valid_var_name(&tool.env) {
                return Err(ValidateError::StateInvalidEnvName(tool.env.clone()));
            }
            if !is_safe_state_subdir(&tool.subdir) {
                return Err(ValidateError::StateInvalidSubdir(tool.subdir.clone()));
            }
            if !seen_env.insert(tool.env.clone()) {
                return Err(ValidateError::StateDuplicateEnv(tool.env.clone()));
            }
        }
        Ok(())
    }

    fn validate_mcps(&self) -> Result<(), ValidateError> {
        use super::McpTransport;

        let mut seen_mcp_names = std::collections::HashSet::new();
        for m in &self.mcp {
            if m.tags.is_empty() {
                return Err(ValidateError::McpNoTags(m.name.clone()));
            }
            if m.name == crate::mcp::resolve::MEMORY_MCP_NAME {
                return Err(ValidateError::McpReservedName(m.name.clone()));
            }
            if !seen_mcp_names.insert(&m.name) {
                return Err(ValidateError::DuplicateMcpName(m.name.clone()));
            }
            match m.transport {
                McpTransport::Stdio => {
                    if m.command.is_none() {
                        return Err(ValidateError::McpStdioMissingCommand(m.name.clone()));
                    }
                }
                McpTransport::Http | McpTransport::Sse => {
                    if m.url.is_none() {
                        return Err(ValidateError::McpRemoteMissingUrl(
                            m.name.clone(),
                            format!("{:?}", m.transport).to_lowercase(),
                        ));
                    }
                }
            }
        }
        if let Some(mem) = self.features.as_ref().and_then(|f| f.memory.as_ref()) {
            if mem.tags.is_empty() {
                return Err(ValidateError::MemoryNoTags);
            }
            if !self.host.contains_key(&mem.server_host) {
                return Err(ValidateError::MemoryUnknownServerHost(
                    mem.server_host.clone(),
                ));
            }
            if mem.listen_host.parse::<std::net::IpAddr>().is_err() {
                return Err(ValidateError::MemoryInvalidListenHost(
                    mem.listen_host.clone(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::HashingMode;
    use proptest::prelude::*;

    fn arb_string() -> impl Strategy<Value = String> {
        r"[a-zA-Z0-9_-]{1,20}"
    }

    // Some(arb)/None so the round-trip exercises both branches of every
    // Option<String> match field rather than only the None default.
    fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
        prop::option::of(arb_string())
    }

    fn arb_hashing_mode() -> impl Strategy<Value = HashingMode> {
        prop_oneof![
            Just(HashingMode::Loose),
            Just(HashingMode::Normal),
            Just(HashingMode::Strict),
        ]
    }

    fn arb_cache() -> impl Strategy<Value = Cache> {
        (
            arb_string(),
            0u64..120,
            prop::option::of(0u64..10_000),
            arb_hashing_mode(),
        )
            .prop_map(
                |(cache_dir, sync_interval_minutes, cache_retention_hours, hashing)| Cache {
                    cache_dir,
                    sync_interval_minutes,
                    cache_retention_hours,
                    hashing,
                },
            )
    }

    fn arb_permission_mode() -> impl Strategy<Value = PermissionMode> {
        prop_oneof![
            Just(PermissionMode::AcceptEdits),
            Just(PermissionMode::Plan),
            Just(PermissionMode::Default),
            Just(PermissionMode::BypassPermissions),
        ]
    }

    fn arb_permission_rule() -> impl Strategy<Value = PermissionRule> {
        (
            arb_string(),
            arb_opt_string(),
            prop::collection::vec(arb_string(), 0..3),
        )
            .prop_map(|(tool, pattern, paths)| PermissionRule {
                tool,
                pattern,
                paths,
            })
    }

    fn arb_native_rules() -> impl Strategy<Value = NativePermissionRules> {
        (
            prop::collection::vec(arb_string(), 0..3),
            prop::collection::vec(arb_string(), 0..3),
            prop::collection::vec(arb_string(), 0..3),
        )
            .prop_map(|(allow, ask, deny)| NativePermissionRules { allow, ask, deny })
    }

    fn arb_permissions() -> impl Strategy<Value = Permissions> {
        (
            prop::option::of(arb_permission_mode()),
            prop::collection::vec(arb_permission_rule(), 0..3),
            prop::collection::vec(arb_permission_rule(), 0..3),
            prop::collection::vec(arb_permission_rule(), 0..3),
        )
            .prop_map(|(default_mode, allow, ask, deny)| Permissions {
                default_mode,
                allow,
                ask,
                deny,
            })
    }

    fn arb_hook() -> impl Strategy<Value = Hook> {
        (
            arb_string(),
            arb_opt_string(),
            prop_oneof![
                arb_opt_string().prop_map(|command| HookHandler {
                    kind: HookHandlerKind::Command,
                    command,
                    tool: None,
                }),
                arb_opt_string().prop_map(|tool| HookHandler {
                    kind: HookHandlerKind::McpTool,
                    command: None,
                    tool,
                }),
            ],
        )
            .prop_map(|(event, matcher, handler)| Hook {
                event,
                matcher,
                handler,
                bundle_origin: None,
            })
    }

    fn arb_capabilities() -> impl Strategy<Value = Capabilities> {
        (
            arb_permissions(),
            prop::collection::vec(arb_hook(), 0..3),
            prop::collection::vec(arb_string(), 0..3),
            prop::collection::vec(arb_mcp_server(), 0..3),
            prop::collection::btree_map(arb_string(), arb_native_rules(), 0..3),
        )
            .prop_map(|(permissions, hooks, plugins, mcp, native_permissions)| {
                Capabilities {
                    permissions,
                    hooks,
                    plugins,
                    mcp,
                    native_permissions,
                    ..Default::default()
                }
            })
    }

    fn arb_transport() -> impl Strategy<Value = McpTransport> {
        prop_oneof![
            Just(McpTransport::Stdio),
            Just(McpTransport::Http),
            Just(McpTransport::Sse),
        ]
    }

    fn arb_mcp_server() -> impl Strategy<Value = McpServer> {
        (
            arb_string(),
            prop::collection::vec(arb_string(), 0..3),
            arb_transport(),
            arb_opt_string(),
            prop::collection::vec(arb_string(), 0..3),
            prop::collection::btree_map(arb_string(), arb_string(), 0..3),
            arb_opt_string(),
        )
            .prop_map(
                |(name, tags, transport, command, args, env, url)| McpServer {
                    name,
                    tags,
                    transport,
                    command,
                    args,
                    env,
                    url,
                },
            )
    }

    fn arb_marketplace() -> impl Strategy<Value = Marketplace> {
        (arb_string(), arb_string()).prop_map(|(name, source)| Marketplace { name, source })
    }

    fn arb_plugin_collection() -> impl Strategy<Value = PluginCollection> {
        (
            arb_string(),
            prop::collection::vec(arb_string(), 0..3),
            prop::collection::vec(
                (arb_string(), arb_string()).prop_map(|(m, p)| format!("{m}:{p}")),
                0..3,
            ),
        )
            .prop_map(|(name, tags, plugins)| PluginCollection {
                name,
                tags,
                plugins,
            })
    }

    fn arb_memory() -> impl Strategy<Value = Memory> {
        (
            arb_string(),
            any::<u16>(),
            prop_oneof![
                Just("127.0.0.1".to_string()),
                Just("0.0.0.0".to_string()),
                Just("::1".to_string()),
            ],
            prop::collection::vec(arb_string(), 0..3),
            prop::collection::vec(arb_string(), 0..3),
        )
            .prop_map(
                |(server_host, port, listen_host, tags, default_topics)| Memory {
                    server_host,
                    port,
                    listen_host,
                    tags,
                    default_topics,
                },
            )
    }

    fn arb_config() -> impl Strategy<Value = Config> {
        (
            arb_cache(),
            prop::collection::vec(
                (
                    arb_string(),
                    arb_opt_string(),
                    arb_opt_string(),
                    arb_opt_string(),
                ),
                0..10,
            )
            .prop_map(|ids| {
                let network = ids
                    .iter()
                    .take(2)
                    .map(|(id, gateway_mac, ssid, cidr)| NetworkScope {
                        id: id.clone(),
                        r#match: NetworkMatch {
                            gateway_mac: gateway_mac.clone(),
                            ssid: ssid.clone(),
                            cidr: cidr.clone(),
                        },
                        tags: vec![],
                        env: Default::default(),
                    })
                    .collect();
                let host = ids
                    .iter()
                    .skip(2)
                    .take(2)
                    .map(|(id, hostname, _, _)| HostScope {
                        id: id.clone(),
                        r#match: HostMatch {
                            hostname: hostname.clone(),
                        },
                        tags: vec![],
                        env: Default::default(),
                    })
                    .collect();
                let user = ids
                    .iter()
                    .skip(4)
                    .take(2)
                    .map(|(id, user, _, _)| UserScope {
                        id: id.clone(),
                        r#match: UserMatch { user: user.clone() },
                        tags: vec![],
                        env: Default::default(),
                    })
                    .collect();
                (network, host, user)
            }),
            prop::collection::vec(
                (arb_string(), prop::collection::vec(arb_string(), 1..3)),
                0..3,
            )
            .prop_map(|bundles| {
                bundles
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, tags))| Bundle {
                        name: format!("bundle-{}-{}", i, name),
                        tags,
                        env: Default::default(),
                    })
                    .collect()
            }),
            prop::collection::vec(arb_mcp_server(), 0..3).prop_map(|servers: Vec<McpServer>| {
                // Names must be unique and must avoid the reserved memory
                // name so the config still passes validation; index-prefix
                // them to guarantee both.
                servers
                    .into_iter()
                    .enumerate()
                    .map(|(i, mut s)| {
                        s.name = format!("mcp-{}-{}", i, s.name);
                        s
                    })
                    .collect()
            }),
            prop::option::of(arb_memory()),
            prop::collection::btree_map(
                arb_string(),
                arb_string().prop_map(|addr| HostEntry { addr }),
                0..3,
            ),
            arb_capabilities(),
            prop::collection::vec(arb_marketplace(), 0..3).prop_map(|ms: Vec<Marketplace>| {
                // Names must be unique so the config passes validation;
                // index-prefix to guarantee it.
                ms.into_iter()
                    .enumerate()
                    .map(|(i, mut m)| {
                        m.name = format!("mkt-{i}-{}", m.name);
                        m
                    })
                    .collect()
            }),
            prop::collection::vec(arb_plugin_collection(), 0..3).prop_map(
                |cs: Vec<PluginCollection>| {
                    cs.into_iter()
                        .enumerate()
                        .map(|(i, mut c)| {
                            c.name = format!("col-{i}-{}", c.name);
                            c
                        })
                        .collect()
                },
            ),
        )
            .prop_map(
                |(
                    cache,
                    (network, host_scopes, user),
                    bundle,
                    mcp,
                    memory,
                    host,
                    capabilities,
                    marketplace,
                    plugin_collection,
                )| {
                    Config {
                        cache,
                        scope: Scopes {
                            network,
                            host: host_scopes,
                            user,
                        },
                        capabilities,
                        native: Default::default(),
                        bundle,
                        mcp,
                        features: memory.map(|mem| Features { memory: Some(mem) }),
                        marketplace,
                        plugin_collection,
                        state: Default::default(),
                        host,
                        init: Default::default(),
                    }
                },
            )
    }

    proptest! {
            #[test]
            fn prop_config_yaml_roundtrip(config in arb_config()) {
                let yaml_str = serde_yaml::to_string(&config).expect("serialize failed");
                let deserialized: Config = serde_yaml::from_str(&yaml_str).expect("deserialize failed");
                prop_assert_eq!(config, deserialized, "roundtrip should preserve config");
            }

            #[test]
            fn prop_config_validate_enforces_unique_scope_ids(
                id in arb_string(),
            ) {
                let network = vec![
                    NetworkScope {
                        id: id.clone(),
                        r#match: NetworkMatch { gateway_mac: None, ssid: None, cidr: None },
                        tags: vec![],
                    env: Default::default(),
    },
                    NetworkScope {
                        id, // Duplicate ID
                        r#match: NetworkMatch { gateway_mac: None, ssid: None, cidr: None },
                        tags: vec![],
                    env: Default::default(),
    },
                ];

                let config = Config {
                    cache: Cache::default(),
                    capabilities: Default::default(),
                    native: Default::default(),
                    scope: Scopes { network, host: vec![], user: vec![] },
                    bundle: vec![],
                    mcp: vec![],
                    features: None,
                    marketplace: vec![],
                    plugin_collection: vec![],
                    state: Default::default(),
                    host: Default::default(),
                    init: Default::default(),
                };
                prop_assert!(
                    config.validate().is_err(),
                    "config with duplicate scope IDs should fail validation"
                );
            }

            #[test]
            fn prop_config_validate_enforces_bundle_tags(
                names in prop::collection::vec(arb_string(), 1..3)
            ) {
                let mut bundles = names.iter()
                    .map(|name| Bundle { name: name.clone(), tags: vec!["tag1".to_string()], env: Default::default() })
                    .collect::<Vec<_>>();
                if !bundles.is_empty() {
                    bundles[0].tags.clear();
                }
                let config = Config {
                    cache: Cache::default(),
                    capabilities: Default::default(),
                    native: Default::default(),
                    scope: Scopes::default(),
                    bundle: bundles,
                    mcp: vec![],
                    features: None,
                    marketplace: vec![],
                    plugin_collection: vec![],
                    state: Default::default(),
                    host: Default::default(),
                    init: Default::default(),
                };
                prop_assert!(
                    config.validate().is_err(),
                    "config with empty bundle tags should fail validation"
                );
            }

            #[test]
            fn prop_config_validate_enforces_unique_bundle_names(
                name in arb_string(),
            ) {
                let config = Config {
                    cache: Cache::default(),
                    capabilities: Default::default(),
                    native: Default::default(),
                    scope: Scopes::default(),
                    bundle: vec![
                        Bundle { name: name.clone(), tags: vec!["tag1".to_string()], env: Default::default() },
                        Bundle { name, tags: vec!["tag2".to_string()], env: Default::default() },
                    ],
                    mcp: vec![],
                    features: None,
                    marketplace: vec![],
                    plugin_collection: vec![],
                    state: Default::default(),
                    host: Default::default(),
                    init: Default::default(),
                };
                prop_assert!(
                    config.validate().is_err(),
                    "config with duplicate bundle names should fail validation"
                );
            }
        }

    #[test]
    fn test_valid_config_passes_validation() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: Some("aa:bb:cc:dd:ee:ff".to_string()),
                        ssid: None,
                        cidr: None,
                    },
                    tags: vec![],
                    env: Default::default(),
                }],
                host: vec![],
                user: vec![],
            },
            bundle: vec![Bundle {
                name: "test-bundle".to_string(),
                tags: vec!["prod".to_string()],
                env: Default::default(),
            }],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_cidr_prefix_too_large() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: None,
                        ssid: None,
                        cidr: Some("192.168.1.0/33".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                host: vec![],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_cidr_malformed() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: None,
                        ssid: None,
                        cidr: Some("256.256.256.256/24".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                host: vec![],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_mcp_named_icm_is_rejected() {
        // A user MCP named "icm" collides with the memory backend's reserved
        // registration name; rendering both would silently drop one entry.
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![crate::config::McpServer {
                name: crate::mcp::resolve::MEMORY_MCP_NAME.to_string(),
                tags: vec!["tag1".to_string()],
                transport: crate::config::McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                env: Default::default(),
                url: None,
            }],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(matches!(
            config.validate(),
            Err(ValidateError::McpReservedName(_))
        ));
    }

    #[test]
    fn test_invalid_mac_incomplete() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: Some("aa:bb:cc:dd:ee".to_string()),
                        ssid: None,
                        cidr: None,
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                host: vec![],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_mac_invalid_hex() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: Some("zz:bb:cc:dd:ee:ff".to_string()),
                        ssid: None,
                        cidr: None,
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                host: vec![],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_starts_with_hyphen() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("-invalid.local".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_ends_with_hyphen() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("invalid-".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_double_dot() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("invalid..local".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_too_long() {
        // RFC 1123: a single label may not exceed 63 octets.
        let long_label = "a".repeat(64);
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some(format!("{long_label}.example.com")),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_cidr_leading_zero_octet() {
        // Dotted-decimal forbids leading zeros ("01") even though they parse.
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![NetworkScope {
                    id: "net1".to_string(),
                    r#match: NetworkMatch {
                        gateway_mac: None,
                        ssid: None,
                        cidr: Some("01.168.1.0/24".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                host: vec![],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_ends_with_hyphen() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("foo-.example.com".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_starts_with_hyphen() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes {
                network: vec![],
                host: vec![HostScope {
                    id: "host1".to_string(),
                    r#match: HostMatch {
                        hostname: Some("foo.-example.com".to_string()),
                    },
                    tags: vec!["tag1".to_string()],
                    env: Default::default(),
                }],
                user: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_var_name_starts_with_digit() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("123var".to_string(), "value".to_string());
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![Bundle {
                name: "test".to_string(),
                tags: vec!["tag1".to_string()],
                env,
            }],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_var_name_contains_hyphen() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("my-var".to_string(), "value".to_string());
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![Bundle {
                name: "test".to_string(),
                tags: vec!["tag1".to_string()],
                env,
            }],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_valid_var_names() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("MY_VAR".to_string(), "value1".to_string());
        env.insert("_private".to_string(), "value2".to_string());
        env.insert("var123".to_string(), "value3".to_string());
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![Bundle {
                name: "test".to_string(),
                tags: vec!["tag1".to_string()],
                env,
            }],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_dir_with_traversal() {
        let config = Config {
            cache: Cache {
                cache_dir: "~/.cache/../../../etc/passwd".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(168),
                ..Default::default()
            },
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_trailing_parent_no_slash() {
        // `foo/..` has no "../" or "/.." substring on the right side but is a
        // real traversal — semantic parsing (#65) must reject it.
        let config = Config {
            cache: Cache {
                cache_dir: "~/.cache/llmenv/..".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(168),
                ..Default::default()
            },
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_with_null_byte() {
        let config = Config {
            cache: Cache {
                cache_dir: "~/.cache/llm\0env".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(168),
                ..Default::default()
            },
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_valid() {
        let config = Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_retention_zero() {
        let config = Config {
            cache: Cache {
                cache_dir: "~/.cache/llmenv".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(0),
                ..Default::default()
            },
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_retention_valid() {
        let config = Config {
            cache: Cache {
                cache_dir: "~/.cache/llmenv".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: Some(168),
                ..Default::default()
            },
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_retention_none() {
        let config = Config {
            cache: Cache {
                cache_dir: "~/.cache/llmenv".to_string(),
                sync_interval_minutes: 15,
                cache_retention_hours: None,
                ..Default::default()
            },
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        };
        assert!(config.validate().is_ok());
    }

    fn config_with_marketplace(name: &str, source: &str) -> Config {
        Config {
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![Marketplace {
                name: name.to_string(),
                source: source.to_string(),
            }],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
        }
    }

    fn config_with_state(tools: Vec<crate::config::StateTool>) -> Config {
        Config {
            state: crate::config::StateConfig { tools },
            ..Config::default()
        }
    }

    fn state_tool(env: &str, subdir: &str) -> crate::config::StateTool {
        crate::config::StateTool {
            env: env.into(),
            subdir: subdir.into(),
        }
    }

    #[test]
    fn state_tool_with_valid_env_and_subdir_accepted() {
        let cfg = config_with_state(vec![state_tool("CONTEXT_MODE_DATA_DIR", "context-mode")]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn state_tool_with_invalid_env_name_rejected() {
        // Leading-digit / space / dash / empty break the shell export contract.
        // (Lowercase is a valid POSIX env name, so it is intentionally accepted.)
        for bad in ["1LEADING", "HAS SPACE", "HAS-DASH", ""] {
            let cfg = config_with_state(vec![state_tool(bad, "ok")]);
            assert!(
                matches!(cfg.validate(), Err(ValidateError::StateInvalidEnvName(_))),
                "env '{bad}' should be rejected"
            );
        }
    }

    #[test]
    fn state_tool_with_unsafe_subdir_rejected() {
        // Subdir must be a single safe path component — block traversal,
        // separators, drive-letter colons, and NUL so a tool's state can't be
        // relocated outside the durable dir (#175).
        for bad in [
            "..",
            ".",
            "",
            "a/b",
            "../escape",
            "/abs",
            "a\\b",
            "C:",
            "a:b",
            "a\0b",
        ] {
            let cfg = config_with_state(vec![state_tool("OK_DIR", bad)]);
            assert!(
                matches!(cfg.validate(), Err(ValidateError::StateInvalidSubdir(_))),
                "subdir '{bad}' should be rejected"
            );
        }
    }

    #[test]
    fn state_duplicate_env_var_rejected() {
        let cfg = config_with_state(vec![
            state_tool("DATA_DIR", "a"),
            state_tool("DATA_DIR", "b"),
        ]);
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::StateDuplicateEnv(_))
        ));
    }

    #[test]
    fn state_reserved_env_vars_rejected() {
        // llmenv (and the Claude Code adapter) emit these into the same env_vars
        // set a tool's relocation var lands in; claiming one would emit a
        // conflicting binding (e.g. redirecting CLAUDE_CONFIG_DIR), so each is
        // rejected up front (#175).
        for reserved in crate::materialize::state::RESERVED_STATE_ENV_VARS {
            let cfg = config_with_state(vec![state_tool(reserved, "x")]);
            assert!(
                matches!(cfg.validate(), Err(ValidateError::StateDuplicateEnv(_))),
                "reserved env '{reserved}' should be rejected"
            );
        }
    }

    #[test]
    fn test_marketplace_name_path_traversal_rejected() {
        let config = config_with_marketplace("../../etc", "https://example.com/m");
        assert!(matches!(
            config.validate(),
            Err(ValidateError::InvalidMarketplaceName(_))
        ));
    }

    #[test]
    fn reserved_marketplace_name_with_non_anthropics_source_rejected() {
        // claude-plugins-official sourced from a non-anthropics repo is exactly
        // what Claude Code rejects at load time; catch it at config validation
        // with an actionable error instead (#190).
        let config = config_with_marketplace(
            "claude-plugins-official",
            "https://github.com/someone-else/plugins",
        );
        assert!(matches!(
            config.validate(),
            Err(ValidateError::ReservedMarketplaceSource { .. })
        ));
    }

    #[test]
    fn reserved_marketplace_name_with_non_github_source_rejected() {
        let config = config_with_marketplace("anthropic-plugins", "/local/clone");
        assert!(matches!(
            config.validate(),
            Err(ValidateError::ReservedMarketplaceSource { .. })
        ));
    }

    #[test]
    fn reserved_marketplace_name_with_anthropics_source_accepted() {
        let config = config_with_marketplace(
            "claude-plugins-official",
            "https://github.com/anthropics/claude-code",
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn non_reserved_marketplace_keeps_arbitrary_source() {
        // A normal marketplace name carries no source constraint.
        let config =
            config_with_marketplace("my-plugins", "https://github.com/someone-else/plugins");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_marketplace_name_dotdot_rejected() {
        let config = config_with_marketplace("..", "https://example.com/m");
        assert!(matches!(
            config.validate(),
            Err(ValidateError::InvalidMarketplaceName(_))
        ));
    }

    #[test]
    fn test_marketplace_name_leading_dash_rejected() {
        // A leading '-' would be parsed as a flag by git when cloning.
        let config = config_with_marketplace("-rf", "https://example.com/m");
        assert!(matches!(
            config.validate(),
            Err(ValidateError::InvalidMarketplaceName(_))
        ));
    }

    #[test]
    fn test_marketplace_name_at_sign_rejected() {
        // '@' would make the rendered `plugin@marketplace` key ambiguous.
        let config = config_with_marketplace("foo@bar", "https://example.com/m");
        assert!(matches!(
            config.validate(),
            Err(ValidateError::InvalidMarketplaceName(_))
        ));
    }

    #[test]
    fn test_marketplace_name_slash_rejected() {
        // '/' would escape the single cache path component.
        let config = config_with_marketplace("foo/bar", "https://example.com/m");
        assert!(matches!(
            config.validate(),
            Err(ValidateError::InvalidMarketplaceName(_))
        ));
    }

    #[test]
    fn test_marketplace_name_valid_accepted() {
        let config = config_with_marketplace("super-powers_2.0", "https://example.com/m");
        assert!(config.validate().is_ok());
    }

    // ===== Property tests against the real validators =====
    //
    // These call the private is_valid_* functions directly (rather than
    // re-implementing the rules in an external integration test) so a change to
    // the validators is caught here instead of silently diverging from a copy.

    // RFC 1123 hostname: 1..=63-octet labels, alnum + interior hyphens, total <= 253.
    fn rfc1123_label() -> impl Strategy<Value = String> {
        prop::string::string_regex("[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?")
            .expect("valid label regex")
    }

    fn valid_hostname() -> impl Strategy<Value = String> {
        prop::collection::vec(rfc1123_label(), 1..4)
            .prop_map(|labels| labels.join("."))
            .prop_filter("total length <= 253", |h| h.len() <= 253)
    }

    fn valid_cidr() -> impl Strategy<Value = String> {
        (0u8..=255, 0u8..=255, 0u8..=255, 0u8..=255, 0u8..=32)
            .prop_map(|(a, b, c, d, m)| format!("{a}.{b}.{c}.{d}/{m}"))
    }

    fn valid_var_name() -> impl Strategy<Value = String> {
        prop::string::string_regex("[A-Za-z_][A-Za-z0-9_]*").expect("valid var name regex")
    }

    proptest! {
        #[test]
        fn prop_valid_hostnames_accepted(h in valid_hostname()) {
            prop_assert!(is_valid_hostname(&h), "RFC 1123 hostname rejected: {h:?}");
        }

        #[test]
        fn prop_label_over_63_octets_rejected(
            prefix in rfc1123_label(),
            extra in 0usize..40,
        ) {
            // Build a single label of 64..=63+40 octets; must be rejected even
            // though it is otherwise alphanumeric.
            let label = "a".repeat(64 + extra);
            prop_assert!(!is_valid_hostname(&label), "64+ octet label accepted");
            // The valid prefix alone must still pass, proving it's the length
            // that's rejected, not the characters.
            prop_assert!(is_valid_hostname(&prefix));
        }

        #[test]
        fn prop_hostname_with_underscore_rejected(
            a in rfc1123_label(),
            b in rfc1123_label(),
        ) {
            let h = format!("{a}_{b}");
            prop_assert!(!is_valid_hostname(&h), "underscore accepted in hostname: {h:?}");
        }

        #[test]
        fn prop_valid_cidrs_accepted(c in valid_cidr()) {
            prop_assert!(is_valid_cidr(&c), "valid CIDR rejected: {c}");
        }

        #[test]
        fn prop_cidr_prefix_over_32_rejected(
            a in 0u8..=255, b in 0u8..=255, c in 0u8..=255, d in 0u8..=255,
            m in 33u16..=255,
        ) {
            let cidr = format!("{a}.{b}.{c}.{d}/{m}");
            prop_assert!(!is_valid_cidr(&cidr), "prefix >32 accepted: {cidr}");
        }

        #[test]
        fn prop_cidr_leading_zero_octet_rejected(
            b in 0u8..=255, c in 0u8..=255, d in 0u8..=255, m in 0u8..=32,
        ) {
            // "01" is forbidden dotted-decimal even though u8 parse accepts it.
            let cidr = format!("01.{b}.{c}.{d}/{m}");
            prop_assert!(!is_valid_cidr(&cidr), "leading-zero octet accepted: {cidr}");
        }

        #[test]
        fn prop_valid_var_names_accepted(name in valid_var_name()) {
            prop_assert!(is_valid_var_name(&name), "valid var name rejected: {name}");
        }

        #[test]
        fn prop_var_name_leading_digit_rejected(
            d in 0u8..=9,
            rest in "[A-Za-z0-9_]{0,10}",
        ) {
            let name = format!("{d}{rest}");
            prop_assert!(!is_valid_var_name(&name), "leading-digit var name accepted: {name}");
        }

        #[test]
        fn prop_valid_mac_addresses_accepted(octets in prop::array::uniform6(0u8..=255)) {
            let mac = octets
                .iter()
                .map(|o| format!("{o:02x}"))
                .collect::<Vec<_>>()
                .join(":");
            prop_assert!(is_valid_mac_address(&mac), "valid MAC rejected: {mac}");
        }

        #[test]
        fn prop_mac_wrong_group_count_rejected(count in prop_oneof![0usize..6, 7usize..12]) {
            let mac = vec!["aa"; count].join(":");
            prop_assert!(!is_valid_mac_address(&mac), "MAC with {count} groups accepted");
        }

        #[test]
        fn prop_mac_non_hex_rejected(
            pos in 0usize..6,
            bad in "[g-zG-Z]{2}",
        ) {
            let mut octets = vec!["aa".to_string(); 6];
            octets[pos] = bad;
            let mac = octets.join(":");
            prop_assert!(!is_valid_mac_address(&mac), "non-hex MAC accepted: {mac}");
        }

        #[test]
        fn prop_cache_dir_with_parent_component_rejected(
            before in "[a-z0-9_-]{1,10}",
            after in "[a-z0-9_-]{1,10}",
        ) {
            let dir = format!("{before}/../{after}");
            prop_assert!(!is_safe_cache_dir(&dir), "parent component accepted: {dir}");
        }

        #[test]
        fn prop_cache_dir_with_null_byte_rejected(
            before in "[a-z0-9/_-]{0,20}",
            after in "[a-z0-9/_-]{0,20}",
        ) {
            let dir = format!("{before}\0{after}");
            prop_assert!(!is_safe_cache_dir(&dir), "null byte accepted in cache dir");
        }

        #[test]
        fn prop_cache_dir_over_max_length_rejected(len in 4097usize..5000) {
            let dir = "a".repeat(len);
            prop_assert!(!is_safe_cache_dir(&dir), "over-length cache dir accepted");
        }

        #[test]
        fn prop_valid_marketplace_names_accepted(
            name in "[A-Za-z0-9._][A-Za-z0-9._-]{0,30}",
        ) {
            // First char is not '-', and the whole thing is the allowed charset,
            // so it must be accepted (the "." / ".." cases are excluded by the
            // leading-char class never producing a bare "." or "..").
            prop_assume!(name != "." && name != "..");
            prop_assert!(is_valid_marketplace_name(&name), "valid name rejected: {name}");
        }

        #[test]
        fn prop_marketplace_name_with_disallowed_char_rejected(
            before in "[A-Za-z0-9._-]{0,10}",
            bad in "[@/:\\\\ ]",
            after in "[A-Za-z0-9._-]{0,10}",
        ) {
            let name = format!("{before}{bad}{after}");
            prop_assert!(
                !is_valid_marketplace_name(&name),
                "name with disallowed char accepted: {name:?}"
            );
        }

        #[test]
        fn prop_marketplace_name_leading_dash_rejected(rest in "[A-Za-z0-9._-]{0,20}") {
            let name = format!("-{rest}");
            prop_assert!(!is_valid_marketplace_name(&name), "leading-dash name accepted: {name}");
        }

        #[test]
        fn prop_safe_subdir_accepts_single_clean_component(
            subdir in "[A-Za-z0-9._-]{1,20}",
        ) {
            // A single component free of separators/colon/NUL is accepted, except
            // the traversal sentinels "." and "..".
            prop_assume!(subdir != "." && subdir != "..");
            prop_assert!(is_safe_state_subdir(&subdir), "clean subdir rejected: {subdir}");
        }

        #[test]
        fn prop_safe_subdir_rejects_separators_and_special(
            before in "[A-Za-z0-9._-]{0,10}",
            bad in prop_oneof![Just('/'), Just('\\'), Just(':'), Just('\0')],
            after in "[A-Za-z0-9._-]{0,10}",
        ) {
            // Any path separator, drive-letter colon, or NUL anywhere in the
            // component is rejected so state can't escape the durable dir (#175).
            let subdir = format!("{before}{bad}{after}");
            prop_assert!(!is_safe_state_subdir(&subdir), "unsafe subdir accepted: {subdir:?}");
        }

        #[test]
        fn prop_safe_subdir_never_panics(subdir in ".{0,40}") {
            let _ = is_safe_state_subdir(&subdir);
        }

        // #354: any key that starts with LLMENV_ is rejected.
        #[test]
        fn prop_llmenv_prefix_env_key_rejected(suffix in "[A-Z0-9_]{1,16}") {
            let key = format!("LLMENV_{suffix}");
            prop_assert!(
                validate_capabilities_env_key("test", &key).is_err(),
                "LLMENV_-prefixed key should be rejected: {key}"
            );
        }

        // #354: keys that are not reserved and not LLMENV_-prefixed are accepted.
        #[test]
        fn prop_normal_env_key_accepted(key in "[A-Za-z_][A-Za-z0-9_]{0,15}") {
            let reserved: &[&str] = crate::materialize::state::RESERVED_STATE_ENV_VARS;
            prop_assume!(!reserved.contains(&key.as_str()));
            prop_assume!(!key.starts_with("LLMENV_"));
            prop_assert!(
                validate_capabilities_env_key("test", &key).is_ok(),
                "valid env key should be accepted: {key}"
            );
        }
    }

    // ===== #354: capabilities.env reserved key validation =====

    fn config_with_capabilities_env(key: &str, value: &str) -> crate::config::Config {
        use std::collections::BTreeMap;
        crate::config::Config {
            capabilities: Capabilities {
                env: BTreeMap::from([(key.to_string(), value.to_string())]),
                ..Default::default()
            },
            ..minimal_config()
        }
    }

    fn minimal_config() -> crate::config::Config {
        crate::config::Config {
            bundle: vec![Bundle {
                name: "b".into(),
                tags: vec!["t".into()],
                vars: Default::default(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn capabilities_env_claude_config_dir_rejected() {
        let cfg = config_with_capabilities_env("CLAUDE_CONFIG_DIR", "/some/path");
        assert!(
            matches!(
                cfg.validate(),
                Err(ValidateError::CapabilitiesReservedEnvKey { .. })
            ),
            "CLAUDE_CONFIG_DIR must be rejected in capabilities.env"
        );
    }

    #[test]
    fn capabilities_env_llmenv_state_dir_rejected() {
        let cfg = config_with_capabilities_env("LLMENV_STATE_DIR", "/some/path");
        assert!(
            matches!(
                cfg.validate(),
                Err(ValidateError::CapabilitiesReservedEnvKey { .. })
            ),
            "LLMENV_STATE_DIR must be rejected in capabilities.env"
        );
    }

    #[test]
    fn capabilities_env_llmenv_prefix_rejected() {
        let cfg = config_with_capabilities_env("LLMENV_CUSTOM", "value");
        assert!(
            matches!(
                cfg.validate(),
                Err(ValidateError::CapabilitiesLlmenvPrefixEnvKey { .. })
            ),
            "LLMENV_* prefix must be rejected in capabilities.env"
        );
    }

    #[test]
    fn capabilities_env_llmenv_prefix_variant_rejected() {
        let cfg = config_with_capabilities_env("LLMENV_ANYTHING_AT_ALL", "x");
        assert!(
            matches!(
                cfg.validate(),
                Err(ValidateError::CapabilitiesLlmenvPrefixEnvKey { .. })
            ),
            "any LLMENV_* key must be rejected"
        );
    }

    #[test]
    fn capabilities_env_valid_key_accepted() {
        let cfg = config_with_capabilities_env("MY_APP_TOKEN", "secret");
        assert!(
            cfg.validate().is_ok(),
            "valid capabilities.env key must be accepted"
        );
    }

    #[test]
    fn capabilities_env_underscore_prefixed_valid_key_accepted() {
        let cfg = config_with_capabilities_env("_MY_VAR", "val");
        assert!(
            cfg.validate().is_ok(),
            "_-prefixed non-reserved key must be accepted"
        );
    }

    #[test]
    fn capabilities_env_error_message_contains_key_name() {
        let cfg = config_with_capabilities_env("CLAUDE_CONFIG_DIR", "/x");
        let msg = cfg.validate().unwrap_err().to_string();
        assert!(
            msg.contains("CLAUDE_CONFIG_DIR"),
            "error message must name the offending key; got: {msg}"
        );
    }

    #[test]
    fn capabilities_env_all_reserved_state_vars_rejected() {
        for reserved in crate::materialize::state::RESERVED_STATE_ENV_VARS {
            let cfg = config_with_capabilities_env(reserved, "x");
            assert!(
                matches!(
                    cfg.validate(),
                    Err(ValidateError::CapabilitiesReservedEnvKey { .. })
                ),
                "reserved env var '{reserved}' must be rejected in capabilities.env"
            );
        }
    }
}
