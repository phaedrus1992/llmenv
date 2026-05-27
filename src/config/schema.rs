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
}

impl Capabilities {
    /// True when no capability is declared — lets callers skip empty fragments.
    pub fn is_empty(&self) -> bool {
        self.permissions.is_empty() && self.hooks.is_empty() && self.plugins.is_empty()
    }
}

/// Permission rules over tools and paths. `default_mode` is a scalar (resolved by
/// scope precedence); `allow`/`ask`/`deny` are lists (concatenated). `native`
/// carries per-engine raw rule strings appended verbatim by the adapter.
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
    /// Per-engine raw rule lists, keyed by engine name. Opaque to llmenv.
    #[serde(default)]
    pub native: std::collections::BTreeMap<String, NativePermissionRules>,
}

impl Permissions {
    pub fn is_empty(&self) -> bool {
        self.default_mode.is_none()
            && self.allow.is_empty()
            && self.ask.is_empty()
            && self.deny.is_empty()
            && self.native.is_empty()
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
    #[serde(default)]
    pub project: Vec<ProjectScope>,
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
pub struct ProjectScope {
    pub id: String,
    pub r#match: ProjectMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectMatch {
    pub path_prefix: Option<String>,
    #[serde(alias = "marker_file")]
    pub marker: Option<String>,
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
