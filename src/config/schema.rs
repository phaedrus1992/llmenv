use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub scope: Scopes,
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    pub cache_dir: String,
    pub sync_interval_minutes: u64,
    pub cache_retention_hours: Option<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cache_dir: "~/.cache/llmenv".into(),
            sync_interval_minutes: 15,
            cache_retention_hours: Some(168), // 7 days
        }
    }
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
