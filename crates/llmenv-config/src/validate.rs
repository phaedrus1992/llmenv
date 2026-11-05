use super::Config;
#[cfg(test)]
use super::PermissionRule;
use llmenv_paths::has_parent_component;

mod permissions;
mod scope_match;
mod servers;
mod widgets;
#[cfg(test)]
use permissions::has_balanced_parens;
pub use permissions::{validate_permission_rule, validate_permission_string};
use scope_match::{is_valid_cidr, is_valid_hostname, is_valid_mac_address};
use thiserror::Error;
pub use widgets::check_widget_display;

#[cfg(test)]
use super::{
    Bundle, Cache, Capabilities, ContentMatch, ContentScope, ContextMode, Features, Hook,
    HookHandler, HookHandlerKind, HostEntry, HostMatch, HostScope, Marketplace,
    McpPermissionAction, McpPermissions, McpServer, McpTransport, Memory, NativePermissionRules,
    NetworkMatch, NetworkScope, PermissionMode, PermissionPreset, Permissions, PluginCollection,
    Scopes, Throttle, UserMatch, UserScope,
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
    #[error("transcript.retention_days must be >= 1")]
    RetentionDaysInvalid,
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
    #[error(
        "memory: wakeup_max_tokens ({0}) must be between 20 and 4000 (icm's own MCP handler \
         clamps to this range; llmenv fails fast instead of silently truncating)"
    )]
    MemoryWakeupMaxTokensInvalid(u32),
    #[error("features.codebase_memory entry has no `when` tags")]
    CodebaseMemoryNoTags,
    #[error(
        "features.codebase_memory mem_budget_mb ({0}) must be between 1 and 1048576. Set it to the \
         number of megabytes the indexer may use, or remove it"
    )]
    CodebaseMemoryBudgetInvalid(u32),
    #[error(
        "features.codebase_memory allowed_roots entry '{0}' is not valid. Use a path that starts \
         with /, ~, or a $VARIABLE, and write each variable as $NAME or ${{NAME}}"
    )]
    CodebaseMemoryRootInvalid(String),
    #[error(
        "features.task_tracker {0} must be 1 or more. Set the number of tool calls, or remove it"
    )]
    TaskTrackerNudgeCountInvalid(&'static str),
    #[error(
        "features.task_tracker workflow_skills has an empty entry. Name a skill, or remove the entry"
    )]
    TaskTrackerSkillEmpty,
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
    #[error("{context}: {field} is '{value}'; allowed values: {allowed}.{hint}")]
    InvalidEffortLevel {
        context: String,
        field: String,
        value: String,
        allowed: String,
        hint: String,
    },
    #[error(
        "{context}: model_effort.{model} sets no field; set effort_level, max_effort_level, or both"
    )]
    ModelEffortEmpty { context: String, model: String },
    #[error(
        "{context}: model_effort key '{model}' is not a canonical Claude model ID; use the \
         canonical model ID, such as claude-opus-5-5; Claude Code matches aliases to that entry itself."
    )]
    ModelEffortKey { context: String, model: String },
    #[error(
        "{context}: capabilities.advisor_model is '{value}'; use fable, opus, sonnet, or a \
         canonical model ID such as claude-opus-5-5."
    )]
    AdvisorModelInvalid { context: String, value: String },
    #[error(
        "{context}: capabilities.advisor_size was removed because Claude Code never read it; set \
         capabilities.advisor_model to fable, opus, sonnet, or a model ID."
    )]
    AdvisorSizeRemoved { context: String },
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
        "output style '{0}' is not a valid style name: use only ASCII letters, digits, \
         '.', '_', '-'"
    )]
    OutputStyleInvalidName(String),
    #[error("output style '{0}' has an empty description")]
    OutputStyleEmptyDescription(String),
    #[error("output style '{0}' has empty content")]
    OutputStyleEmptyContent(String),
    #[error(
        "duplicate output style name (case-insensitive): '{0}' — names must be unique \
         regardless of case, since case-insensitive filesystems (macOS, Windows) would \
         otherwise collide when writing output-styles/*.md"
    )]
    OutputStyleDuplicateName(String),
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
    #[error("hook '{event}' has type: command but no command")]
    HookCommandMissingCommand { event: String },
    #[error("hook '{event}' has type: mcp_tool but no tool")]
    HookMcpToolMissingTool { event: String },
    #[error(
        "widget {widget}: display \"{value}\" is not one of {allowed}. Fix: set display to one of \
         those values, or remove it."
    )]
    WidgetDisplayInvalid {
        widget: String,
        value: String,
        allowed: String,
    },
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
    !dir.contains('\0') && !has_parent_component(dir)
}

/// Whether `root` has a shape that the codebase-memory root expansion accepts: an absolute path,
/// `~` alone or `~/...` (`~user` is not supported), or a path whose variables are well formed.
/// The variable grammar is the one the expansion uses, so a malformed entry fails here rather
/// than being dropped at session start.
fn allowed_root_shape_is_valid(root: &str) -> bool {
    let root = root.trim();
    let rest = match root.strip_prefix('~') {
        Some(after) if after.is_empty() || after.starts_with('/') => after,
        Some(_) => return false,
        None => root,
    };
    if !(root.starts_with('/') || root.starts_with('~') || root.starts_with('$')) {
        return false;
    }
    llmenv_util::expand_env_refs(rest, &|_| Some("x".to_string())).is_some()
}

impl Config {
    pub fn validate(&self) -> Result<(), ValidateError> {
        self.validate_cache()?;
        self.validate_widget_displays()?;
        self.validate_scope_ids()?;
        self.validate_scope_matches()?;
        self.validate_bundles()?;
        self.validate_capability_env()?;
        self.validate_mcps()?;
        self.validate_hooks()?;
        self.validate_lsp()?;
        self.validate_skills()?;
        self.validate_output_styles()?;
        self.validate_model_providers()?;
        self.validate_default_models()?;
        self.validate_plugins()?;
        self.validate_state()?;
        self.validate_permissions()?;
        Ok(())
    }

    fn validate_cache(&self) -> Result<(), ValidateError> {
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
        if let Some(days) = self
            .session_log
            .as_ref()
            .and_then(|s| s.transcript.as_ref())
            .and_then(|t| t.retention_days)
            && days == 0
        {
            return Err(ValidateError::RetentionDaysInvalid);
        }
        Ok(())
    }

    /// A scope id names one scope across every scope kind, so it must be unique over all of them.
    fn validate_scope_ids(&self) -> Result<(), ValidateError> {
        let mut seen_scope_ids = std::collections::HashSet::new();
        let ids = self
            .scope
            .network
            .iter()
            .map(|s| &s.id)
            .chain(self.scope.host.iter().map(|s| &s.id))
            .chain(self.scope.user.iter().map(|s| &s.id))
            .chain(self.scope.content.iter().map(|s| &s.id));
        for id in ids {
            if !seen_scope_ids.insert(id) {
                return Err(ValidateError::DuplicateScopeId(id.clone()));
            }
        }
        Ok(())
    }

    fn validate_scope_matches(&self) -> Result<(), ValidateError> {
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
        Ok(())
    }

    fn validate_bundles(&self) -> Result<(), ValidateError> {
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
        Ok(())
    }

    fn validate_capability_env(&self) -> Result<(), ValidateError> {
        for key in self.capabilities.env.keys() {
            validate_capabilities_env_key("config.yaml: capabilities", key)?;
        }
        crate::effort::validate_effort("config.yaml: capabilities", &self.capabilities)?;
        crate::advisor::validate_advisor("config.yaml: capabilities", &self.capabilities)
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
        let mut seen_marketplace_names: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
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
            if !seen_marketplace_names.insert(m.name.as_str()) {
                return Err(ValidateError::DuplicateMarketplaceName(m.name.clone()));
            }
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
                if !seen_marketplace_names.contains(marketplace) {
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
    /// Every widget `display` must be a preset that the widget accepts (#3059).
    fn validate_widget_displays(&self) -> Result<(), ValidateError> {
        let Some(statusline) = &self.statusline else {
            return Ok(());
        };
        for (widget, cfg) in &statusline.widgets {
            if let Some(value) = &cfg.display {
                check_widget_display(widget, value)?;
            }
        }
        Ok(())
    }

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
        servers::validate_mcp_servers(&self.mcp)?;
        servers::validate_hosts(&self.host)?;
        match &self.features {
            Some(features) => servers::validate_features(features, &self.host),
            None => Ok(()),
        }
    }

    /// A hook whose `kind` doesn't match its `command`/`tool` field is
    /// schema-valid but produces a silent no-op in the generated engine
    /// config (Claude Code emits `{"type":"command"}` with no command key
    /// and runs nothing; Crush rejects it at load time). Fail fast here
    /// instead, mirroring `validate_mcps`'s `McpStdioMissingCommand`.
    fn validate_hooks(&self) -> Result<(), ValidateError> {
        use super::HookHandlerKind;

        let is_missing = |field: &Option<String>| field.as_deref().unwrap_or("").is_empty();
        for hook in &self.capabilities.hooks {
            match hook.handler.kind {
                HookHandlerKind::Command if is_missing(&hook.handler.command) => {
                    return Err(ValidateError::HookCommandMissingCommand {
                        event: hook.event.clone(),
                    });
                }
                HookHandlerKind::McpTool if is_missing(&hook.handler.tool) => {
                    return Err(ValidateError::HookMcpToolMissingTool {
                        event: hook.event.clone(),
                    });
                }
                HookHandlerKind::Command | HookHandlerKind::McpTool => {}
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

    // #1130 (security-audit P2): mirrors validate_skills — catches an unsafe
    // name, an empty description/content, or a duplicate at config-load time
    // instead of letting it surface mid-materialize after other files were
    // already written. Duplicate check is case-insensitive because output
    // styles render to `output-styles/<name>.md`, and a case-insensitive
    // filesystem (macOS, Windows) collapses two distinct-cased names onto the
    // same file.
    fn validate_output_styles(&self) -> Result<(), ValidateError> {
        let mut seen_names = std::collections::HashSet::new();
        for o in &self.output_styles {
            if !llmenv_paths::is_valid_short_name(&o.name) {
                return Err(ValidateError::OutputStyleInvalidName(o.name.clone()));
            }
            if o.description.is_empty() {
                return Err(ValidateError::OutputStyleEmptyDescription(o.name.clone()));
            }
            if o.content.is_empty() {
                return Err(ValidateError::OutputStyleEmptyContent(o.name.clone()));
            }
            if !seen_names.insert(o.name.to_lowercase()) {
                return Err(ValidateError::OutputStyleDuplicateName(o.name.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

#[cfg(test)]
mod hostname_properties;
