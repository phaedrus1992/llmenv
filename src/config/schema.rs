use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub cache: Cache,
    #[serde(default)]
    pub scope: Scopes,
    /// Engine-agnostic capabilities (permissions, hooks, plugins) declared at the
    /// top level. Merged with each selected bundle's `bundle.yaml` fragment by
    /// value shape: lists concatenate + dedup, scalars resolve by scope
    /// precedence. See `docs/design/engine-capabilities.md`.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Per-engine opaque passthrough. Keyed by engine name (e.g. `claude_code`).
    /// llmenv does not interpret these values — adapters merge them verbatim into
    /// the engine's native config. The escape hatch for non-portable keys
    /// (`alwaysThinkingEnabled`, `outputStyle`, …).
    #[serde(default)]
    pub native: std::collections::BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub bundle: Vec<Bundle>,
    /// MCP servers, selected onto scopes by tag intersection (same model as
    /// bundles). These are plain user-declared servers (stdio or remote);
    /// llmenv's own memory backend is configured separately under `memory`.
    #[serde(default)]
    pub mcp: Vec<McpServer>,
    /// llmenv's memory backend (ICM). A single optional topology: one host runs
    /// the daemon, every other selected host connects to it as a network
    /// client. Desugars into a resolved MCP server so it lands in the agent's
    /// MCP config alongside the `mcp` entries.
    #[serde(default)]
    pub memory: Option<Memory>,
    /// Agent plugin marketplaces, each a named source (git URL or local path).
    /// Referenced by `plugin-collection` entries as the left half of a
    /// `marketplace:plugin` identifier. Cloned once into the shared marketplace
    /// cache and rendered into the agent's plugin config by adapters that
    /// support plugins.
    #[serde(default)]
    pub marketplace: Vec<Marketplace>,
    /// Named bags of plugins, selected onto scopes by tag intersection (same
    /// model as bundles and MCP servers). The union of all selected
    /// collections' plugins is what gets wired up for the active host.
    #[serde(default, rename = "plugin-collection")]
    pub plugin_collection: Vec<PluginCollection>,
    /// Static host directory mapping a host name to a reachable address. Used
    /// by the `memory` topology to build a client URL pointing at the host
    /// that runs the server. Keyed by host name (matched against host-scope
    /// `id`s by convention, though any name the config references works).
    #[serde(default)]
    pub host: std::collections::BTreeMap<String, HostEntry>,
}

/// llmenv's own cache/sync behavior. Distinct from engine `capabilities` — this
/// governs the local materialization cache, not anything written into an agent's
/// config.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Cache {
    pub cache_dir: String,
    pub sync_interval_minutes: u64,
    pub cache_retention_hours: Option<u64>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            cache_dir: "~/.cache/llmenv".into(),
            sync_interval_minutes: 15,
            cache_retention_hours: Some(168), // 7 days
        }
    }
}

/// Engine-agnostic capability vocabulary. Identical shape whether declared at the
/// top level of `config.yaml` or in a bundle's `bundle.yaml`. Merged across all
/// contributors by value shape (see [`crate::merge`]).
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Capabilities {
    #[serde(default)]
    pub permissions: Permissions,
    /// Hook registrations. A list — concatenates across contributors.
    #[serde(default)]
    pub hooks: Vec<Hook>,
    /// Plugin ids as `<marketplace>:<plugin>`. A list — concatenates.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// Per-engine native permission rule lists, keyed by engine name. The
    /// engine-only override for permissions — raw rule strings in the engine's
    /// own grammar, appended verbatim. Sibling to the neutral `permissions`
    /// block (every feature's native override is a top-level `native_*` map).
    #[serde(default)]
    pub native_permissions: std::collections::BTreeMap<String, NativePermissionRules>,
    /// Per-engine native hook fragments, keyed by engine name. The engine-only
    /// layer-(b) override for hooks — engine-specific events/handlers that have
    /// no neutral equivalent, emitted verbatim. Opaque to llmenv.
    #[serde(default)]
    pub native_hooks: std::collections::BTreeMap<String, serde_yaml::Value>,
    /// Per-engine native plugin fragments, keyed by engine name. The engine-only
    /// override for plugins (e.g. a Claude-only plugin flag). Opaque to llmenv.
    #[serde(default)]
    pub native_plugins: std::collections::BTreeMap<String, serde_yaml::Value>,
    /// Per-engine native MCP fragments, keyed by engine name. The engine-only
    /// override for MCP (e.g. `enabledMcpjsonServers`, a transport quirk).
    /// Opaque to llmenv.
    #[serde(default)]
    pub native_mcp: std::collections::BTreeMap<String, serde_yaml::Value>,
}

impl Capabilities {
    /// True when no capability is declared — lets callers skip empty fragments.
    pub fn is_empty(&self) -> bool {
        self.permissions.is_empty()
            && self.hooks.is_empty()
            && self.plugins.is_empty()
            && self.native_permissions.is_empty()
            && self.native_hooks.is_empty()
            && self.native_plugins.is_empty()
            && self.native_mcp.is_empty()
    }
}

/// Neutral permission rules over tools and paths. `default_mode` is a scalar
/// (resolved by scope precedence); `allow`/`ask`/`deny` are lists (concatenated).
/// The per-engine raw rule override lives in the sibling `native_permissions`
/// map on [`Capabilities`], not here — matching every other feature's
/// `native_*` override shape.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Permissions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<PermissionMode>,
    #[serde(default)]
    pub allow: Vec<PermissionRule>,
    #[serde(default)]
    pub ask: Vec<PermissionRule>,
    #[serde(default)]
    pub deny: Vec<PermissionRule>,
}

impl Permissions {
    pub fn is_empty(&self) -> bool {
        self.default_mode.is_none()
            && self.allow.is_empty()
            && self.ask.is_empty()
            && self.deny.is_empty()
    }
}

/// Per-engine raw permission rule strings, in the engine's own grammar (e.g.
/// Claude's `WebFetch(domain:...)`). Appended verbatim — never translated.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct NativePermissionRules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Neutral default permission mode. Adopts Claude Code's vocabulary as the
/// engine-neutral set (open question O2 resolved in favor of reuse).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    AcceptEdits,
    Plan,
    Default,
    BypassPermissions,
}

/// A neutral permission rule: a tool plus either a glob `pattern` or a list of
/// path roots. The adapter renders this to the engine's string grammar.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PermissionRule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

/// A hook registration. `command` paths in `handler` are bundle-relative when
/// declared in a `bundle.yaml`, resolved at materialize time.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Hook {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub handler: HookHandler,
    #[serde(skip)]
    pub bundle_origin: Option<std::path::PathBuf>,
}

/// A hook handler. `type` selects the mechanism; `command` is set for
/// `command`-type handlers, `tool` for `mcp_tool`-type.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HookHandler {
    #[serde(rename = "type")]
    pub kind: HookHandlerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookHandlerKind {
    Command,
    McpTool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Scopes {
    #[serde(default)]
    pub network: Vec<NetworkScope>,
    #[serde(default)]
    pub host: Vec<HostScope>,
    #[serde(default)]
    pub user: Vec<UserScope>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NetworkScope {
    pub id: String,
    pub r#match: NetworkMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NetworkMatch {
    pub gateway_mac: Option<String>,
    pub ssid: Option<String>,
    pub cidr: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostScope {
    pub id: String,
    pub r#match: HostMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostMatch {
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserScope {
    pub id: String,
    pub r#match: UserMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserMatch {
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Bundle {
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, String>,
}

/// A reachable address for a named host, used by the `memory` backend to
/// construct a client URL pointing at whichever host runs the server.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostEntry {
    /// Hostname, DNS name, or IP literal (e.g. `"still.local"`, `"10.0.0.4"`).
    pub addr: String,
}

/// Transport for an MCP server. `stdio` launches a local subprocess; `http`
/// and `sse` register a remote endpoint by URL.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
    Sse,
}

/// llmenv's memory backend topology. One host (`server_host`) runs the daemon
/// locally over stdio (wrapped in `mcp-proxy` to expose it on the network);
/// every other selected host connects to it as an HTTP client at
/// `http://<addr>:<port>`. ICM is an implementation detail — the config
/// vocabulary deliberately does not mention it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Memory {
    /// Name of the host (key into the top-level `host:` table) that runs the
    /// memory daemon.
    pub server_host: String,
    /// Port the proxy listens on and clients connect to.
    pub port: u16,
    /// Tags that activate the memory server, intersected with active scope
    /// tags (same selection model as bundles and MCP servers).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Default memory topics, surfaced for documentation/tooling. Not consumed
    /// by rendering today but preserved so config round-trips.
    #[serde(default)]
    pub default_topics: Vec<String>,
}

/// An MCP server definition. Selected onto a scope when any of its `tags`
/// intersect the active scope tag set (identical to bundle selection).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpServer {
    /// Registration name in the agent's MCP config (e.g. `"playwright"`).
    pub name: String,
    /// Tags that activate this server, intersected with active scope tags.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, rename = "type")]
    pub transport: McpTransport,
    /// Command to launch for `stdio` transport.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Endpoint URL for `http`/`sse` transport.
    pub url: Option<String>,
}

/// An agent plugin marketplace: a name plus a source the marketplace is fetched
/// from. The `source` is interpreted by [`MarketplaceSource::classify`] as
/// either a git URL (cloned into the shared cache) or a local path (used in
/// place). Marketplaces are referenced from [`PluginCollection`] entries as the
/// left half of a `marketplace:plugin` identifier.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Marketplace {
    /// Registration name, referenced by `plugin-collection` plugin strings.
    pub name: String,
    /// Where the marketplace lives: a git URL (`https://…`, `git@…`,
    /// `ssh://…`, `git://…`) or a local filesystem path (absolute, `~`-relative,
    /// or `./`-relative).
    pub source: String,
}

/// How a [`Marketplace::source`] string is fetched. A git URL is cloned into
/// the shared marketplace cache and refreshed by `plugin sync`; a local path is
/// used in place (no fetch, content-hashed by its current state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketplaceSource {
    /// Remote git repository, cloned into `~/.cache/llmenv/marketplaces/<name>/`.
    Git,
    /// Local filesystem path, used in place.
    Path,
}

impl Marketplace {
    /// Classify [`Self::source`] as a git URL or a local path. Anything with a
    /// recognised git scheme (`https://`, `http://`, `ssh://`, `git://`) or
    /// scp-style `host:path` with no leading slash is treated as git; everything
    /// else (absolute, `~`, `./`, `../`, bare relative) is a local path.
    #[must_use]
    pub fn classify_source(&self) -> MarketplaceSource {
        classify_source(&self.source)
    }
}

/// Classify a marketplace source string. Split out as a free function so both
/// the schema accessor and validation can share one definition.
#[must_use]
pub fn classify_source(source: &str) -> MarketplaceSource {
    const GIT_SCHEMES: &[&str] = &["https://", "http://", "ssh://", "git://", "git+ssh://"];
    if GIT_SCHEMES.iter().any(|s| source.starts_with(s)) {
        return MarketplaceSource::Git;
    }
    // Local-path forms take priority over scp-style detection so a Windows-style
    // `C:\…` or a `~`/`./` path is never misread as `host:path`.
    if source.starts_with('/')
        || source.starts_with("~")
        || source.starts_with("./")
        || source.starts_with("../")
    {
        return MarketplaceSource::Path;
    }
    // scp-style `git@host:owner/repo` — a colon before any slash, with text on
    // both sides — is git. Otherwise treat as a bare relative path.
    if let Some(colon) = source.find(':') {
        let before = &source[..colon];
        let after = &source[colon + 1..];
        if !before.is_empty() && !after.is_empty() && !before.contains('/') {
            return MarketplaceSource::Git;
        }
    }
    MarketplaceSource::Path
}

/// A named collection of plugins selected onto a scope by tag intersection
/// (identical model to bundles and MCP servers). Each plugin is identified as
/// `<marketplace>:<plugin>`, where `<marketplace>` references a top-level
/// [`Marketplace`] by name.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginCollection {
    /// Collection name, used in diagnostics and `plugin ls` provenance.
    pub name: String,
    /// Tags that activate this collection, intersected with active scope tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Plugins in this collection, each `<marketplace>:<plugin>`.
    #[serde(default)]
    pub plugins: Vec<String>,
}

/// Split a `<marketplace>:<plugin>` identifier into its two halves. Returns
/// `None` when the string is not exactly one non-empty marketplace and one
/// non-empty plugin separated by a single `:`.
#[must_use]
pub fn split_plugin_ref(s: &str) -> Option<(&str, &str)> {
    let (marketplace, plugin) = s.split_once(':')?;
    if marketplace.is_empty() || plugin.is_empty() || plugin.contains(':') {
        return None;
    }
    Some((marketplace, plugin))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn classify_scp_style_is_git() {
        assert_eq!(
            classify_source("git@github.com:owner/repo"),
            MarketplaceSource::Git
        );
    }

    #[test]
    fn split_plugin_ref_roundtrips() {
        assert_eq!(
            split_plugin_ref("superpowers:caveman"),
            Some(("superpowers", "caveman"))
        );
    }

    #[test]
    fn split_plugin_ref_rejects_malformed() {
        assert_eq!(split_plugin_ref("nocolon"), None);
        assert_eq!(split_plugin_ref(":plugin"), None);
        assert_eq!(split_plugin_ref("market:"), None);
        assert_eq!(split_plugin_ref("a:b:c"), None);
    }

    proptest! {
        #[test]
        fn prop_git_scheme_sources_classified_git(
            scheme in prop_oneof![
                Just("https://"),
                Just("http://"),
                Just("ssh://"),
                Just("git://"),
                Just("git+ssh://"),
            ],
            rest in "[a-z0-9./_-]{1,30}",
        ) {
            let source = format!("{scheme}{rest}");
            prop_assert_eq!(classify_source(&source), MarketplaceSource::Git);
        }

        #[test]
        fn prop_absolute_and_tilde_paths_classified_path(
            prefix in prop_oneof![Just("/"), Just("~/"), Just("./"), Just("../")],
            rest in "[a-z0-9._-]{0,30}",
        ) {
            let source = format!("{prefix}{rest}");
            prop_assert_eq!(classify_source(&source), MarketplaceSource::Path);
        }

        #[test]
        fn prop_classify_source_never_panics(source in ".{0,60}") {
            // The classifier must total over arbitrary input.
            let _ = classify_source(&source);
        }

        #[test]
        fn prop_split_plugin_ref_roundtrip(
            market in "[a-z0-9_-]{1,15}",
            plugin in "[a-z0-9_-]{1,15}",
        ) {
            let s = format!("{market}:{plugin}");
            prop_assert_eq!(split_plugin_ref(&s), Some((market.as_str(), plugin.as_str())));
        }

        #[test]
        fn prop_split_plugin_ref_no_colon_is_none(s in "[a-z0-9_-]{0,30}") {
            prop_assert_eq!(split_plugin_ref(&s), None);
        }

        #[test]
        fn prop_split_plugin_ref_never_panics(s in ".{0,60}") {
            let _ = split_plugin_ref(&s);
        }
    }
}
