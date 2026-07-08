use serde::{Deserialize, Serialize};

/// Settings pre-seeded during `llmenv init` for newly-materialized folders.
///
/// Keys are written into `settings.json` as foreign (non-llmenv-owned) keys
/// before the adapter's render pass, so they survive every re-render via the
/// existing foreign-key preservation in `reconcile_settings`. Values preserve
/// their original JSON type (bool, number, string, etc.).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct InitConfig {
    /// Non-owned `settings.json` keys the user elected to carry into every new
    /// materialized folder. Never contains keys from the adapter's owned-key
    /// set — validated at write time by `llmenv init`.
    #[serde(default)]
    pub seeded_settings: serde_json::Map<String, serde_json::Value>,
}

/// llmenv feature toggles and experimental configuration. Nested under
/// `features:` in `config.yaml`.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Features {
    /// llmenv's memory backend (ICM). A list of tag-scoped topology entries:
    /// each declares one host that runs the daemon and the tag set that
    /// activates it. The resolver selects by tag intersection (same model as
    /// bundles and MCP servers) and errors when more than one entry is active
    /// simultaneously — the `icm` name is reserved and singular at
    /// connect-time. Zero active entries is valid (memory disabled for this
    /// scope).
    #[serde(default)]
    pub memory: Vec<Memory>,
    /// Tag-scoped usage throttle entries. At most one entry is active per scope
    /// (same model as memory). Zero active entries means throttling is disabled.
    #[serde(default)]
    pub throttle: Vec<Throttle>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
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
    /// Feature toggles and experimental configuration.
    #[serde(default)]
    pub features: Option<Features>,
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
    /// Durable state relocation (#175). Tools that persist runtime state into
    /// `CLAUDE_CONFIG_DIR` lose it on every content-hash change; this declares
    /// per-tool env vars llmenv points at a stable, hash-independent state dir.
    #[serde(default)]
    pub state: StateConfig,
    /// Static host directory mapping a host name to a reachable address. Used
    /// by the `memory` topology to build a client URL pointing at the host
    /// that runs the server. Keyed by host name (matched against host-scope
    /// `id`s by convention, though any name the config references works).
    #[serde(default)]
    pub host: std::collections::BTreeMap<String, HostEntry>,
    /// Settings pre-seeded during `llmenv init`. Written as foreign keys into
    /// new materialized folders' `settings.json`, surviving every re-render.
    #[serde(default)]
    pub init: InitConfig,
    /// Path to a `.jsonl` file for session logging. When set, llmenv appends one
    /// JSON object per tracing event. Tilde-expanded at startup. Disabled by
    /// default (`None` = no file written).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log: Option<String>,
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
    /// Cache-folder strictness dial (#246). A single knob trading cache reuse
    /// against invalidation precision: `loose` ⊂ `normal` ⊂ `strict`. See
    /// [`HashingMode`] for the per-mode folder layout. Drift detection and
    /// reconciliation are identical across modes — the manifest dotfile carries
    /// the content hash and owned-file set regardless.
    pub hashing: HashingMode,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            cache_dir: "~/.cache/llmenv".into(),
            sync_interval_minutes: 15,
            cache_retention_hours: Some(168), // 7 days
            hashing: HashingMode::default(),
        }
    }
}

/// Cache-folder strictness dial (#246). One knob, three positions, ordered by
/// how aggressively a folder is reused: `loose` ⊂ `normal` ⊂ `strict`. Default
/// is [`Self::Normal`] — the common path, isolating by selection *shape* and
/// binary minor version while still reusing a folder across content edits so a
/// running agent's in-session state survives a re-render.
///
/// The *shape* is a 12-hex digest over the active selection (`active_tags ∪
/// directly_enabled_bundles`), so two different tag/bundle combinations never
/// collide in one folder (the version-mode overwrite bug that motivated #246).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HashingMode {
    /// Folder = `<adapter>/<shape>`. No version axis: a binary upgrade reuses
    /// the same per-shape folder. Fewest folders; relies on age-based gc to
    /// trim shapes that fall out of use.
    Loose,
    /// Folder = `<adapter>/<version_mm>/<shape>` (`version_mm` = `major.minor`).
    /// The default. Content edits re-render into the same folder; a minor
    /// version bump or a selection change mints a new one.
    #[default]
    Normal,
    /// Folder = `<adapter>/<VERSION_TAG>-<content_hash>`. Any input change mints
    /// a fresh folder — strongest isolation, most cache churn.
    Strict,
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
    /// MCP servers declared inside a bundle. A list — concatenates across contributors.
    /// Tagless entries are active whenever the bundle is selected; tagged entries are
    /// further filtered by scope tag intersection. Neutral counterpart to `native_mcp`.
    #[serde(default)]
    pub mcp: Vec<McpServer>,
    /// Environment variables declared inside a bundle. Merged into the agent's env.
    /// A map — later contributors override earlier ones (same precedence model as
    /// the top-level config merging).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Whether the agent's built-in automatic memory is enabled. Optional scalar
    /// — resolves by scope precedence (highest scope wins). When llmenv's ICM
    /// memory backend is active, this defaults to `false` to prevent competition
    /// between memory systems, but can be overridden here if needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_memory_enabled: Option<bool>,
    /// Agent reasoning effort level (e.g., "low", "medium", "high"). Optional scalar
    /// — resolves by scope precedence. Engine-specific via native override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<String>,
    /// Advisor/expert capability size ("small", "medium", "large"). Optional scalar — resolves by
    /// scope precedence. Adapters map to their engine-specific models via native overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_size: Option<String>,
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
    /// Per-engine opaque passthrough values merged verbatim into the engine's
    /// native config by adapters. Identical shape to the top-level `native:`
    /// block in `config.yaml`; bundle contributions deep-merge with it.
    #[serde(default)]
    pub native: std::collections::BTreeMap<String, serde_yaml::Value>,
    /// Memory backend entries contributed by this capability source (bundle or
    /// top-level). Merged with all other contributors' entries at resolve time
    /// by concat + dedup; the resolver then selects by tag intersection and
    /// errors on ambiguity (>1 active). Uses the same YAML shape as the
    /// top-level `features.memory` list.
    #[serde(default)]
    pub features: Option<Features>,
    /// Host address table entries contributed by this capability source. Merged
    /// per-key across contributors: higher-precedence contributor wins on
    /// collision (same scalar rule as `env`).
    #[serde(default)]
    pub host: std::collections::BTreeMap<String, HostEntry>,
}

impl Capabilities {
    /// True when no capability is declared — lets callers skip empty fragments.
    pub fn is_empty(&self) -> bool {
        self.permissions.is_empty()
            && self.hooks.is_empty()
            && self.plugins.is_empty()
            && self.mcp.is_empty()
            && self.env.is_empty()
            && self.auto_memory_enabled.is_none()
            && self.effort_level.is_none()
            && self.advisor_size.is_none()
            && self.native_permissions.is_empty()
            && self.native_hooks.is_empty()
            && self.native_plugins.is_empty()
            && self.native_mcp.is_empty()
            && self.native.is_empty()
            && self.features.as_ref().is_none_or(|f| f.memory.is_empty())
            && self.host.is_empty()
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

/// An environment variable to inject into agent context. `name` is the variable
/// name; `value` is the value. Both are required.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// A hook registration. `command` paths in `handler` are bundle-relative when
/// declared in a `bundle.yaml`, resolved at materialize time.
#[derive(Debug, Clone, Deserialize, Serialize, Eq)]
pub struct Hook {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub handler: HookHandler,
    #[serde(skip)]
    pub bundle_origin: Option<std::path::PathBuf>,
}

impl PartialEq for Hook {
    fn eq(&self, other: &Self) -> bool {
        self.event == other.event && self.matcher == other.matcher && self.handler == other.handler
    }
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
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub name: String,
    #[serde(default)]
    pub when: Vec<String>,
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

/// Default bind host for the memory server proxy.
fn default_listen_host() -> String {
    "127.0.0.1".to_string()
}

/// Memory type classification for stored memory chunks (R1).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    /// Unclassified / generic memory.
    #[default]
    Episodic,
    /// Factual knowledge extracted from experience.
    Semantic,
    /// How-to / operational knowledge.
    Procedural,
}

impl MemoryType {
    /// Return the lowercase snake_case string used in HTML-comment markers.
    /// Matches `#[serde(rename_all = "snake_case")]` — avoids `Debug` formatting
    /// which would silently diverge for multi-word variants.
    pub fn as_marker_str(&self) -> &'static str {
        match self {
            Self::Episodic => "episodic",
            Self::Semantic => "semantic",
            Self::Procedural => "procedural",
        }
    }
}

/// Importance level for stored memory chunks (R3).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ImportanceLevel {
    /// Routine / low-priority information.
    Low,
    /// Notable but not critical.
    #[default]
    Medium,
    /// Important information worth keeping.
    High,
    /// Critical — must not be forgotten.
    Critical,
}

impl ImportanceLevel {
    /// Return the lowercase snake_case string used in HTML-comment markers.
    /// Matches `#[serde(rename_all = "snake_case")]` — avoids `Debug` formatting.
    pub fn as_marker_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// Post-session consolidation configuration (R5). Opt-in — disabled by default.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConsolidationConfig {
    /// Whether consolidation is enabled. Defaults to `false` — opt-in.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of semantic rules to distill per session.
    #[serde(default = "default_max_rules")]
    pub max_rules_per_session: u32,
}

const fn default_max_rules() -> u32 {
    10
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
    /// Host address the proxy binds to on the server. Defaults to `"127.0.0.1"`
    /// (loopback only). Set to `"0.0.0.0"` to accept connections on all
    /// interfaces, or to a specific IP to bind to one interface. Must be a valid
    /// IP address literal; hostnames are not supported.
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    /// Tags that activate the memory server, intersected with active scope
    /// tags (same selection model as bundles and MCP servers).
    #[serde(default)]
    pub when: Vec<String>,
    /// Default memory topics, surfaced for documentation/tooling. Not consumed
    /// by rendering today but preserved so config round-trips.
    #[serde(default)]
    pub default_topics: Vec<String>,
    /// Default memory type when no <!-- llmenv-type: --> marker is present
    /// in the context chunk (R1).
    #[serde(default)]
    pub default_type: Option<MemoryType>,
    /// Default importance when no <!-- llmenv-importance: --> marker is
    /// present in the context chunk (R3).
    #[serde(default)]
    pub default_importance: Option<ImportanceLevel>,
    /// Per-type importance overrides. When no inline marker is present and
    /// the resolved memory type (from marker or default_type) matches a key
    /// here, the mapped importance is used (R3).
    #[serde(default)]
    pub type_importance: std::collections::BTreeMap<MemoryType, ImportanceLevel>,
    /// Post-session consolidation configuration (R5). When `None`,
    /// consolidation is unavailable. When `Some(Config)`, gated by
    /// `consolidation.enabled` (default: false — opt-in).
    #[serde(default)]
    pub consolidation: Option<ConsolidationConfig>,
}

fn default_throttle_cache_ttl() -> u64 {
    30
}
fn default_throttle_max_wait() -> u64 {
    300
}
fn default_throttle_soft_threshold() -> u64 {
    20
}

/// Usage throttling configuration. A tag-scoped entry that injects PreToolUse
/// and UserPromptSubmit hooks which poll the named backend and sleep a capped
/// adaptive delay to avoid hitting rate limits.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Throttle {
    /// Backend name that supplies usage data. Currently `"umans"` is the only
    /// supported value. An unknown backend is a no-op (with a stderr warning).
    pub backend: String,
    /// Tags that activate this throttle entry, intersected with active scope
    /// tags (same selection model as bundles, MCP servers, and memory).
    #[serde(default)]
    pub when: Vec<String>,
    /// How long (seconds) a fetched usage snapshot is cached on disk before
    /// the backend is polled again.
    #[serde(default = "default_throttle_cache_ttl")]
    pub cache_ttl: u64,
    /// Hard cap (seconds) on any single per-hook sleep.
    #[serde(default = "default_throttle_max_wait")]
    pub max_wait: u64,
    /// Remaining-request level at which adaptive delays begin.
    #[serde(default = "default_throttle_soft_threshold")]
    pub soft_threshold: u64,
}

/// An MCP server definition. Selected onto a scope when any of its `tags`
/// intersect the active scope tag set (identical to bundle selection).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpServer {
    /// Registration name in the agent's MCP config (e.g. `"playwright"`).
    pub name: String,
    /// Tags that activate this server, intersected with active scope tags.
    #[serde(default)]
    pub when: Vec<String>,
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

/// Marketplace names Claude Code reserves for official Anthropic marketplaces.
/// Each may only be sourced from a `github.com/anthropics/...` repository; any
/// other source is rejected by Claude Code at load time (#190).
pub const RESERVED_OFFICIAL_MARKETPLACES: &[&str] = &[
    "claude-plugins-official",
    "claude-code-plugins",
    "claude-code-marketplace",
    "anthropic-marketplace",
    "anthropic-plugins",
];

/// The GitHub organization that must own a reserved official marketplace's
/// source repository (#190).
pub const OFFICIAL_MARKETPLACE_OWNER: &str = "anthropics";

/// Whether `name` is a marketplace name reserved for an official Anthropic
/// marketplace. Reserved names carry the `anthropics`-GitHub source constraint.
#[must_use]
pub fn is_reserved_official_marketplace(name: &str) -> bool {
    RESERVED_OFFICIAL_MARKETPLACES.contains(&name)
}

/// Parse a GitHub source string into its `(owner, repo)` pair, or `None` if the
/// source is not a `github.com` repository in `owner/repo` form.
///
/// Accepts the common git URL shapes: `https://github.com/o/r[.git][/]`,
/// scp-style `git@github.com:o/r[.git]`, and `ssh://git@github.com/o/r`. Used to
/// enforce the reserved-name → `anthropics` org constraint (#190).
#[must_use]
pub fn github_owner_repo(source: &str) -> Option<(&str, &str)> {
    // Reduce every accepted shape to the "github.com<sep>owner/repo..." tail,
    // then split the first two path segments.
    let rest = source
        .strip_prefix("https://github.com/")
        .or_else(|| source.strip_prefix("http://github.com/"))
        .or_else(|| source.strip_prefix("ssh://git@github.com/"))
        .or_else(|| source.strip_prefix("git://github.com/"))
        .or_else(|| source.strip_prefix("git@github.com:"))?;

    let mut segments = rest.trim_end_matches('/').splitn(3, '/');
    let owner = segments.next().filter(|s| !s.is_empty())?;
    let repo = segments.next().filter(|s| !s.is_empty())?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// Durable per-tool state relocation (#175).
///
/// llmenv materializes each agent config into a content-hashed cache folder and
/// points `CLAUDE_CONFIG_DIR` at it; every hash change yields a fresh folder, so
/// tool state written under the config dir is lost. llmenv guarantees a stable
/// sibling state directory (name has no content hash) and emits `LLMENV_STATE_DIR`
/// pointing at it. Each [`StateTool`] additionally relocates one tool's state by
/// emitting its env var pointed at a per-tool subdirectory of that stable dir.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct StateConfig {
    /// Per-tool env-var relocations. Each emits `<env>=<state_dir>/<subdir>`.
    #[serde(default)]
    pub tools: Vec<StateTool>,
}

/// One tool's durable-state relocation: the env var the tool reads to find its
/// state, and the subdirectory of the stable state dir to point it at (#175).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct StateTool {
    /// Environment variable the tool honors to relocate its state (e.g.
    /// `CONTEXT_MODE_DATA_DIR`). Emitted alongside `CLAUDE_CONFIG_DIR`.
    pub env: String,
    /// Subdirectory under the stable state dir this tool's state lives in (e.g.
    /// `context-mode`). A single safe path component — no separators or `..`.
    pub subdir: String,
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
    pub when: Vec<String>,
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
    fn cache_defaults_to_normal_when_hashing_absent() {
        // #246: a Cache block with no `hashing` key parses to the default
        // strictness (`normal`), not loose or strict.
        let cache: Cache =
            serde_yaml::from_str("cache_dir: ~/.cache/llmenv\nsync_interval_minutes: 60\n")
                .expect("parse minimal cache");
        assert_eq!(cache.hashing, HashingMode::Normal);
        // The bare Default impl must agree with the parsed-absent behavior.
        assert_eq!(Cache::default().hashing, HashingMode::Normal);
    }

    #[test]
    fn cache_parses_each_strictness_position() {
        // #246: the single dial accepts loose|normal|strict.
        for (text, expected) in [
            ("loose", HashingMode::Loose),
            ("normal", HashingMode::Normal),
            ("strict", HashingMode::Strict),
        ] {
            let cache: Cache = serde_yaml::from_str(&format!(
                "cache_dir: ~/.cache/llmenv\nsync_interval_minutes: 60\nhashing: {text}\n"
            ))
            .expect("parse explicit cache");
            assert_eq!(cache.hashing, expected, "hashing: {text}");
        }
    }

    #[test]
    fn classify_scp_style_is_git() {
        assert_eq!(
            classify_source("git@github.com:owner/repo"),
            MarketplaceSource::Git
        );
    }

    #[test]
    fn reserved_official_marketplace_names_detected() {
        // Claude Code reserves these names for official Anthropic marketplaces
        // (#190); each can only be sourced from a github.com/anthropics repo.
        for name in [
            "claude-plugins-official",
            "claude-code-plugins",
            "claude-code-marketplace",
            "anthropic-marketplace",
            "anthropic-plugins",
        ] {
            assert!(is_reserved_official_marketplace(name), "{name} reserved");
        }
        for name in ["superpowers", "dev-commons", "claude", "my-claude-plugins"] {
            assert!(!is_reserved_official_marketplace(name), "{name} free");
        }
    }

    #[test]
    fn github_owner_repo_parses_common_forms() {
        let want = Some(("anthropics", "claude-code"));
        assert_eq!(
            github_owner_repo("https://github.com/anthropics/claude-code"),
            want
        );
        assert_eq!(
            github_owner_repo("https://github.com/anthropics/claude-code.git"),
            want
        );
        assert_eq!(
            github_owner_repo("https://github.com/anthropics/claude-code/"),
            want
        );
        assert_eq!(
            github_owner_repo("git@github.com:anthropics/claude-code.git"),
            want
        );
        assert_eq!(
            github_owner_repo("ssh://git@github.com/anthropics/claude-code"),
            want
        );
    }

    #[test]
    fn github_owner_repo_rejects_non_github_and_malformed() {
        assert_eq!(
            github_owner_repo("https://gitlab.com/anthropics/claude-code"),
            None
        );
        assert_eq!(github_owner_repo("https://github.com/anthropics"), None);
        assert_eq!(github_owner_repo("/local/path"), None);
        assert_eq!(github_owner_repo("not a url"), None);
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

        #[test]
        fn prop_github_owner_repo_roundtrip(
            owner in "[a-z0-9][a-z0-9-]{0,20}",
            repo in "[a-z0-9][a-z0-9._-]{0,20}",
        ) {
            // A canonical https github URL always parses back to its components.
            // repo strips a trailing ".git", so exclude inputs ending in it.
            prop_assume!(!repo.ends_with(".git"));
            let source = format!("https://github.com/{owner}/{repo}");
            prop_assert_eq!(github_owner_repo(&source), Some((owner.as_str(), repo.as_str())));
        }

        #[test]
        fn prop_github_owner_repo_never_panics(source in ".{0,80}") {
            // Must total over arbitrary input — it gates reserved-name enforcement.
            let _ = github_owner_repo(&source);
        }

        #[test]
        fn prop_state_config_yaml_roundtrip(
            tools in proptest::collection::vec(
                ("[A-Z][A-Z0-9_]{0,10}", "[a-z0-9][a-z0-9._-]{0,12}"),
                0..5,
            ),
        ) {
            // StateConfig survives a YAML serialize→deserialize round-trip for any
            // well-formed tool list (#175). Dedup of env names is enforced by
            // validate(), not serde, so keep generated env names distinct here.
            prop_assume!({
                let names: std::collections::HashSet<_> = tools.iter().map(|(e, _)| e).collect();
                names.len() == tools.len()
            });
            let cfg = StateConfig {
                tools: tools
                    .into_iter()
                    .map(|(env, subdir)| StateTool { env, subdir })
                    .collect(),
            };
            let yaml = serde_yaml::to_string(&cfg).expect("serialize StateConfig");
            let back: StateConfig = serde_yaml::from_str(&yaml).expect("deserialize StateConfig");
            prop_assert_eq!(cfg, back);
        }
    }
}
