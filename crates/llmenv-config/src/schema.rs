use serde::{Deserialize, Deserializer, Serialize};

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
#[serde(deny_unknown_fields)]
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
    /// context-mode built-in (token-efficiency). The counterpart to `memory`
    /// (ICM). A simple enable/disable toggle; absent means disabled.
    #[serde(default)]
    pub context_mode: Option<ContextMode>,
    /// Self-upgrade configuration (`llmenv upgrade`). Absent means defaults
    /// (release track).
    #[serde(default)]
    pub upgrade: Option<UpgradeConfig>,
    /// ReadOnce: avoid re-reading unchanged files within a TTL window.
    #[serde(default)]
    pub read_once: Option<ReadOnce>,
    /// Slippage control: guardrails against model behavior drift.
    #[serde(default)]
    pub slippage: Option<SlippageControl>,
    /// In-engine task tracker (#231): durable, agent-native "what am I
    /// working on" state, off by default.
    #[serde(default)]
    pub task_tracker: Option<TaskTracker>,
    /// codebase-memory-mcp integration (#365): tag-activated local stdio MCP
    /// entries. Each resolves independently (no "at most one active" rule
    /// like `memory` — multiple local per-project servers can coexist).
    #[serde(default)]
    pub codebase_memory: Vec<CodebaseMemory>,
}

/// Self-upgrade configuration, nested under `features.upgrade`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpgradeConfig {
    /// Upgrade track: "release" (non-prerelease) or "beta" (includes prereleases).
    #[serde(default)]
    pub track: UpgradeTrack,
}

/// Which release track to follow for `llmenv upgrade`.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeTrack {
    /// Stable releases only (non-prerelease GitHub releases).
    #[default]
    Release,
    /// All releases including prereleases (beta/rc).
    Beta,
}

impl UpgradeTrack {
    /// Return the config string for this track.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Beta => "beta",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct Config {
    /// Engine ids to skip entirely during materialization, even if the
    /// engine's binary is on `PATH` (#562). Uses the same underscore-form
    /// engine id as `native.<engine>` and the `--engine` flag (e.g.
    /// `claude_code`, `crush`) rather than an adapter's hyphenated
    /// cache-dir name (`claude-code`).
    #[serde(default)]
    pub disabled_engines: Vec<String>,
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
    /// LSP servers, selected onto scopes by tag intersection (same model as
    /// `mcp`). Engines that do not support LSP (`supports_lsp() == false`)
    /// silently ignore these entries — declaring an LSP server in a shared
    /// bundle is not an error for such engines.
    #[serde(default)]
    pub lsp: Vec<LspServer>,
    /// First-class skills declared at the top level, selected by tag intersection
    /// (same model as `mcp`/`lsp`). Skills are supported by all adapters that
    /// have a skills directory concept; adapters without one silently skip these.
    #[serde(default)]
    pub skills: Vec<SkillSource>,
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
    /// Session logging configuration. Absent → ICM transcript on (info), file off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log: Option<SessionLog>,
    /// Status line widget configuration (`statusline:` section).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline: Option<StatuslineConfig>,
}

/// Per-sink log level for session-log events. Each level includes all events
/// from the levels above it: Trace ⊃ Debug ⊃ Info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Lifecycle events, user prompts, stop, notifications, precompact.
    #[default]
    Info,
    /// Info + tool uses, tool results.
    Debug,
    /// Debug + hook stdout/stderr, hook exit codes.
    Trace,
}

impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            other => Err(serde::de::Error::custom(format!(
                "unknown log level {other:?}, expected info | debug | trace"
            ))),
        }
    }
}

/// Configuration for the file sink within `session_log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSinkConfig {
    /// Enable the JSONL file sink.
    #[serde(default)]
    pub enabled: bool,
    /// Minimum event level recorded to this file.
    #[serde(default)]
    pub level: LogLevel,
    /// Override the default path (`<state_dir>/session-log.jsonl`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Default for FileSinkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            level: LogLevel::Info,
            path: None,
        }
    }
}

/// Configuration for the ICM transcript sink within `session_log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptSinkConfig {
    /// Enable the ICM transcript sink.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Minimum event level recorded to the transcript.
    #[serde(default)]
    pub level: LogLevel,
    /// Optional reaping of the file-sink session log: transcripts older than
    /// `retention_days` are best-effort deleted during SessionStart. Absent
    /// (default) means no reaping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u64>,
}

impl Default for TranscriptSinkConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: LogLevel::Info,
            retention_days: None,
        }
    }
}

/// Where and how llmenv records session activity. `file` and `transcript` are
/// independent sinks that share the same event stream; each filters by its own
/// `level`. `max_content_bytes` applies uniformly to both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionLog {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileSinkConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<TranscriptSinkConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_bytes: Option<usize>,
}

impl Default for SessionLog {
    fn default() -> Self {
        Self {
            file: None,
            transcript: Some(TranscriptSinkConfig::default()),
            max_content_bytes: None,
        }
    }
}

impl SessionLog {
    #[must_use]
    pub fn any_sink_enabled(&self) -> bool {
        self.file.as_ref().is_some_and(|f| f.enabled)
            || self.transcript.as_ref().is_some_and(|t| t.enabled)
    }

    #[must_use]
    pub fn any_sink_wants(&self, level: LogLevel) -> bool {
        self.file
            .as_ref()
            .is_some_and(|f| f.enabled && level <= f.level)
            || self
                .transcript
                .as_ref()
                .is_some_and(|t| t.enabled && level <= t.level)
    }

    #[must_use]
    pub fn file_wants(&self, level: LogLevel) -> bool {
        self.file
            .as_ref()
            .is_some_and(|f| f.enabled && level <= f.level)
    }

    #[must_use]
    pub fn transcript_wants(&self, level: LogLevel) -> bool {
        self.transcript
            .as_ref()
            .is_some_and(|t| t.enabled && level <= t.level)
    }

    #[must_use]
    pub fn file_path(&self) -> Option<&str> {
        self.file.as_ref().and_then(|f| f.path.as_deref())
    }
}

impl<'de> serde::Deserialize<'de> for SessionLog {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_yaml::Value::deserialize(d)?;
        if !v.is_mapping() {
            let got = match &v {
                serde_yaml::Value::String(_) => {
                    "a string (the pre-3.0 bare path-string form is no longer supported); use \
                     `session_log: {{ file: true }}` (file path overridable via `path:`)"
                        .to_string()
                }
                serde_yaml::Value::Bool(_) => "a boolean".to_string(),
                serde_yaml::Value::Number(_) => "a number".to_string(),
                serde_yaml::Value::Sequence(_) => "a sequence".to_string(),
                other => format!("{other:?}"),
            };
            return Err(serde::de::Error::custom(format!(
                "session_log must be a mapping, not {got}; use \
                 `session_log: {{ file: true }}` (file path overridable via `path:`)"
            )));
        }
        #[expect(clippy::unwrap_used, reason = "guarded by is_mapping() check above")]
        let m = v.as_mapping().unwrap();

        let is_old_shape = m.iter().any(|(k, v)| {
            let key = k.as_str().unwrap_or("");
            (key == "file" || key == "transcript" || key == "verbose") && v.is_bool()
        });

        if is_old_shape {
            #[derive(Deserialize)]
            struct OldShape {
                #[serde(default)]
                file: bool,
                #[serde(default = "default_true")]
                transcript: bool,
                #[serde(default)]
                verbose: bool,
                #[serde(default)]
                path: Option<String>,
                #[serde(default)]
                max_content_bytes: Option<usize>,
            }
            let old: OldShape = serde_yaml::from_value(v).map_err(serde::de::Error::custom)?;
            let level = if old.verbose {
                LogLevel::Debug
            } else {
                LogLevel::Info
            };
            return Ok(SessionLog {
                file: old.file.then_some(FileSinkConfig {
                    enabled: true,
                    level,
                    path: old.path,
                }),
                transcript: if old.transcript {
                    Some(TranscriptSinkConfig {
                        enabled: true,
                        level,
                        retention_days: None,
                    })
                } else {
                    Some(TranscriptSinkConfig {
                        enabled: false,
                        level,
                        retention_days: None,
                    })
                },
                max_content_bytes: old.max_content_bytes,
            });
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NewShape {
            #[serde(default)]
            file: Option<FileSinkConfig>,
            #[serde(default = "default_transcript_sink")]
            transcript: Option<TranscriptSinkConfig>,
            #[serde(default)]
            max_content_bytes: Option<usize>,
        }
        let n: NewShape = serde_yaml::from_value(v).map_err(serde::de::Error::custom)?;
        Ok(SessionLog {
            file: n.file,
            transcript: n.transcript,
            max_content_bytes: n.max_content_bytes,
        })
    }
}

fn default_true() -> bool {
    true
}

fn default_transcript_sink() -> Option<TranscriptSinkConfig> {
    Some(TranscriptSinkConfig::default())
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
    /// Allow remote git operations (fetch, pull, push) on the config directory
    /// and plugin marketplaces. Set to `false` to prevent shell freezes when
    /// 1Password or other SSH credential helpers are locked. Default: `true`.
    #[serde(default = "default_remote_sync")]
    pub remote_sync: bool,
}

const fn default_remote_sync() -> bool {
    true
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            cache_dir: "~/.cache/llmenv".into(),
            sync_interval_minutes: 15,
            cache_retention_hours: Some(168), // 7 days
            hashing: HashingMode::default(),
            remote_sync: default_remote_sync(),
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
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
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
    /// LSP servers declared inside a bundle. A list — concatenates across contributors.
    /// Selected by tag intersection like `mcp`. Engines with `supports_lsp() == false`
    /// silently skip these entries (not an error).
    #[serde(default)]
    pub lsp: Vec<LspServer>,
    /// First-class skills contributed directly by this capability source, independent
    /// of any plugin. A list — concatenates across contributors. Engines with
    /// `supports_skills() == false` silently skip these entries.
    #[serde(default)]
    pub skills: Vec<SkillSource>,
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
    /// Custom/self-hosted model provider endpoints declared inside a bundle.
    /// A list — concatenates across contributors, same model as `mcp`/`lsp`.
    /// Selected by tag intersection. Engines with
    /// `supports_model_providers() == false` silently skip these entries.
    #[serde(default)]
    pub model_providers: Vec<ModelProvider>,
    /// Default model selection, keyed by an open-string role ("large",
    /// "small", etc — matches Crush's real `SelectedModelType` without
    /// hardcoding to it). Merged per-key: higher-precedence contributor
    /// wins on collision (same scalar rule as `env`/`host`), not
    /// tag-intersected like a list — there is only one default per role.
    #[serde(default)]
    pub default_models: std::collections::BTreeMap<String, ModelRef>,
}

impl Capabilities {
    /// True when no capability is declared — lets callers skip empty fragments.
    pub fn is_empty(&self) -> bool {
        self.permissions.is_empty()
            && self.hooks.is_empty()
            && self.plugins.is_empty()
            && self.mcp.is_empty()
            && self.lsp.is_empty()
            && self.skills.is_empty()
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
            && self.features.as_ref().is_none_or(|f| f.throttle.is_empty())
            && self.features.as_ref().is_none_or(|f| f.read_once.is_none())
            && self.features.as_ref().is_none_or(|f| f.slippage.is_none())
            && self
                .features
                .as_ref()
                .is_none_or(|f| f.context_mode.is_none())
            && self.features.as_ref().is_none_or(|f| f.upgrade.is_none())
            && self
                .features
                .as_ref()
                .is_none_or(|f| f.task_tracker.is_none())
            && self.host.is_empty()
            && self.model_providers.is_empty()
            && self.default_models.is_empty()
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
/// Claude's `WebFetch(domain:...)`). Appended verbatim — never translated,
/// except for the Claude Code adapter's `Write` -> `Edit` deprecation rewrite
/// (anthropics/claude-code#78817; see `normalize_deprecated_tool`).
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
///
/// `Auto` — Claude Code only honors this from user-scope settings. With
/// llmenv's single-config model (`CLAUDE_CONFIG_DIR`) it always applies.
/// The restriction only matters for Claude Code's own multi-source config
/// discovery when running without llmenv.
///
/// `Manual` — maps to the string `"manual"`, which Claude Code treats
/// equivalently to `default`. Prefer `Default` unless you specifically need
/// the `manual` identity for Claude Code compatibility.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    AcceptEdits,
    Plan,
    Default,
    BypassPermissions,
    Auto,
    DontAsk,
    Manual,
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
    #[serde(default)]
    pub content: Vec<ContentScope>,
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
pub struct ContentScope {
    pub id: String,
    pub r#match: ContentMatch,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ContentMatch {
    pub glob: String,
    pub depth: Option<usize>,
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

/// context-mode built-in feature toggle. Loaded as a Claude Code *plugin*
/// (not an MCP) because its hooks reference `${CLAUDE_PLUGIN_ROOT}`, which only
/// resolves inside the plugin system. When enabled, llmenv auto-injects the
/// context-mode marketplace + plugin, a durable `CONTEXT_MODE_DATA_DIR`, and the
/// MCP permission grant. Unlike `memory`, this is a simple toggle — context-mode
/// is a local FTS5 store with no host topology, so there is nothing to tag-scope.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct ContextMode {
    /// Whether the built-in context-mode plugin is wired up.
    #[serde(default)]
    pub enabled: bool,
}

/// In-engine task tracker (#231): a file-based task store with CLI commands,
/// injected context, and lifecycle-hook ordering enforcement. Off by default
/// — disabled means zero materialized-output change and zero hook cost.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct TaskTracker {
    /// Whether the task tracker's CLAUDE.md fragment and lifecycle hooks are
    /// active. The `llmenv task` CLI subcommands work regardless of this flag
    /// — it only gates the injected-context and hook-reminder side effects.
    #[serde(default)]
    pub enabled: bool,
}

/// Widget layout, formatting, and colour config for `llmenv statusline`
/// (`statusline:` section of `config.yaml`). See
/// `docs/superpowers/specs/2026-07-15-statusline-design.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslineConfig {
    /// One row template per rendered status line. `{widget_name}`
    /// placeholders are resolved against `widgets` or widget defaults.
    pub rows: Vec<String>,
    pub style: StatuslineStyle,
    /// Per-widget overrides. Keyed by widget name (`model`, `scopes`, ...).
    pub widgets: std::collections::BTreeMap<String, WidgetConfig>,
    /// Named icon overrides (`config_stale`, `throttle`, ...).
    pub icons: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatuslineStyle {
    pub icon_set: IconSet,
    /// Master colour switch for the whole statusline. `true` (default) lets
    /// each widget render its default (or configured) colour; `false` forces
    /// every widget to plain text regardless of per-widget `style`, on top of
    /// the `--color`/`NO_COLOR` runtime gate. The opt-out for users who want
    /// the layout without any ANSI.
    pub color: bool,
}

impl Default for StatuslineStyle {
    fn default() -> Self {
        Self {
            icon_set: IconSet::default(),
            // Colour on by default — a bare `statusline:` block should look
            // readable, not monochrome. `serde(default)` fills a missing
            // `color:` field from here, so omitting it keeps colour on.
            color: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconSet {
    #[default]
    Auto,
    Nerd,
    Simple,
    None,
}

/// Per-widget override: display format, truncation, and style.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WidgetConfig {
    pub format: Option<String>,
    pub max_len: Option<usize>,
    pub style: Option<String>,
    /// Widget-specific display mode. Consumed by widgets that offer named
    /// presets rather than a free-form `format`: `model` (`short` / `version`
    /// / `full`) and `pr` (`number` / `url`). Ignored by widgets without a
    /// display mode, and overridden by `format` when both are set.
    pub display: Option<String>,
    /// Bar cell width (default 10) for `context`/`cache_usage`/`usage_5h`/
    /// `usage_7d`. Ignored by other widgets.
    pub width: Option<u8>,
    /// Two ascending percentages `[warn, crit]` for value-based coloring
    /// (green < warn ≤ yellow < crit ≤ red). Used by `context`/`usage_5h`/
    /// `usage_7d`. Ignored by widgets without threshold coloring.
    pub thresholds: Option<[u8; 2]>,
}

/// ReadOnce mode: what happens when a file that was already read is requested
/// again within the TTL window.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnceMode {
    /// Emit a warning that the file was already read (default).
    #[default]
    Warn,
    /// Deny re-reading the file entirely.
    Deny,
}

/// ReadOnce: avoid re-reading files that haven't changed within a TTL window.
/// Opt-in (disabled by default).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadOnce {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: ReadOnceMode,
    /// Session-cache TTL fallback in seconds (default 1200 = 20 min).
    #[serde(default = "default_read_once_ttl")]
    pub ttl_seconds: u64,
}

const fn default_read_once_ttl() -> u64 {
    1200
}

/// SlippageControl: guardrails against model behavior drift.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SlippageControl {
    #[serde(default)]
    pub enabled: bool,
    /// Effort level injected into generated settings (e.g. "xhigh", "high").
    #[serde(default)]
    pub effort_level: Option<String>,
    #[serde(default = "default_slippage_true")]
    pub rule_reinjection: bool,
    #[serde(default = "default_slippage_true")]
    pub read_before_edit: bool,
    #[serde(default = "default_slippage_true")]
    pub self_critique: bool,
    #[serde(default = "default_slippage_true")]
    pub metrics: bool,
    #[serde(default = "default_slippage_true")]
    pub compact_survival: bool,
    #[serde(default = "default_slippage_true")]
    pub diagnose_command: bool,
    #[serde(default)]
    pub explain_before_act: bool,
    #[serde(default)]
    pub answer_before_act: bool,
}

const fn default_slippage_true() -> bool {
    true
}

impl Default for SlippageControl {
    fn default() -> Self {
        Self {
            enabled: false,
            effort_level: None,
            rule_reinjection: true,
            read_before_edit: true,
            self_critique: true,
            metrics: true,
            compact_survival: true,
            diagnose_command: true,
            explain_before_act: false,
            answer_before_act: false,
        }
    }
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
    /// LLM backend to use for consolidation.
    #[serde(default)]
    pub backend: ConsolidationBackend,
    /// Maximum number of semantic rules to distill per session.
    #[serde(default = "default_max_rules")]
    pub max_rules_per_session: u32,
}

const fn default_max_rules() -> u32 {
    10
}

/// LLM backend for post-session consolidation.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ConsolidationBackend {
    /// Use `claude -p` subprocess (works with Claude subscription, no API key needed).
    #[default]
    #[serde(rename = "claude-cli")]
    ClaudeCli,
    /// Use Anthropic Messages API directly (requires `ANTHROPIC_API_KEY` env var).
    #[serde(rename = "anthropic-api")]
    AnthropicApi,
}

/// Per-type retention policy for memory pruning (R4).
///
/// Each field is a `humantime`-parseable duration string (e.g. `"30d"`,
/// `"365d"`, `"90 days"`). A value of `None` means "never prune" for that
/// type.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RetentionConfig {
    /// Retention for episodic memories. Defaults to `"30d"`.
    #[serde(default = "default_retention_episodic")]
    pub episodic: Option<String>,
    /// Retention for semantic memories. Defaults to `None` (never prune).
    #[serde(default)]
    pub semantic: Option<String>,
    /// Retention for procedural memories. Defaults to `"365d"`.
    #[serde(default = "default_retention_procedural")]
    pub procedural: Option<String>,
}

fn default_retention_episodic() -> Option<String> {
    Some("30d".to_string())
}

fn default_retention_procedural() -> Option<String> {
    Some("365d".to_string())
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
    /// Per-type retention policy for memory pruning (R4). When `None`,
    /// pruning is fully disabled. When `Some(Config)`, the per-type
    /// duration strings drive `llmenv memory prune`.
    #[serde(default)]
    pub retention: Option<RetentionConfig>,
    /// Whether to automatically run `llmenv memory prune` during
    /// `llmenv materialize`. Defaults to `false` — opt-in.
    #[serde(default)]
    pub auto_prune: bool,
}

/// A local, tag-activated `codebase-memory-mcp` server. Unlike `Memory`
/// (ICM), this resolves to a **local stdio** MCP entry, not a network
/// client — codebase-memory-mcp has no remote-serve mode. `CBM_CACHE_DIR`
/// and `CBM_ALLOWED_ROOT` are always computed by llmenv (state dir +
/// project root), never user-configurable, so a declared entry can't
/// accidentally scope the indexer outside the intended project.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CodebaseMemory {
    /// Tags that activate this server, intersected with active scope tags
    /// (same selection model as `mcp`/`memory`).
    #[serde(default)]
    pub when: Vec<String>,
    /// Override the index storage directory (`CBM_CACHE_DIR` env var).
    /// Defaults to `<state_dir>/codebase-memory` when unset — a shared cache
    /// root, matching upstream's own default (`~/.cache/codebase-memory-mcp/`
    /// stores every project's index together; the server partitions by
    /// project internally, the same way `list_projects` enumerates them).
    #[serde(default)]
    pub index_path: Option<String>,
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
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
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
    /// HTTP request headers injected for `http`/`sse` transport (e.g. auth tokens).
    /// Silently ignored for `stdio` servers.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// When `true` the server is excluded from the resolved set for all engines.
    #[serde(default)]
    pub disabled: bool,
    /// Tool names the engine should hide from the model for this server.
    /// #506: consumed by CrushAdapter when it renders its MCP config.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// Per-server request timeout in seconds. `None` means use the engine default.
    /// #506: consumed by CrushAdapter when it renders its MCP config.
    #[serde(default)]
    pub timeout: Option<u32>,
}

/// A first-class skill contributed directly by config or bundle, independent of
/// any plugin. Claude Code loads skills from its `skills/` directory; this entry
/// declares one skill's source directory and the scope tags that activate it.
///
/// Selected onto a scope when any of its `when` tags intersect the active scope
/// tag set (identical to `McpServer` selection). Adapters without a skills
/// directory concept silently skip these entries — declaring a skill in a shared
/// bundle is legitimate; it is a no-op for such adapters.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct SkillSource {
    /// Registration name for the skill (written as the directory name under `skills/`).
    pub name: String,
    /// Absolute path to the skill's source directory (must contain `SKILL.md`).
    pub path: String,
    /// Scope tags that activate this skill, intersected with active scope tags.
    /// An empty list means the entry is always active when selected.
    #[serde(default)]
    pub when: Vec<String>,
}

/// Engines that report `supports_lsp() == false` silently ignore these entries —
/// declaring an LSP server in a shared bundle is legitimate; it is simply a
/// no-op for engines that have no LSP concept.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct LspServer {
    /// Registration name for the language server (e.g. `"rust-analyzer"`).
    pub name: String,
    /// Tags that activate this server, intersected with active scope tags.
    /// An empty list means the entry is always active (no tag filtering applied).
    #[serde(default)]
    pub when: Vec<String>,
    /// Executable to launch (absolute path or name resolved via `PATH`).
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables injected into the server process.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// When `true` the server is excluded from the resolved set for all engines.
    #[serde(default)]
    pub disabled: bool,
    /// File-type / language identifiers this server handles (e.g. `["rust"]`).
    /// Engines use these to associate the server with open buffers.
    #[serde(default)]
    pub filetypes: Vec<String>,
    /// Filenames or patterns that mark the root of a workspace (e.g.
    /// `["Cargo.toml", ".git"]`). Engines use these to anchor the project root.
    #[serde(default)]
    pub root_markers: Vec<String>,
    /// Opaque `initializationOptions` passed verbatim to the language server
    /// during the LSP `initialize` handshake. Engine adapters forward this as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_options: Option<serde_yaml::Value>,
    /// Per-server request timeout in seconds. `None` means use the engine default.
    #[serde(default)]
    pub timeout: Option<u32>,
    /// File extension → language identifier (e.g. `{".rs": "rust"}`).
    /// #556: Claude Code's plugin `lspServers` schema requires an
    /// `extensionToLanguage` map (it has no `filetypes`-style language-id list);
    /// consumed only by `ClaudeCodeAdapter`. A server without this mapping is
    /// skipped (with a warning) when rendering for Claude Code, since `filetypes`
    /// language ids don't reliably convert to file extensions (e.g. `rust` → `.rs`).
    #[serde(default)]
    pub extension_to_language: std::collections::BTreeMap<String, String>,
}

/// A custom/self-hosted model provider endpoint (Ollama, vLLM, LM Studio, a
/// proxy, or an override of a built-in provider). Selected by tag
/// intersection like `mcp`/`lsp`/`skills`. Engines without a multi-provider
/// concept (`supports_model_providers() == false`) silently skip these —
/// declaring one in a shared bundle is legitimate; it is simply a no-op for
/// such adapters.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct ModelProvider {
    /// Stable identifier, used as the map key when rendered (e.g. "ollama",
    /// "my-proxy") and as the `provider` field target of `ModelRef`.
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// Tags that activate this provider, intersected with active scope tags.
    #[serde(default)]
    pub when: Vec<String>,
    /// API endpoint URL.
    pub base_url: Option<String>,
    /// Wire format, e.g. "openai", "anthropic", "google". Open string, not
    /// an enum — new wire formats appear faster than llmenv releases.
    pub api_type: Option<String>,
    /// Passthrough credential string — may be a literal, or a $VAR/!command
    /// reference the *target engine* resolves at its own runtime. llmenv
    /// never interprets this value (resolving it here would write a
    /// plaintext secret into the materialized cache directory).
    pub api_key: Option<String>,
    /// Extra HTTP headers, passthrough (same rationale as `api_key`).
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// When `true` the provider is excluded from the resolved set for all engines.
    #[serde(default)]
    pub disabled: bool,
    /// Models exposed by this provider.
    #[serde(default)]
    pub models: Vec<ModelSource>,
}

/// One model exposed by a `ModelProvider`. All fields but `id` are optional
/// — mirrors Pi's "only `id` is required for local models" convention.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct ModelSource {
    pub id: String,
    pub name: Option<String>,
    /// Supports extended thinking/reasoning.
    #[serde(default)]
    pub reasoning: bool,
    /// Context window size in tokens.
    pub context_window: Option<u32>,
    /// Maximum output tokens.
    pub max_tokens: Option<u32>,
    /// Cost per million tokens.
    pub cost: Option<ModelCost>,
    /// Input modalities, e.g. `["text"]` or `["text", "image"]`.
    #[serde(default)]
    pub modalities: Vec<String>,
}

/// Cost per million tokens, matching the near-identical shape used by Crush,
/// Pi, and OpenCode's own model schemas.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Default)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

/// A pointer to a model+provider pair, used for default-model selection.
/// `provider` may be a `ModelProvider.id` declared alongside it, or an
/// engine builtin id (e.g. Crush's built-in `"anthropic"`) that llmenv has
/// no knowledge of and does not validate against.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
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

    fn arb_level() -> impl Strategy<Value = LogLevel> {
        prop_oneof![
            Just(LogLevel::Info),
            Just(LogLevel::Debug),
            Just(LogLevel::Trace),
        ]
    }

    #[test]
    fn codebase_memory_round_trips_through_yaml() {
        let yaml = r#"
when: [my-project]
index_path: /custom/index/path
"#;
        let cm: CodebaseMemory = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cm.when, vec!["my-project".to_string()]);
        assert_eq!(cm.index_path.as_deref(), Some("/custom/index/path"));

        let cm_defaults: CodebaseMemory = serde_yaml::from_str("when: [x]").unwrap();
        assert_eq!(cm_defaults.index_path, None);
    }

    #[test]
    fn features_codebase_memory_defaults_to_empty() {
        let features: Features = serde_yaml::from_str("{}").unwrap();
        assert!(features.codebase_memory.is_empty());
    }

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
    fn session_log_serialize_deserialize_roundtrips() {
        let original = SessionLog {
            file: Some(FileSinkConfig {
                enabled: true,
                level: LogLevel::Debug,
                path: Some("/tmp/log.jsonl".into()),
            }),
            transcript: Some(TranscriptSinkConfig {
                enabled: false,
                level: LogLevel::Trace,
                retention_days: None,
            }),
            max_content_bytes: Some(1024),
        };
        let yaml = serde_yaml::to_string(&original).expect("serialize");
        let back: SessionLog = serde_yaml::from_str(&yaml).expect("deserialize");
        assert_eq!(original, back);
    }

    proptest! {
        #[test]
        fn prop_session_log_yaml_roundtrip(
            file in prop::option::of((proptest::bool::ANY, arb_level(), prop::option::of("[a-zA-Z0-9/_.-]{1,32}"))),
            (transcript_enabled, transcript_level) in (proptest::bool::ANY, arb_level()),
            max_content_bytes in prop::option::of(0usize..1_000_000),
        ) {
            let original = SessionLog {
                file: file.map(|(enabled, level, path)| FileSinkConfig { enabled, level, path }),
                transcript: Some(TranscriptSinkConfig { enabled: transcript_enabled, level: transcript_level, retention_days: None }),
                max_content_bytes,
            };
            let yaml = serde_yaml::to_string(&original).expect("serialize");
            let back: SessionLog = serde_yaml::from_str(&yaml).expect("deserialize");
            prop_assert_eq!(original, back);
        }
    }

    #[test]
    fn session_log_rejects_bare_string_with_migration_hint() {
        let err = serde_yaml::from_str::<SessionLog>("some/path.jsonl").unwrap_err();
        assert!(err.to_string().contains("pre-3.0 bare path-string form"));
    }

    #[test]
    fn session_log_rejects_non_mapping_with_type_specific_message() {
        // #509 item 4: a bool/number value must not be described as "not a path
        // string" — that message is misleading when the value isn't a string.
        let bool_err = serde_yaml::from_str::<SessionLog>("true").unwrap_err();
        assert!(bool_err.to_string().contains("a boolean"));
        assert!(!bool_err.to_string().contains("path string"));

        let num_err = serde_yaml::from_str::<SessionLog>("5").unwrap_err();
        assert!(num_err.to_string().contains("a number"));
        assert!(!num_err.to_string().contains("path string"));
    }

    #[test]
    fn session_log_error_carries_config_file_path() {
        // #509 item 6: the migration-hint error must be locatable in a
        // multi-file setup. Config::load already wraps every parse error with
        // the file path (crates/llmenv-config/src/lib.rs); confirm that wrap
        // covers this custom Deserialize impl's error too.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "session_log: true\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains(&path.display().to_string()));
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

        // ===== 3.0.0 release PBT backfill =====

        #[test]
        fn prop_memory_type_serde_roundtrip(
            v in prop_oneof![
                Just(MemoryType::Episodic),
                Just(MemoryType::Semantic),
                Just(MemoryType::Procedural),
            ],
        ) {
            let json = serde_json::to_string(&v).expect("serialize MemoryType");
            let back: MemoryType = serde_json::from_str(&json).expect("deserialize MemoryType");
            prop_assert_eq!(v, back);
        }

        #[test]
        fn prop_importance_level_serde_roundtrip(
            v in prop_oneof![
                Just(ImportanceLevel::Low),
                Just(ImportanceLevel::Medium),
                Just(ImportanceLevel::High),
                Just(ImportanceLevel::Critical),
            ],
        ) {
            let json = serde_json::to_string(&v).expect("serialize ImportanceLevel");
            let back: ImportanceLevel =
                serde_json::from_str(&json).expect("deserialize ImportanceLevel");
            prop_assert_eq!(v, back);
        }

        #[test]
        fn prop_consolidation_config_serde_roundtrip(
            enabled: bool,
            max_rules_per_session in 0u32..100,
        ) {
            let cfg = ConsolidationConfig {
                enabled,
                backend: Default::default(),
                max_rules_per_session,
            };
            let json = serde_json::to_string(&cfg).expect("serialize ConsolidationConfig");
            let back: ConsolidationConfig =
                serde_json::from_str(&json).expect("deserialize ConsolidationConfig");
            prop_assert_eq!(cfg, back);
        }
    }

    #[test]
    fn context_mode_parses_enabled() {
        let yaml = "features:\n  context_mode:\n    enabled: true\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let cm = cfg.features.unwrap().context_mode.unwrap();
        assert!(cm.enabled);
    }

    #[test]
    fn context_mode_absent_is_none() {
        let cfg: Config = serde_yaml::from_str("features:\n  memory: []\n").unwrap();
        assert!(cfg.features.unwrap().context_mode.is_none());
    }

    #[test]
    fn context_mode_default_disabled() {
        let yaml = "features:\n  context_mode: {}\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.features.unwrap().context_mode.unwrap().enabled);
    }

    // ===== ReadOnce config tests =====

    #[test]
    fn read_once_parses_enabled() {
        let yaml = "features:\n  read_once:\n    enabled: true\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let ro = cfg.features.unwrap().read_once.unwrap();
        assert!(ro.enabled);
        assert_eq!(ro.mode, ReadOnceMode::Warn);
        assert_eq!(ro.ttl_seconds, 1200);
    }

    #[test]
    fn read_once_parses_mode() {
        let yaml = "features:\n  read_once:\n    mode: deny\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let ro = cfg.features.unwrap().read_once.unwrap();
        assert_eq!(ro.mode, ReadOnceMode::Deny);
    }

    #[test]
    fn read_once_absent_is_none() {
        let cfg: Config = serde_yaml::from_str("features:\n  memory: []\n").unwrap();
        assert!(cfg.features.unwrap().read_once.is_none());
    }

    #[test]
    fn read_once_defaults_disabled() {
        let yaml = "features:\n  read_once: {}\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let ro = cfg.features.unwrap().read_once.unwrap();
        assert!(!ro.enabled);
        assert_eq!(ro.mode, ReadOnceMode::Warn);
        assert_eq!(ro.ttl_seconds, 1200);
    }

    // ===== SlippageControl config tests =====

    #[test]
    fn slippage_parses_enabled() {
        let yaml = "features:\n  slippage:\n    enabled: true\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let sc = cfg.features.unwrap().slippage.unwrap();
        assert!(sc.enabled);
        // All default-true fields default to true
        assert!(sc.rule_reinjection);
        assert!(sc.read_before_edit);
        assert!(sc.self_critique);
        assert!(sc.metrics);
        assert!(sc.compact_survival);
        assert!(sc.diagnose_command);
        // Default-false fields
        assert!(!sc.explain_before_act);
        assert!(!sc.answer_before_act);
        assert_eq!(sc.effort_level, None);
    }

    #[test]
    fn slippage_parses_effort_level() {
        let yaml = "features:\n  slippage:\n    effort_level: xhigh\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let sc = cfg.features.unwrap().slippage.unwrap();
        assert_eq!(sc.effort_level.as_deref(), Some("xhigh"));
        assert!(!sc.enabled);
    }

    #[test]
    fn slippage_absent_is_none() {
        let cfg: Config = serde_yaml::from_str("features:\n  memory: []\n").unwrap();
        assert!(cfg.features.unwrap().slippage.is_none());
    }

    #[test]
    fn slippage_defaults() {
        let yaml = "features:\n  slippage: {}\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let sc = cfg.features.unwrap().slippage.unwrap();
        assert!(!sc.enabled);
        assert!(sc.rule_reinjection);
        assert!(sc.read_before_edit);
        assert!(sc.self_critique);
        assert!(sc.metrics);
        assert!(sc.compact_survival);
        assert!(sc.diagnose_command);
        assert!(!sc.explain_before_act);
        assert!(!sc.answer_before_act);
        assert_eq!(sc.effort_level, None);
    }

    // ===== TaskTracker config tests =====

    #[test]
    fn task_tracker_parses_enabled() {
        let yaml = "features:\n  task_tracker:\n    enabled: true\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.features.unwrap().task_tracker.unwrap().enabled);
    }

    #[test]
    fn task_tracker_absent_is_none() {
        let cfg: Config = serde_yaml::from_str("features:\n  memory: []\n").unwrap();
        assert!(cfg.features.unwrap().task_tracker.is_none());
    }

    #[test]
    fn task_tracker_default_disabled() {
        let yaml = "features:\n  task_tracker: {}\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.features.unwrap().task_tracker.unwrap().enabled);
    }

    /// The manual `Default` impl for SlippageControl must stay in sync with
    /// serde defaults — if a field is added to the struct with a serde default
    /// but the manual impl isn't updated, they silently diverge.
    #[test]
    fn slippage_default_matches_serde_empty() {
        let from_serde: SlippageControl =
            serde_json::from_str("{}").expect("empty object should deserialize");
        let from_default = SlippageControl::default();
        assert_eq!(
            from_default, from_serde,
            "manual Default impl diverges from serde defaults for SlippageControl"
        );
    }

    // ===== Feature round-trip tests =====

    #[test]
    fn features_roundtrip_read_once() {
        let original = Features {
            read_once: Some(ReadOnce {
                enabled: true,
                mode: ReadOnceMode::Deny,
                ttl_seconds: 600,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Features = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn features_roundtrip_slippage() {
        let original = Features {
            slippage: Some(SlippageControl {
                enabled: true,
                effort_level: Some("xhigh".to_string()),
                rule_reinjection: false,
                read_before_edit: true,
                self_critique: false,
                metrics: true,
                compact_survival: false,
                diagnose_command: true,
                explain_before_act: false,
                answer_before_act: false,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Features = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn features_roundtrip_task_tracker() {
        let original = Features {
            task_tracker: Some(TaskTracker { enabled: true }),
            ..Default::default()
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Features = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    // #505: MCP field parity — new optional fields

    /// A McpServer with all new fields set survives YAML and JSON round-trips with
    /// values intact.
    #[test]
    fn mcp_server_new_fields_roundtrip_yaml() {
        use std::collections::BTreeMap;
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer tok".to_string());
        let original = McpServer {
            name: "ctx7".to_string(),
            when: vec!["tag".to_string()],
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("https://ctx7.example/mcp".to_string()),
            headers,
            disabled: false,
            disabled_tools: vec!["dangerous_tool".to_string()],
            timeout: Some(30),
        };
        let yaml = serde_yaml::to_string(&original).unwrap();
        let parsed: McpServer = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn mcp_server_new_fields_roundtrip_json() {
        use std::collections::BTreeMap;
        let mut headers = BTreeMap::new();
        headers.insert("X-Api-Key".to_string(), "secret".to_string());
        let original = McpServer {
            name: "playwright".to_string(),
            when: vec![],
            transport: McpTransport::Stdio,
            command: Some("npx".to_string()),
            args: vec!["-y".to_string()],
            env: BTreeMap::new(),
            url: None,
            headers,
            disabled: true,
            disabled_tools: vec!["snap".to_string(), "click".to_string()],
            timeout: Some(120),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: McpServer = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    /// A McpServer YAML without any of the new fields parses with defaults applied:
    /// `headers` empty, `disabled` false, `disabled_tools` empty, `timeout` None.
    #[test]
    fn mcp_server_back_compat_missing_new_fields_parse_as_defaults() {
        let yaml = "name: playwright\ncommand: npx\nargs:\n  - -y\n";
        let parsed: McpServer = serde_yaml::from_str(yaml).unwrap();
        assert!(parsed.headers.is_empty(), "headers must default to empty");
        assert!(!parsed.disabled, "disabled must default to false");
        assert!(
            parsed.disabled_tools.is_empty(),
            "disabled_tools must default to empty"
        );
        assert_eq!(parsed.timeout, None, "timeout must default to None");
    }

    /// Two identical McpServers (incl new fields) dedup to one; differing timeout
    /// means two entries survive.
    #[test]
    fn mcp_server_dedup_respects_new_fields() {
        use std::collections::BTreeMap;
        let a = McpServer {
            name: "ctx".to_string(),
            when: vec![],
            transport: McpTransport::Stdio,
            command: Some("ctx-mcp".to_string()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            disabled: false,
            disabled_tools: vec![],
            timeout: Some(30),
        };
        let mut b = a.clone();
        // Same: dedup to one.
        let mut list = vec![a.clone(), b.clone()];
        list.dedup();
        assert_eq!(list.len(), 1, "identical servers must dedup to one");
        // Different timeout: both survive.
        b.timeout = Some(60);
        let mut list2 = vec![a, b];
        list2.dedup();
        assert_eq!(list2.len(), 2, "differing timeout must not dedup");
    }

    // #503: LSP capability schema tests

    /// An LspServer with all fields set survives a YAML round-trip with values intact.
    #[test]
    fn lsp_server_roundtrip_yaml() {
        use std::collections::BTreeMap;
        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".to_string(), "info".to_string());
        let mut extension_to_language = BTreeMap::new();
        extension_to_language.insert(".rs".to_string(), "rust".to_string());
        let original = LspServer {
            name: "rust-analyzer".to_string(),
            when: vec!["rust".to_string()],
            command: "rust-analyzer".to_string(),
            args: vec!["--no-default-features".to_string()],
            env,
            disabled: false,
            filetypes: vec!["rust".to_string()],
            root_markers: vec!["Cargo.toml".to_string(), ".git".to_string()],
            init_options: Some(serde_yaml::from_str("checkOnSave:\n  command: clippy\n").unwrap()),
            timeout: Some(30),
            extension_to_language,
        };
        let yaml = serde_yaml::to_string(&original).unwrap();
        let parsed: LspServer = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(original, parsed);
    }

    /// A config/bundle YAML with NO `lsp:` key parses — lsp defaults to empty.
    /// This is the critical back-compat invariant: no existing config breaks.
    #[test]
    fn lsp_defaults_to_empty_when_absent() {
        let yaml = "name: playwright\ncommand: npx\nargs:\n  - -y\n";
        // Config-level: no lsp key
        let cfg_yaml = "cache:\n  path: /tmp/x\n";
        let cfg: Config = serde_yaml::from_str(cfg_yaml).unwrap();
        assert!(cfg.lsp.is_empty(), "Config.lsp must default to empty");
        // Capabilities-level: no lsp key
        let caps_yaml = "hooks: []\n";
        let caps: Capabilities = serde_yaml::from_str(caps_yaml).unwrap();
        assert!(
            caps.lsp.is_empty(),
            "Capabilities.lsp must default to empty"
        );
        // LspServer back-compat: minimal fields only
        let server_yaml = "name: rust-analyzer\ncommand: rust-analyzer\n";
        let server: LspServer = serde_yaml::from_str(server_yaml).unwrap();
        assert!(server.when.is_empty(), "when must default to empty");
        assert!(server.args.is_empty(), "args must default to empty");
        assert!(server.env.is_empty(), "env must default to empty");
        assert!(!server.disabled, "disabled must default to false");
        assert!(
            server.filetypes.is_empty(),
            "filetypes must default to empty"
        );
        assert!(
            server.root_markers.is_empty(),
            "root_markers must default to empty"
        );
        assert!(
            server.init_options.is_none(),
            "init_options must default to None"
        );
        assert_eq!(server.timeout, None, "timeout must default to None");
        assert!(
            server.extension_to_language.is_empty(),
            "extension_to_language must default to empty"
        );
        // Suppress unused variable warning from yaml binding
        let _ = yaml;
    }

    /// Capabilities::is_empty() returns false when lsp is non-empty.
    #[test]
    fn capabilities_is_empty_false_with_lsp() {
        let caps = Capabilities {
            lsp: vec![LspServer {
                name: "rust-analyzer".to_string(),
                command: "rust-analyzer".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(
            !caps.is_empty(),
            "is_empty must be false when lsp is non-empty"
        );
    }

    /// Capabilities::is_empty() returns false when task_tracker is set — a
    /// merge-gate regression: `merge()` skips adding the top-level contributor
    /// entirely when `is_empty()` is true, so any field consumed only via the
    /// merged manifest (not read from raw `Config`) must be listed here.
    #[test]
    fn capabilities_is_empty_false_with_task_tracker() {
        let caps = Capabilities {
            features: Some(Features {
                task_tracker: Some(TaskTracker { enabled: true }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            !caps.is_empty(),
            "is_empty must be false when task_tracker is set"
        );
    }

    /// Variant of the task_tracker merge-gate bug: `throttle`/`context_mode`/
    /// `upgrade` are also consumed only via the merged manifest and had the
    /// same omission — found during #231's pre-pr-review.
    #[test]
    fn capabilities_is_empty_false_with_throttle() {
        let caps = Capabilities {
            features: Some(Features {
                throttle: vec![Throttle {
                    backend: "umans".to_string(),
                    when: vec![],
                    cache_ttl: 60,
                    max_wait: 5,
                    soft_threshold: 10,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            !caps.is_empty(),
            "is_empty must be false when throttle is set"
        );
    }

    #[test]
    fn capabilities_is_empty_false_with_context_mode() {
        let caps = Capabilities {
            features: Some(Features {
                context_mode: Some(ContextMode { enabled: true }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            !caps.is_empty(),
            "is_empty must be false when context_mode is set"
        );
    }

    #[test]
    fn capabilities_is_empty_false_with_upgrade() {
        let caps = Capabilities {
            features: Some(Features {
                upgrade: Some(UpgradeConfig {
                    track: UpgradeTrack::default(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            !caps.is_empty(),
            "is_empty must be false when upgrade is set"
        );
    }

    #[test]
    fn model_provider_yaml_roundtrip() {
        let yaml = r#"
id: ollama
name: Local Ollama
base_url: http://localhost:11434/v1
api_type: openai
api_key: "$OLLAMA_KEY"
headers:
  x-custom: value
models:
  - id: llama3.1:8b
    name: Llama 3.1 8B
    reasoning: false
    context_window: 128000
    max_tokens: 32000
    cost:
      input: 0.0
      output: 0.0
    modalities:
      - text
"#;
        let provider: ModelProvider = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(provider.id, "ollama");
        assert_eq!(
            provider.base_url,
            Some("http://localhost:11434/v1".to_string())
        );
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].id, "llama3.1:8b");
        assert_eq!(
            provider.models[0].cost,
            Some(ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: None,
                cache_write: None
            })
        );
    }

    #[test]
    fn model_provider_only_id_required() {
        // Mirrors Pi's "only id is required for local models" convention.
        let yaml = "id: bare-provider\n";
        let provider: ModelProvider = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(provider.id, "bare-provider");
        assert_eq!(provider.name, None);
        assert!(provider.models.is_empty());
    }

    #[test]
    fn default_models_map_yaml_roundtrip() {
        let yaml = r#"
large:
  provider: anthropic
  model: claude-opus-4-7
small:
  provider: ollama
  model: llama3.1:8b
"#;
        let map: std::collections::BTreeMap<String, ModelRef> =
            serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(map["large"].provider, "anthropic");
        assert_eq!(map["small"].model, "llama3.1:8b");
    }

    // ---- PermissionMode serde roundtrip ----

    #[test]
    fn permission_mode_serde_roundtrip() {
        for (mode, expected) in [
            (PermissionMode::AcceptEdits, "acceptEdits"),
            (PermissionMode::Plan, "plan"),
            (PermissionMode::Default, "default"),
            (PermissionMode::BypassPermissions, "bypassPermissions"),
            (PermissionMode::Auto, "auto"),
            (PermissionMode::DontAsk, "dontAsk"),
            (PermissionMode::Manual, "manual"),
        ] {
            let json = serde_json::to_string(&mode).expect("serialize PermissionMode");
            assert_eq!(json, format!("\"{expected}\""), "serialize {mode:?}");
            let back: PermissionMode =
                serde_json::from_str(&json).expect("deserialize PermissionMode");
            assert_eq!(back, mode, "roundtrip {mode:?}");
        }
    }

    #[test]
    fn statusline_config_parses_full_example() {
        let yaml = r#"
rows:
  - "{model} │ {context_pct} │ {budget}"
  - "{scopes:t} · {plugins} {config_stale}"
style:
  icon_set: auto
widgets:
  model:
    format: "{short_name} {version}"
    style: "bold cyan"
  scopes:
    format: "║ {tags}"
    max_len: 40
    style: "dim"
icons:
  config_ok: ""
  config_stale: "◌"
"#;
        let cfg: StatuslineConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.rows.len(), 2);
        assert_eq!(cfg.style.icon_set, IconSet::Auto);
        let model = cfg.widgets.get("model").unwrap();
        assert_eq!(model.format.as_deref(), Some("{short_name} {version}"));
        assert_eq!(model.style.as_deref(), Some("bold cyan"));
        let scopes = cfg.widgets.get("scopes").unwrap();
        assert_eq!(scopes.max_len, Some(40));
        assert_eq!(cfg.icons.get("config_stale").map(String::as_str), Some("◌"));
    }

    #[test]
    fn statusline_config_defaults_on_empty_yaml() {
        let cfg: StatuslineConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.rows.is_empty());
        assert_eq!(cfg.style.icon_set, IconSet::Auto);
        assert!(cfg.widgets.is_empty());
        assert!(cfg.icons.is_empty());
    }

    #[test]
    fn statusline_config_rejects_unknown_top_level_key() {
        let err = serde_yaml::from_str::<StatuslineConfig>("widgest:\n  model: {}\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn statusline_style_rejects_unknown_key() {
        let err = serde_yaml::from_str::<StatuslineStyle>("icon_est: nerd\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn widget_config_rejects_unknown_key() {
        let err = serde_yaml::from_str::<WidgetConfig>("max_length: 10\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }
}
