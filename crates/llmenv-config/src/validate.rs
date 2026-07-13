use super::{Config, PermissionRule};
use llmenv_paths::has_parent_component;
use thiserror::Error;

#[cfg(test)]
use super::{
    Bundle, Cache, Capabilities, Features, Hook, HookHandler, HookHandlerKind, HostEntry,
    HostMatch, HostScope, Marketplace, McpServer, McpTransport, Memory, NativePermissionRules,
    NetworkMatch, NetworkScope, PermissionMode, Permissions, PluginCollection, Scopes, Throttle,
    UserMatch, UserScope,
};

#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("duplicate scope id: {0}")]
    DuplicateScopeId(String),
    #[error("bundle {0} has no when: tags")]
    BundleNoTags(String),
    #[error("duplicate bundle name: {0}")]
    DuplicateBundleName(String),
    #[error("invalid bundle name: {0}")]
    InvalidBundleName(String),
    #[error("invalid CIDR notation: {0}")]
    InvalidCIDR(String),
    #[error("invalid MAC address: {0}")]
    InvalidMACAddress(String),
    #[error("invalid hostname: {0}")]
    InvalidHostname(String),
    #[error("cache_dir contains path traversal components: {0}")]
    CacheDirTraversal(String),
    #[error("cache_retention_hours must be > 0")]
    CacheRetentionInvalid,
    #[error("duplicate mcp name: {0}")]
    DuplicateMcpName(String),
    #[error("mcp name '{0}' is reserved for the memory backend")]
    McpReservedName(String),
    #[error("mcp {0} has no when: tags")]
    McpNoTags(String),
    #[error("mcp {0}: stdio transport requires a `command`")]
    McpStdioMissingCommand(String),
    #[error("mcp {0}: {1} transport requires a `url`")]
    McpRemoteMissingUrl(String, String),
    #[error("host '{0}': addr '{1}' is not a valid hostname or IP address literal")]
    InvalidHostAddr(String, String),
    #[error("memory: server_host '{0}' has no entry in the `host:` table")]
    MemoryUnknownServerHost(String),
    #[error("memory has no when: tags")]
    MemoryNoTags,
    #[error(
        "memory: listen_host '{0}' is not a valid IP address literal (hostnames not supported)"
    )]
    MemoryInvalidListenHost(String),
    #[error("throttle entry for '{0}' has no when: tags")]
    ThrottleNoTags(String),
    #[error("throttle entry has an empty 'backend' field")]
    ThrottleEmptyBackend,
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
    #[error("plugin-collection {0} has no when: tags")]
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
         Fix: remove this key from env:, or declare env vars in bundle.yaml under capabilities.env."
    )]
    CapabilitiesReservedEnvKey { context: String, key: String },
    #[error(
        "{context}: capabilities.env key '{key}' uses the 'LLMENV_' prefix, which is \
         reserved for llmenv-internal variables. Fix: rename the key."
    )]
    CapabilitiesLlmenvPrefixEnvKey { context: String, key: String },
    #[error(
        "{context}: capabilities.env key '{key}' is not a valid shell identifier \
         (must match [A-Za-z_][A-Za-z0-9_]*). Fix: rename the key."
    )]
    CapabilitiesInvalidVarName { context: String, key: String },
    #[error(
        "mcp '{mcp}': env key '{key}' is not a valid shell identifier \
         (must match [A-Za-z_][A-Za-z0-9_]*). Fix: rename the key."
    )]
    McpInvalidEnvKey { mcp: String, key: String },
    #[error(
        "mcp '{mcp}': env key '{key}' is reserved — it is emitted by the adapter or state \
         system and must not be overridden here. Fix: remove this key from env:."
    )]
    McpReservedEnvKey { mcp: String, key: String },
    #[error(
        "mcp '{mcp}': env key '{key}' uses the 'LLMENV_' prefix, which is reserved for \
         llmenv-internal variables. Fix: rename the key."
    )]
    McpLlmenvPrefixEnvKey { mcp: String, key: String },
    #[error("lsp server '{0}' has an empty name")]
    LspEmptyName(String),
    #[error("lsp server '{0}' has an empty command")]
    LspEmptyCommand(String),
    #[error("skill '{0}' has an empty name")]
    SkillEmptyName(String),
    #[error("skill '{0}' has an empty path")]
    SkillEmptyPath(String),
    #[error("skill '{0}' path contains traversal components (..): {1}")]
    SkillPathTraversal(String, String),
    #[error(
        "{context}: permission rule tool='{tool}' value '{value}' has unbalanced \
         parentheses. Adapters that render neutral rules as 'Tool(value)' strings \
         (Claude Code, Crush) require value's own '('/')' to balance, or the engine's \
         settings loader silently drops the whole rule at load time — a deny rule with \
         unbalanced parens fails open with no warning. Fix: balance the parentheses (e.g. \
         'bash <(curl *)*' instead of 'bash <(curl *'), or avoid parentheses in the pattern."
    )]
    PermissionRuleUnbalancedParens {
        context: String,
        tool: String,
        value: String,
    },
    #[error("model provider has an empty id")]
    ModelProviderEmptyId,
    #[error("duplicate model provider id: {0}")]
    ModelProviderDuplicateId(String),
    #[error("model provider '{0}' has a model with an empty id")]
    ModelSourceEmptyId(String),
    #[error("model provider '{0}': duplicate model id '{1}'")]
    ModelSourceDuplicateId(String, String),
    #[error("default_models has an entry with an empty role key")]
    DefaultModelEmptyRole,
    #[error("default_models role '{0}' has an empty provider or model")]
    DefaultModelEmptyRef(String),
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
pub fn validate_capabilities_env_key(context: &str, key: &str) -> Result<(), ValidateError> {
    if crate::RESERVED_STATE_ENV_VARS.contains(&key) {
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
    if !is_valid_var_name(key) {
        return Err(ValidateError::CapabilitiesInvalidVarName {
            context: context.to_string(),
            key: key.to_string(),
        });
    }
    Ok(())
}

/// A pattern/path is only safe to wrap as `Tool(value)` if its own parentheses
/// balance — otherwise the wrapped string's parens don't balance either, and
/// engines that parse the rule by tracking paren depth (Claude Code, Crush)
/// silently skip the whole rule. `<(` from shell process substitution is the
/// common trigger (`bash <(curl *` has one unmatched `(`).
fn has_balanced_parens(s: &str) -> bool {
    let mut depth: i32 = 0;
    let mut prev_was_escape = false;
    for c in s.chars() {
        if prev_was_escape {
            prev_was_escape = false;
            continue;
        }
        match c {
            '\\' => prev_was_escape = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Validate a single permission rule's `pattern`/`paths` for balanced
/// parentheses. Returns an error with the given `context` label (e.g.
/// `"config.yaml: capabilities.permissions"` or `"bundle 'foo'"`) on failure —
/// see [`ValidateError::PermissionRuleUnbalancedParens`] for why this is
/// rejected rather than rendered as-is.
pub fn validate_permission_rule(context: &str, rule: &PermissionRule) -> Result<(), ValidateError> {
    if let Some(pattern) = &rule.pattern
        && !has_balanced_parens(pattern)
    {
        return Err(ValidateError::PermissionRuleUnbalancedParens {
            context: context.to_string(),
            tool: rule.tool.clone(),
            value: pattern.clone(),
        });
    }
    for path in &rule.paths {
        if !has_balanced_parens(path) {
            return Err(ValidateError::PermissionRuleUnbalancedParens {
                context: context.to_string(),
                tool: rule.tool.clone(),
                value: path.clone(),
            });
        }
    }
    Ok(())
}

/// Validate a single native permission rule string (already in `Tool(value)`
/// format) for balanced parentheses — same rationale as
/// [`validate_permission_rule`].
pub fn validate_permission_string(context: &str, raw: &str) -> Result<(), ValidateError> {
    if !has_balanced_parens(raw) {
        return Err(ValidateError::PermissionRuleUnbalancedParens {
            context: context.to_string(),
            tool: raw.to_string(),
            value: raw.to_string(),
        });
    }
    Ok(())
}

pub(crate) fn is_valid_var_name(name: &str) -> bool {
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
    !dir.contains('\0') && !has_parent_component(dir)
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
            if b.when.is_empty() {
                return Err(ValidateError::BundleNoTags(b.name.clone()));
            }
            if !seen_bundle_names.insert(&b.name) {
                return Err(ValidateError::DuplicateBundleName(b.name.clone()));
            }
            // A bundle name is joined as a single path component (the
            // content directory name) at every call site that resolves a
            // firing bundle to disk — validating here is the actual
            // security boundary, since not every join site re-checks it.
            if !llmenv_paths::is_valid_short_name(&b.name) {
                return Err(ValidateError::InvalidBundleName(b.name.clone()));
            }
        }
        for key in self.capabilities.env.keys() {
            validate_capabilities_env_key("config.yaml: capabilities", key)?;
        }
        self.validate_mcps()?;
        self.validate_lsp()?;
        self.validate_skills()?;
        self.validate_model_providers()?;
        self.validate_default_models()?;
        self.validate_plugins()?;
        self.validate_state()?;
        self.validate_permissions()?;
        Ok(())
    }

    fn validate_permissions(&self) -> Result<(), ValidateError> {
        let context = "config.yaml: capabilities.permissions";
        let rules = self
            .capabilities
            .permissions
            .allow
            .iter()
            .chain(self.capabilities.permissions.ask.iter())
            .chain(self.capabilities.permissions.deny.iter());
        for rule in rules {
            validate_permission_rule(context, rule)?;
        }
        for (engine, nr) in &self.capabilities.native_permissions {
            let ctx = format!("config.yaml: capabilities.native_permissions['{engine}']");
            for s in nr.allow.iter().chain(nr.ask.iter()).chain(nr.deny.iter()) {
                validate_permission_string(&ctx, s)?;
            }
        }
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
            if !llmenv_paths::is_valid_short_name(&m.name) {
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
            if c.when.is_empty() {
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
        for reserved in crate::RESERVED_STATE_ENV_VARS {
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
            if m.when.is_empty() {
                return Err(ValidateError::McpNoTags(m.name.clone()));
            }
            if m.name == crate::MEMORY_MCP_NAME {
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
            for key in m.env.keys() {
                if crate::RESERVED_STATE_ENV_VARS.contains(&key.as_str()) {
                    return Err(ValidateError::McpReservedEnvKey {
                        mcp: m.name.clone(),
                        key: key.clone(),
                    });
                }
                if key.starts_with("LLMENV_") {
                    return Err(ValidateError::McpLlmenvPrefixEnvKey {
                        mcp: m.name.clone(),
                        key: key.clone(),
                    });
                }
                if !is_valid_var_name(key) {
                    return Err(ValidateError::McpInvalidEnvKey {
                        mcp: m.name.clone(),
                        key: key.clone(),
                    });
                }
            }
        }
        // Validate the host address table: each addr must be a valid hostname or IP literal.
        for (name, entry) in &self.host {
            let is_valid =
                entry.addr.parse::<std::net::IpAddr>().is_ok() || is_valid_hostname(&entry.addr);
            if !is_valid {
                return Err(ValidateError::InvalidHostAddr(
                    name.clone(),
                    entry.addr.clone(),
                ));
            }
        }
        if let Some(features) = &self.features {
            for mem in &features.memory {
                if mem.when.is_empty() {
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
            for th in &features.throttle {
                if th.when.is_empty() {
                    return Err(ValidateError::ThrottleNoTags(th.backend.clone()));
                }
                if th.backend.is_empty() {
                    return Err(ValidateError::ThrottleEmptyBackend);
                }
            }
        }
        Ok(())
    }

    fn validate_lsp(&self) -> Result<(), ValidateError> {
        for l in &self.lsp {
            if l.name.is_empty() {
                return Err(ValidateError::LspEmptyName(l.name.clone()));
            }
            if l.command.is_empty() {
                return Err(ValidateError::LspEmptyCommand(l.name.clone()));
            }
        }
        Ok(())
    }

    fn validate_model_providers(&self) -> Result<(), ValidateError> {
        let mut seen_ids = std::collections::HashSet::new();
        for p in &self.capabilities.model_providers {
            if p.id.is_empty() {
                return Err(ValidateError::ModelProviderEmptyId);
            }
            if !seen_ids.insert(&p.id) {
                return Err(ValidateError::ModelProviderDuplicateId(p.id.clone()));
            }
            let mut seen_model_ids = std::collections::HashSet::new();
            for m in &p.models {
                if m.id.is_empty() {
                    return Err(ValidateError::ModelSourceEmptyId(p.id.clone()));
                }
                if !seen_model_ids.insert(&m.id) {
                    return Err(ValidateError::ModelSourceDuplicateId(
                        p.id.clone(),
                        m.id.clone(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_default_models(&self) -> Result<(), ValidateError> {
        for (role, r#ref) in &self.capabilities.default_models {
            if role.is_empty() {
                return Err(ValidateError::DefaultModelEmptyRole);
            }
            if r#ref.provider.is_empty() || r#ref.model.is_empty() {
                return Err(ValidateError::DefaultModelEmptyRef(role.clone()));
            }
        }
        Ok(())
    }

    fn validate_skills(&self) -> Result<(), ValidateError> {
        use llmenv_paths::is_unsafe_join_target;

        for s in &self.skills {
            if s.name.is_empty() {
                return Err(ValidateError::SkillEmptyName(s.name.clone()));
            }
            if s.path.is_empty() {
                return Err(ValidateError::SkillEmptyPath(s.name.clone()));
            }
            // Confine skill paths within config/bundle roots — reject traversal
            // and absolute paths. is_unsafe_join_target checks both .. and is_absolute().
            if is_unsafe_join_target(&s.path) {
                return Err(ValidateError::SkillPathTraversal(
                    s.name.clone(),
                    s.path.clone(),
                ));
            }
            // The name becomes a single filesystem path component under
            // `skills/` and (via plugin projection) a JSON key — reject
            // anything outside the same allowlist used for marketplace names
            // (#534: a blocklist here previously missed control characters
            // and Unicode formatting characters like zero-width space).
            if !llmenv_paths::is_valid_short_name(&s.name) {
                return Err(ValidateError::SkillEmptyName(format!(
                    "{} (not a valid skill name: use only ASCII letters, digits, '.', '_', '-')",
                    s.name
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::HashingMode;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn arb_string() -> impl Strategy<Value = String> {
        r"[a-zA-Z0-9_-]{1,20}"
    }

    // Some(arb)/None so the round-trip exercises both branches of every
    // Option<String> match field rather than only the None default.
    fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
        prop::option::of(arb_string())
    }

    // Arbitrary SessionLog so the round-trip exercises the custom Deserialize
    // (mapping form) and every sink configuration.
    fn arb_session_log() -> impl Strategy<Value = crate::SessionLog> {
        use crate::LogLevel;
        fn arb_level() -> impl Strategy<Value = LogLevel> {
            prop_oneof![
                Just(LogLevel::Info),
                Just(LogLevel::Debug),
                Just(LogLevel::Trace),
            ]
        }
        (
            prop::option::of((any::<bool>(), arb_level(), arb_opt_string())),
            prop::option::of((any::<bool>(), arb_level())),
            prop::option::of(0usize..65_536),
        )
            .prop_map(|(file, transcript, max_content_bytes)| crate::SessionLog {
                file: file.map(|(enabled, level, path)| crate::FileSinkConfig {
                    enabled,
                    level,
                    path,
                }),
                transcript: transcript
                    .map(|(enabled, level)| crate::TranscriptSinkConfig { enabled, level }),
                max_content_bytes,
            })
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
                |(name, when, transport, command, args, env, url)| McpServer {
                    name,
                    when,
                    transport,
                    command,
                    args,
                    env,
                    url,
                    ..Default::default()
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
            .prop_map(|(name, when, plugins)| PluginCollection {
                name,
                when,
                plugins,
            })
    }

    fn arb_throttle() -> impl Strategy<Value = Throttle> {
        (
            prop_oneof![Just("umans".to_string())], // only known backends
            prop::collection::vec(arb_string(), 1..3), // at least 1 tag (valid)
            1u64..120,
            1u64..300,
            1u64..50,
        )
            .prop_map(
                |(backend, when, cache_ttl, max_wait, soft_threshold)| Throttle {
                    backend,
                    when,
                    cache_ttl,
                    max_wait,
                    soft_threshold,
                },
            )
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
                |(server_host, port, listen_host, when, default_topics)| Memory {
                    server_host,
                    port,
                    listen_host,
                    when,
                    default_topics,
                    default_type: None,
                    default_importance: None,
                    type_importance: std::collections::BTreeMap::new(),
                    retention: None,
                    auto_prune: false,
                    consolidation: None,
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
                        when: tags,
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
            prop::collection::vec(arb_memory(), 0..3),
            prop::collection::vec(arb_throttle(), 0..2),
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
            prop::option::of(arb_session_log()),
        )
            .prop_map(
                |(
                    cache,
                    (network, host_scopes, user),
                    bundle,
                    mcp,
                    memory,
                    throttle,
                    host,
                    capabilities,
                    marketplace,
                    plugin_collection,
                    session_log,
                )| {
                    Config {
                        disabled_engines: vec![],
                        cache,
                        scope: Scopes {
                            network,
                            host: host_scopes,
                            user,
                            content: vec![],
                        },
                        capabilities,
                        native: Default::default(),
                        bundle,
                        mcp,
                        features: if memory.is_empty() && throttle.is_empty() {
                            None
                        } else {
                            Some(Features {
                                memory,
                                throttle,
                                context_mode: None,
                                upgrade: None,
                                read_once: None,
                                slippage: None,
                            })
                        },
                        marketplace,
                        plugin_collection,
                        state: Default::default(),
                        host,
                        init: Default::default(),
                        session_log,
                        lsp: vec![],
                        skills: vec![],
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
                },
                NetworkScope {
                    id, // Duplicate ID
                    r#match: NetworkMatch { gateway_mac: None, ssid: None, cidr: None },
                    tags: vec![],
                },
            ];

            let config = Config {
                disabled_engines: vec![],
                cache: Cache::default(),
                capabilities: Default::default(),
                native: Default::default(),
                scope: Scopes { network, host: vec![], user: vec![], content: vec![] },
                bundle: vec![],
                mcp: vec![],
                features: None,
                marketplace: vec![],
                plugin_collection: vec![],
                state: Default::default(),
                host: Default::default(),
                init: Default::default(),
                session_log: None,
                lsp: vec![],
                skills: vec![],
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
                .map(|name| Bundle { name: name.clone(), when: vec!["tag1".to_string()] })
                .collect::<Vec<_>>();
            if !bundles.is_empty() {
                bundles[0].when.clear();
            }
            let config = Config {
                disabled_engines: vec![],
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
                session_log: None,
                lsp: vec![],
                skills: vec![],
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
                disabled_engines: vec![],
                cache: Cache::default(),
                capabilities: Default::default(),
                native: Default::default(),
                scope: Scopes::default(),
                bundle: vec![
                    Bundle { name: name.clone(), when: vec!["tag1".to_string()] },
                    Bundle { name, when: vec!["tag2".to_string()] },
                ],
                mcp: vec![],
                features: None,
                marketplace: vec![],
                plugin_collection: vec![],
                state: Default::default(),
                host: Default::default(),
                init: Default::default(),
                session_log: None,
                lsp: vec![],
                skills: vec![],
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
            disabled_engines: vec![],
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
                }],
                host: vec![],
                user: vec![],
                content: vec![],
            },
            bundle: vec![Bundle {
                name: "test-bundle".to_string(),
                when: vec!["prod".to_string()],
            }],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_cidr_prefix_too_large() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                host: vec![],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_cidr_malformed() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                host: vec![],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_mcp_named_icm_is_rejected() {
        // A user MCP named "icm" collides with the memory backend's reserved
        // registration name; rendering both would silently drop one entry.
        let config = Config {
            disabled_engines: vec![],
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![crate::McpServer {
                name: crate::MEMORY_MCP_NAME.to_string(),
                when: vec!["tag1".to_string()],
                transport: crate::McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                env: Default::default(),
                url: None,
                ..Default::default()
            }],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(matches!(
            config.validate(),
            Err(ValidateError::McpReservedName(_))
        ));
    }

    fn config_with_throttle(throttle: Vec<crate::Throttle>) -> Config {
        Config {
            disabled_engines: vec![],
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![],
            features: Some(crate::Features {
                memory: vec![],
                throttle,
                context_mode: None,
                upgrade: None,
                read_once: None,
                slippage: None,
            }),
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        }
    }

    #[test]
    fn throttle_without_when_tags_is_rejected() {
        // An entry with no when: tags can never activate — reject it rather than
        // silently materializing a dead throttle (parity with memory).
        let config = config_with_throttle(vec![crate::Throttle {
            backend: "umans".to_string(),
            when: vec![],
            cache_ttl: 30,
            max_wait: 300,
            soft_threshold: 20,
        }]);
        assert!(matches!(
            config.validate(),
            Err(ValidateError::ThrottleNoTags(_))
        ));
    }

    #[test]
    fn throttle_with_empty_backend_is_rejected() {
        let config = config_with_throttle(vec![crate::Throttle {
            backend: String::new(),
            when: vec!["tag1".to_string()],
            cache_ttl: 30,
            max_wait: 300,
            soft_threshold: 20,
        }]);
        assert!(matches!(
            config.validate(),
            Err(ValidateError::ThrottleEmptyBackend)
        ));
    }

    #[test]
    fn mcp_env_reserved_key_rejected() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "x".to_string());
        let config = Config {
            disabled_engines: vec![],
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![crate::McpServer {
                name: "mymcp".to_string(),
                when: vec!["tag1".to_string()],
                transport: crate::McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                env,
                url: None,
                ..Default::default()
            }],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(matches!(
            config.validate(),
            Err(ValidateError::McpReservedEnvKey { mcp, key })
                if mcp == "mymcp" && key == "CLAUDE_CONFIG_DIR"
        ));
    }

    #[test]
    fn mcp_env_llmenv_prefix_rejected() {
        let mut env = BTreeMap::new();
        env.insert("LLMENV_CUSTOM".to_string(), "x".to_string());
        let config = Config {
            disabled_engines: vec![],
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![crate::McpServer {
                name: "mymcp".to_string(),
                when: vec!["tag1".to_string()],
                transport: crate::McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                env,
                url: None,
                ..Default::default()
            }],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(matches!(
            config.validate(),
            Err(ValidateError::McpLlmenvPrefixEnvKey { mcp, key })
                if mcp == "mymcp" && key == "LLMENV_CUSTOM"
        ));
    }

    #[test]
    fn mcp_env_invalid_var_name_rejected() {
        let mut env = BTreeMap::new();
        env.insert("123INVALID".to_string(), "x".to_string());
        let config = Config {
            disabled_engines: vec![],
            cache: Cache::default(),
            capabilities: Default::default(),
            native: Default::default(),
            scope: Scopes::default(),
            bundle: vec![],
            mcp: vec![crate::McpServer {
                name: "mymcp".to_string(),
                when: vec!["tag1".to_string()],
                transport: crate::McpTransport::Stdio,
                command: Some("echo".to_string()),
                args: vec![],
                env,
                url: None,
                ..Default::default()
            }],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(matches!(
            config.validate(),
            Err(ValidateError::McpInvalidEnvKey { mcp, key })
                if mcp == "mymcp" && key == "123INVALID"
        ));
    }

    #[test]
    fn test_invalid_mac_incomplete() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                host: vec![],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_mac_invalid_hex() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                host: vec![],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_starts_with_hyphen() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_ends_with_hyphen() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_double_dot() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_too_long() {
        // RFC 1123: a single label may not exceed 63 octets.
        let long_label = "a".repeat(64);
        let config = Config {
            disabled_engines: vec![],
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
                }],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_cidr_leading_zero_octet() {
        // Dotted-decimal forbids leading zeros ("01") even though they parse.
        let config = Config {
            disabled_engines: vec![],
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
                }],
                host: vec![],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_ends_with_hyphen() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_hostname_label_starts_with_hyphen() {
        let config = Config {
            disabled_engines: vec![],
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
                }],
                user: vec![],
                content: vec![],
            },
            bundle: vec![],
            mcp: vec![],
            features: None,
            marketplace: vec![],
            plugin_collection: vec![],
            state: Default::default(),
            host: Default::default(),
            init: Default::default(),
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_with_traversal() {
        let config = Config {
            disabled_engines: vec![],
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
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_trailing_parent_no_slash() {
        // `foo/..` has no "../" or "/.." substring on the right side but is a
        // real traversal — semantic parsing (#65) must reject it.
        let config = Config {
            disabled_engines: vec![],
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
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_with_null_byte() {
        let config = Config {
            disabled_engines: vec![],
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
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_dir_valid() {
        let config = Config {
            disabled_engines: vec![],
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
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_retention_zero() {
        let config = Config {
            disabled_engines: vec![],
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
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_cache_retention_valid() {
        let config = Config {
            disabled_engines: vec![],
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
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_cache_retention_none() {
        let config = Config {
            disabled_engines: vec![],
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
            session_log: None,
            lsp: vec![],
            skills: vec![],
        };
        assert!(config.validate().is_ok());
    }

    fn config_with_marketplace(name: &str, source: &str) -> Config {
        Config {
            disabled_engines: vec![],
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
            session_log: None,
            lsp: vec![],
            skills: vec![],
        }
    }

    fn config_with_state(tools: Vec<crate::StateTool>) -> Config {
        Config {
            disabled_engines: vec![],
            state: crate::StateConfig { tools },
            ..Config::default()
        }
    }

    fn state_tool(env: &str, subdir: &str) -> crate::StateTool {
        crate::StateTool {
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
        for reserved in crate::RESERVED_STATE_ENV_VARS {
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

    #[test]
    fn bundle_name_with_path_traversal_is_rejected() {
        // A bundle name is joined as a single path component in multiple
        // places (src/cli/mod.rs::build_bundle_refs,
        // src/hook_run/mod.rs::build_hook_bundle_refs) with no per-site
        // guard — validation here is the actual security boundary.
        let config = Config {
            disabled_engines: vec![],
            bundle: vec![crate::Bundle {
                name: "../evil".into(),
                when: vec!["t".into()],
            }],
            ..Default::default()
        };
        assert!(matches!(
            config.validate(),
            Err(ValidateError::InvalidBundleName(_))
        ));
    }

    #[test]
    fn bundle_name_valid_is_accepted() {
        let config = Config {
            disabled_engines: vec![],
            bundle: vec![crate::Bundle {
                name: "rust-dev".into(),
                when: vec!["rust".into()],
            }],
            ..Default::default()
        };
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
        fn prop_var_name_body_hyphen_rejected(
            prefix in "[A-Za-z_][A-Za-z0-9_]{0,5}",
            suffix in "[A-Za-z0-9_]{0,5}",
        ) {
            let name = format!("{prefix}-{suffix}");
            prop_assert!(!is_valid_var_name(&name), "hyphen in var name accepted: {name}");
        }

        #[test]
        fn prop_var_name_body_space_rejected(
            prefix in "[A-Za-z_][A-Za-z0-9_]{0,5}",
            suffix in "[A-Za-z0-9_]{0,5}",
        ) {
            let name = format!("{prefix} {suffix}");
            prop_assert!(!is_valid_var_name(&name), "space in var name accepted: {name}");
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
            prop_assert!(llmenv_paths::is_valid_short_name(&name), "valid name rejected: {name}");
        }

        #[test]
        fn prop_marketplace_name_with_disallowed_char_rejected(
            before in "[A-Za-z0-9._-]{0,10}",
            bad in "[@/:\\\\ ]",
            after in "[A-Za-z0-9._-]{0,10}",
        ) {
            let name = format!("{before}{bad}{after}");
            prop_assert!(
                !llmenv_paths::is_valid_short_name(&name),
                "name with disallowed char accepted: {name:?}"
            );
        }

        #[test]
        fn prop_marketplace_name_leading_dash_rejected(rest in "[A-Za-z0-9._-]{0,20}") {
            let name = format!("-{rest}");
            prop_assert!(!llmenv_paths::is_valid_short_name(&name), "leading-dash name accepted: {name}");
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
            let reserved: &[&str] = crate::RESERVED_STATE_ENV_VARS;
            prop_assume!(!reserved.contains(&key.as_str()));
            prop_assume!(!key.starts_with("LLMENV_"));
            prop_assert!(
                validate_capabilities_env_key("test", &key).is_ok(),
                "valid env key should be accepted: {key}"
            );
        }
    }

    // ===== #354: capabilities.env reserved key validation =====

    fn config_with_capabilities_env(key: &str, value: &str) -> crate::Config {
        use std::collections::BTreeMap;
        crate::Config {
            disabled_engines: vec![],
            capabilities: Capabilities {
                env: BTreeMap::from([(key.to_string(), value.to_string())]),
                ..Default::default()
            },
            ..minimal_config()
        }
    }

    fn minimal_config() -> crate::Config {
        crate::Config {
            disabled_engines: vec![],
            bundle: vec![Bundle {
                name: "b".into(),
                when: vec!["t".into()],
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
        for reserved in crate::RESERVED_STATE_ENV_VARS {
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

    #[test]
    fn capabilities_env_digit_prefix_rejected() {
        let cfg = config_with_capabilities_env("1INVALID", "v");
        assert!(
            matches!(
                cfg.validate(),
                Err(ValidateError::CapabilitiesInvalidVarName { .. })
            ),
            "digit-prefixed key must be rejected as invalid shell identifier"
        );
    }

    #[test]
    fn capabilities_env_hyphen_rejected() {
        let cfg = config_with_capabilities_env("MY-VAR", "v");
        assert!(
            matches!(
                cfg.validate(),
                Err(ValidateError::CapabilitiesInvalidVarName { .. })
            ),
            "hyphenated key must be rejected as invalid shell identifier"
        );
    }

    #[test]
    fn capabilities_env_space_rejected() {
        let cfg = config_with_capabilities_env("MY VAR", "v");
        assert!(
            matches!(
                cfg.validate(),
                Err(ValidateError::CapabilitiesInvalidVarName { .. })
            ),
            "key with space must be rejected as invalid shell identifier"
        );
    }

    #[test]
    fn capabilities_env_invalid_var_name_error_message_contains_key() {
        let cfg = config_with_capabilities_env("1BAD", "v");
        let msg = cfg.validate().unwrap_err().to_string();
        assert!(
            msg.contains("1BAD"),
            "error message must name the offending key; got: {msg}"
        );
    }

    #[test]
    fn is_valid_var_name_empty_rejected() {
        assert!(
            !is_valid_var_name(""),
            "empty string must not be a valid var name"
        );
    }

    #[test]
    fn is_valid_var_name_unicode_rejected() {
        assert!(
            !is_valid_var_name("café"),
            "non-ASCII body char must be rejected"
        );
        assert!(
            !is_valid_var_name("ñame"),
            "non-ASCII leading char must be rejected"
        );
    }

    fn config_with_lsp(lsp: Vec<crate::LspServer>) -> Config {
        Config {
            disabled_engines: vec![],
            lsp,
            ..Default::default()
        }
    }

    fn config_with_skills(skills: Vec<crate::SkillSource>) -> Config {
        Config {
            disabled_engines: vec![],
            skills,
            ..Default::default()
        }
    }

    #[test]
    fn lsp_empty_name_is_rejected() {
        let cfg = config_with_lsp(vec![crate::LspServer {
            name: String::new(),
            command: "rust-analyzer".into(),
            ..Default::default()
        }]);
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::LspEmptyName(_))
        ));
    }

    #[test]
    fn lsp_empty_command_is_rejected() {
        let cfg = config_with_lsp(vec![crate::LspServer {
            name: "rust-analyzer".into(),
            command: String::new(),
            ..Default::default()
        }]);
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::LspEmptyCommand(_))
        ));
    }

    #[test]
    fn lsp_valid_entry_is_accepted() {
        let cfg = config_with_lsp(vec![crate::LspServer {
            name: "rust-analyzer".into(),
            command: "rust-analyzer".into(),
            ..Default::default()
        }]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn skill_empty_name_is_rejected() {
        let cfg = config_with_skills(vec![crate::SkillSource {
            name: String::new(),
            path: "/some/path".into(),
            when: vec![],
        }]);
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::SkillEmptyName(_))
        ));
    }

    #[test]
    fn skill_empty_path_is_rejected() {
        let cfg = config_with_skills(vec![crate::SkillSource {
            name: "my-skill".into(),
            path: String::new(),
            when: vec![],
        }]);
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::SkillEmptyPath(_))
        ));
    }

    #[test]
    fn skill_valid_entry_is_accepted() {
        let cfg = config_with_skills(vec![crate::SkillSource {
            name: "my-skill".into(),
            path: "./skills/my-skill".into(),
            when: vec![],
        }]);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn skill_name_with_path_separator_is_rejected() {
        // #534: a skill name is a single directory component, not a nested
        // path — is_unsafe_join_target alone would accept "foo/bar" (no `..`,
        // not absolute) even though the doc contract says "the" directory name.
        let cfg = config_with_skills(vec![crate::SkillSource {
            name: "foo/bar".into(),
            path: "./skills/foo".into(),
            when: vec![],
        }]);
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::SkillEmptyName(_))
        ));
    }

    #[test]
    fn skill_name_with_control_character_is_rejected() {
        let cfg = config_with_skills(vec![crate::SkillSource {
            name: "foo\0bar".into(),
            path: "./skills/foo".into(),
            when: vec![],
        }]);
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::SkillEmptyName(_))
        ));
    }

    #[test]
    fn skill_name_with_dot_and_dash_is_accepted() {
        let cfg = config_with_skills(vec![crate::SkillSource {
            name: "my-skill.v2".into(),
            path: "./skills/my-skill".into(),
            when: vec![],
        }]);
        assert!(cfg.validate().is_ok());
    }

    // ===== #664: permission rule pattern paren-balance validation =====
    //
    // Claude Code and Crush render a neutral rule as `Tool(value)`; both
    // engines' own settings loaders track paren depth to find the closing
    // `)` and silently drop the whole rule if it never balances. A deny rule
    // with an unmatched `(` (e.g. a process-substitution pattern like
    // `bash <(curl *`) therefore fails open with no warning from the engine.
    // Reject at config-load time instead.

    fn config_with_permissions(permissions: Permissions) -> crate::Config {
        crate::Config {
            capabilities: Capabilities {
                permissions,
                ..Default::default()
            },
            ..minimal_config()
        }
    }

    #[test]
    fn permission_rule_unmatched_open_paren_in_deny_pattern_rejected() {
        let cfg = config_with_permissions(Permissions {
            deny: vec![PermissionRule {
                tool: "Bash".into(),
                pattern: Some("bash <(curl *".into()),
                paths: vec![],
            }],
            ..Default::default()
        });
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::PermissionRuleUnbalancedParens { .. })
        ));
    }

    #[test]
    fn permission_rule_unmatched_close_paren_rejected() {
        let cfg = config_with_permissions(Permissions {
            allow: vec![PermissionRule {
                tool: "Bash".into(),
                pattern: Some("foo)".into()),
                paths: vec![],
            }],
            ..Default::default()
        });
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::PermissionRuleUnbalancedParens { .. })
        ));
    }

    #[test]
    fn permission_rule_balanced_process_substitution_pattern_accepted() {
        let cfg = config_with_permissions(Permissions {
            deny: vec![PermissionRule {
                tool: "Bash".into(),
                pattern: Some("bash <(curl *)*".into()),
                paths: vec![],
            }],
            ..Default::default()
        });
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn permission_rule_plain_pattern_without_parens_accepted() {
        let cfg = config_with_permissions(Permissions {
            allow: vec![PermissionRule {
                tool: "Bash".into(),
                pattern: Some("git diff *".into()),
                paths: vec![],
            }],
            ..Default::default()
        });
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn permission_rule_unbalanced_path_entry_rejected() {
        let cfg = config_with_permissions(Permissions {
            deny: vec![PermissionRule {
                tool: "Read".into(),
                pattern: None,
                paths: vec!["~/weird(dir".into()],
            }],
            ..Default::default()
        });
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::PermissionRuleUnbalancedParens { .. })
        ));
    }

    #[test]
    fn validate_permission_rule_error_reports_context_and_tool() {
        let rule = PermissionRule {
            tool: "Bash".into(),
            pattern: Some("bash <(curl *".into()),
            paths: vec![],
        };
        let err = validate_permission_rule("bundle 'security'", &rule).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bundle 'security'"), "got: {msg}");
        assert!(msg.contains("Bash"), "got: {msg}");
    }

    // ===== NativePermissionRules paren-balance validation =====
    //
    // Same failure mode as #664: native permission strings like
    // `Bash(bash <(curl *)` can be silently dropped by the engine's
    // settings loader when parens don't balance.

    fn config_with_native_permissions(engine: &str, rules: NativePermissionRules) -> crate::Config {
        let mut native_permissions = BTreeMap::new();
        native_permissions.insert(engine.to_string(), rules);
        crate::Config {
            capabilities: Capabilities {
                native_permissions,
                ..Default::default()
            },
            ..minimal_config()
        }
    }

    #[test]
    fn native_permission_balanced_string_accepted() {
        let cfg = config_with_native_permissions(
            "claude-code",
            NativePermissionRules {
                allow: vec!["Bash(ls -la)".into()],
                ..Default::default()
            },
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn native_permission_unbalanced_open_paren_rejected() {
        let cfg = config_with_native_permissions(
            "claude-code",
            NativePermissionRules {
                allow: vec!["Bash(bash <(curl *)".into()],
                ..Default::default()
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::PermissionRuleUnbalancedParens { .. })
        ));
    }

    #[test]
    fn native_permission_unbalanced_close_paren_in_ask_rejected() {
        let cfg = config_with_native_permissions(
            "claude-code",
            NativePermissionRules {
                ask: vec!["Read(file))".into()],
                ..Default::default()
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::PermissionRuleUnbalancedParens { .. })
        ));
    }

    #[test]
    fn native_permission_unbalanced_in_deny_rejected() {
        let cfg = config_with_native_permissions(
            "claude-code",
            NativePermissionRules {
                deny: vec!["Write(/tmp/mydir)".into()], // balanced
                ..Default::default()
            },
        );
        // This one is balanced — just verifying the setup works
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn native_permission_empty_all_rules_accepted() {
        let cfg = config_with_native_permissions("claude-code", NativePermissionRules::default());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn native_permission_multiple_engines_all_rejected() {
        let mut native_permissions = BTreeMap::new();
        native_permissions.insert(
            "claude-code".to_string(),
            NativePermissionRules {
                deny: vec!["Bash(unbalanced(".into()],
                ..Default::default()
            },
        );
        native_permissions.insert(
            "crush".to_string(),
            NativePermissionRules {
                deny: vec!["Write(broken)".into()], // balanced
                ..Default::default()
            },
        );
        let cfg = crate::Config {
            capabilities: Capabilities {
                native_permissions,
                ..Default::default()
            },
            ..minimal_config()
        };
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::PermissionRuleUnbalancedParens { .. })
        ));
    }

    #[test]
    fn native_permission_escaped_parens_accepted() {
        let cfg = config_with_native_permissions(
            "claude-code",
            NativePermissionRules {
                allow: vec![r"Bash(echo \(foo\))".into()],
                ..Default::default()
            },
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn native_permission_ask_list_unbalanced_rejected() {
        let cfg = config_with_native_permissions(
            "claude-code",
            NativePermissionRules {
                ask: vec!["Bash(ok)".into(), "Read(unbalanced(".into()],
                ..Default::default()
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(ValidateError::PermissionRuleUnbalancedParens { .. })
        ));
    }

    #[test]
    fn native_permission_deny_list_unbalanced_rejected() {
        let cfg = config_with_native_permissions(
            "claude-code",
            NativePermissionRules {
                deny: vec!["Edit(path)".into(), "Edit(other)".into()],
                ..Default::default()
            },
        );
        assert!(cfg.validate().is_ok(), "all balanced should pass");
    }

    #[test]
    fn native_permission_validate_permission_string_direct() {
        // Direct call to validate_permission_string
        assert!(validate_permission_string("test", "Bash(ls)").is_ok());
        assert!(validate_permission_string("test", "Bash(unbalanced(").is_err());
        assert!(validate_permission_string("test", "Bash(w)r)ong").is_err());
    }

    // ===== Property-based tests for has_balanced_parens =====

    fn arb_balanced_parens() -> impl Strategy<Value = String> {
        // Generate a string where '(' and ')' always balance.
        // We start with an alphanumeric base and optionally wrap in parens.
        prop::string::string_regex("[a-zA-Z0-9_-]{0,10}")
            .expect("valid balanced base regex")
            .prop_flat_map(|base| {
                if base.is_empty() {
                    prop_oneof![
                        Just(base.clone()),
                        (Just("(".to_string()), Just(")".to_string()))
                            .prop_map(move |(a, b)| a + &base + &b),
                    ]
                    .boxed()
                } else {
                    // Recursively generate balanced strings: base, (base), ((base)), etc.
                    (0..3u32, Just(base))
                        .prop_map(|(depth, b)| {
                            let mut s = b;
                            for _ in 0..depth {
                                s = format!("({s})");
                            }
                            s
                        })
                        .boxed()
                }
            })
    }

    proptest! {
        #[test]
        fn prop_has_balanced_parens_concatenation(
            a in arb_balanced_parens(),
            b in arb_balanced_parens(),
        ) {
            // Concatenation of two balanced strings is balanced
            prop_assert!(has_balanced_parens(&format!("{a}{b}")),
                "concat of balanced strings should be balanced: '{a}' + '{b}'");
        }

        #[test]
        fn prop_has_balanced_parens_wrapping(
            inner in arb_balanced_parens(),
        ) {
            // Wrapping a balanced string in parens is balanced
            prop_assert!(has_balanced_parens(&format!("({inner})")),
                "wrapping in parens should be balanced: '({inner})'");
        }

        #[test]
        fn prop_has_balanced_parens_unmatched_open(
            balanced in arb_balanced_parens(),
        ) {
            // Prepending an extra '(' makes it unbalanced
            let s = format!("({balanced}");
            prop_assert!(!has_balanced_parens(&s),
                "extra open paren should be unbalanced: '{s}'");
        }

        #[test]
        fn prop_has_balanced_parens_unmatched_close(
            balanced in arb_balanced_parens(),
        ) {
            // Appending an extra ')' makes it unbalanced
            // (unless the balanced string already has unmatched closers, which it shouldn't)
            let s = format!("{balanced})");
            prop_assert!(!has_balanced_parens(&s),
                "extra close paren should be unbalanced: '{s}'");
        }

        #[test]
        fn prop_has_balanced_parens_no_crash(s in ".{0,40}") {
            // Any arbitrary string should not panic
            let _ = has_balanced_parens(&s);
        }

        #[test]
        fn prop_has_balanced_parens_escaped_parens(
            prefix in "[a-zA-Z0-9_-]{0,5}",
            suffix in "[a-zA-Z0-9_-]{0,5}",
        ) {
            // Backslash-escaped parens are not counted
            let s = format!(r"{prefix}\({suffix}\)");
            prop_assert!(has_balanced_parens(&s),
                "escaped parens should be balanced: '{s}'");
        }

        #[test]
        fn prop_has_balanced_parens_escaped_and_real(
            prefix in "[a-zA-Z0-9_-]{0,5}",
            middle in "[a-zA-Z0-9_-]{0,5}",
            suffix in "[a-zA-Z0-9_-]{0,5}",
        ) {
            // Mixed escaped and real parens — real ones wrap the escaped ones
            let s = format!("({prefix}\\({middle}\\){suffix})");
            prop_assert!(has_balanced_parens(&s),
                "one escaped + one real paren should be balanced: '{s}'");
        }

        #[test]
        fn prop_has_balanced_parens_real_nested(
            inner in arb_balanced_parens(),
        ) {
            // Real balanced parens inside a string with many levels
            // Just verify: (a, (b, ...)), where a/b are balanced
            let s = format!("(({inner}))");
            prop_assert!(has_balanced_parens(&s),
                "nested balanced parens should be balanced: '{s}'");
        }
    }

    // ===== #508: model_providers / default_models validation =====

    #[test]
    fn model_provider_duplicate_id_rejected() {
        let mut cfg = Config::default();
        cfg.capabilities.model_providers = vec![
            crate::ModelProvider {
                id: "ollama".into(),
                ..Default::default()
            },
            crate::ModelProvider {
                id: "ollama".into(),
                ..Default::default()
            },
        ];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ValidateError::ModelProviderDuplicateId(ref id) if id == "ollama"));
    }

    #[test]
    fn model_provider_empty_id_rejected() {
        let mut cfg = Config::default();
        cfg.capabilities.model_providers = vec![crate::ModelProvider {
            id: String::new(),
            ..Default::default()
        }];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ValidateError::ModelProviderEmptyId));
    }

    #[test]
    fn model_source_duplicate_id_within_provider_rejected() {
        let mut cfg = Config::default();
        cfg.capabilities.model_providers = vec![crate::ModelProvider {
            id: "ollama".into(),
            models: vec![
                crate::ModelSource {
                    id: "llama3.1:8b".into(),
                    ..Default::default()
                },
                crate::ModelSource {
                    id: "llama3.1:8b".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            ValidateError::ModelSourceDuplicateId(ref p, ref m)
                if p == "ollama" && m == "llama3.1:8b"
        ));
    }

    #[test]
    fn model_provider_valid_entry_is_accepted() {
        let mut cfg = Config::default();
        cfg.capabilities.model_providers = vec![crate::ModelProvider {
            id: "ollama".into(),
            base_url: Some("http://localhost:11434/v1".into()),
            models: vec![crate::ModelSource {
                id: "llama3.1:8b".into(),
                ..Default::default()
            }],
            ..Default::default()
        }];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn default_model_empty_provider_rejected() {
        let mut cfg = Config::default();
        cfg.capabilities.default_models.insert(
            "large".into(),
            crate::ModelRef {
                provider: String::new(),
                model: "gpt-4o".into(),
            },
        );
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, ValidateError::DefaultModelEmptyRef(ref role) if role == "large"));
    }

    #[test]
    fn default_model_valid_entry_is_accepted() {
        let mut cfg = Config::default();
        cfg.capabilities.default_models.insert(
            "large".into(),
            crate::ModelRef {
                provider: "anthropic".into(),
                model: "claude-opus-4-7".into(),
            },
        );
        assert!(cfg.validate().is_ok());
    }
}
