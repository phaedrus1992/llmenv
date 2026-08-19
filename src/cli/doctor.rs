use crate::config::{Bundle, Capabilities, Config};
use crate::paths;
use crate::plugins::cache;
use anyhow::Context;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Effective value of a token-efficiency env var: the process environment
/// wins if set (matches what Claude Code will actually see if it inherited
/// the shell), otherwise fall back to `native.claude_code.env` in the
/// resolved config — a var declared there lands in settings.json's own `env`
/// block, which Claude Code applies to itself independent of the shell that
/// launched it, so it counts as "set" even when the shell never exported it.
fn effective_token_efficiency_var(
    native_claude_env: Option<&serde_yaml::Value>,
    key: &str,
) -> Option<String> {
    if let Ok(val) = std::env::var(key) {
        return Some(val);
    }
    let value = native_claude_env?.get(key)?;
    value
        .as_str()
        .map(String::from)
        .or_else(|| value.as_bool().map(|b| b.to_string()))
        .or_else(|| value.as_i64().map(|n| n.to_string()))
}

fn run_doctor_token_efficiency(
    use_color: bool,
    pass: &str,
    warn: &str,
    cm_enabled: bool,
    native_claude_env: Option<&serde_yaml::Value>,
) {
    let info = super::doctor_info(use_color);
    eprintln!();
    eprintln!("Token-efficiency checks:");
    let get = |key: &str| effective_token_efficiency_var(native_claude_env, key);

    match get("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE") {
        Some(val) => match val.parse::<u32>() {
            Ok(pct) if pct <= 70 => eprintln!("{pass} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct}"),
            Ok(pct) => eprintln!(
                "{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE={pct} (recommend ≤70 for PreCompact cleanup)"
            ),
            Err(_) => {
                eprintln!("{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE has invalid (non-numeric) value")
            }
        },
        None => eprintln!(
            "{warn} CLAUDE_AUTOCOMPACT_PCT_OVERRIDE not set (recommend 50 for PreCompact headroom)"
        ),
    }

    match get("BASH_MAX_OUTPUT_LENGTH").map(|v| v.parse::<u64>()) {
        Some(Ok(n)) => eprintln!("{pass} BASH_MAX_OUTPUT_LENGTH={n}"),
        Some(Err(_)) => eprintln!("{warn} BASH_MAX_OUTPUT_LENGTH has invalid (non-numeric) value"),
        None => eprintln!("{warn} BASH_MAX_OUTPUT_LENGTH not set (recommend 10000)"),
    }

    match get("MAX_MCP_OUTPUT_TOKENS").map(|v| v.parse::<u64>()) {
        Some(Ok(n)) => eprintln!("{pass} MAX_MCP_OUTPUT_TOKENS={n}"),
        Some(Err(_)) => eprintln!("{warn} MAX_MCP_OUTPUT_TOKENS has invalid (non-numeric) value"),
        None => eprintln!("{warn} MAX_MCP_OUTPUT_TOKENS not set (recommend 10000)"),
    }

    match get("ENABLE_PROMPT_CACHING_1H") {
        Some(val) if val.eq_ignore_ascii_case("true") || val == "1" => {
            eprintln!("{pass} ENABLE_PROMPT_CACHING_1H=true (1h cache TTL enabled)")
        }
        Some(_) => {
            eprintln!("{warn} ENABLE_PROMPT_CACHING_1H has unexpected value (recommend true)")
        }
        None => {
            eprintln!("{warn} ENABLE_PROMPT_CACHING_1H not set (recommend true for 1h cache reuse)")
        }
    }

    match get("CLAUDE_CODE_SUBAGENT_MODEL") {
        Some(_) => eprintln!("{info} CLAUDE_CODE_SUBAGENT_MODEL is set"),
        None => {
            eprintln!("{info} CLAUDE_CODE_SUBAGENT_MODEL not set (default: claude-sonnet-4-6)")
        }
    }

    if cm_enabled {
        eprintln!("{pass} context-mode built-in feature enabled (token-efficiency)");
    } else {
        eprintln!(
            "{info} context-mode not enabled \
             (set features.context_mode.enabled: true for built-in context saving)"
        );
    }
}

/// Returns bundle names whose directory does not exist under `bundles_dir`.
///
/// # Errors
///
/// A stat error other than "not found" — permission denied, an unreachable
/// mount — is propagated instead of being folded into the missing list
/// (#1436). "The directory isn't there" and "I can't tell whether it's there"
/// need different fixes, so reporting the second as the first sends the user
/// hunting for a folder that is present but unreadable.
fn bundles_with_missing_dirs<'a>(
    bundles: &'a [Bundle],
    bundles_dir: &Path,
) -> anyhow::Result<Vec<&'a str>> {
    let mut missing = Vec::new();
    for bundle in bundles {
        let path = bundles_dir.join(&bundle.name);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => missing.push(bundle.name.as_str()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(bundle.name.as_str());
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("stat bundle directory {}", path.display())));
            }
        }
    }
    Ok(missing)
}

/// Version folder names cached under one adapter's cache directory, with any
/// content-hash suffix stripped. `Ok(None)` means the adapter has no cache dir
/// at all — normal for an adapter that was never materialized.
///
/// # Errors
///
/// Any read or stat error other than the cache directory being absent (#1436).
/// The caller downgrades this to a `warn` line rather than aborting: an
/// unreadable cache is worth telling the user about, but it must not take down
/// the rest of the diagnostics.
fn cached_version_folders(adapter_cache: &Path) -> anyhow::Result<Option<Vec<String>>> {
    let Some(entries) = crate::paths::read_dir_optional(adapter_cache)? else {
        return Ok(None);
    };
    let mut versions = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", adapter_cache.display()))?;
        let path = entry.path();
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            // A dangling symlink or an entry a concurrent GC just removed is
            // genuinely not a cached build, so it is skipped rather than raised.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("stat cache entry {}", path.display()))
                );
            }
        }
        // llmenv writes ASCII version tags, so a non-UTF-8 name is not a cache
        // folder it created and can't carry a version to compare against.
        let Some(dir_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if dir_name.ends_with(".tmp") {
            continue;
        }
        versions.push(match dir_name.rsplit_once('-') {
            Some((prefix, tail)) if super::is_content_hash(tail) => prefix.to_string(),
            _ => dir_name,
        });
    }
    Ok(Some(versions))
}

/// Returns marketplace names defined in `config` that no plugin collection references.
fn unused_marketplaces(config: &Config) -> Vec<&str> {
    use crate::config::split_plugin_ref;
    let referenced: HashSet<&str> = config
        .plugin_collection
        .iter()
        .flat_map(|c| c.plugins.iter())
        .filter_map(|p| split_plugin_ref(p))
        .map(|(m, _)| m)
        .collect();
    config
        .marketplace
        .iter()
        .filter(|m| !referenced.contains(m.name.as_str()))
        .map(|m| m.name.as_str())
        .collect()
}

/// True if a network scope's `match` can never activate: the matcher
/// (`src/scope/matcher.rs`) only evaluates `gateway_mac` today — `ssid`/`cidr`
/// are accepted by the config schema and documented as fields, but silently
/// ignored (#1051). A scope with no `gateway_mac` set can never match,
/// regardless of what `ssid`/`cidr` say.
#[must_use]
fn network_scope_cannot_match(m: &crate::config::NetworkMatch) -> bool {
    m.gateway_mac.is_none()
}

/// Returns the `when` tag sets of `codebase_memory` entries (top-level +
/// bundle-contributed) that no emitted tag covers — these can never activate.
/// Unlike `memory`, there's no `host:` table reference to check (codebase-
/// memory-mcp always resolves to a local stdio process, never a network
/// client).
fn orphan_codebase_memory_entries<'a>(
    config: &'a Config,
    bundle_caps: &'a Capabilities,
    emitted: &HashSet<String>,
) -> Vec<&'a [String]> {
    let top = config
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())
        .unwrap_or_default();
    let bundle = bundle_caps
        .features
        .as_ref()
        .map(|f| f.codebase_memory.as_slice())
        .unwrap_or_default();
    top.iter()
        .chain(bundle.iter())
        .filter(|cm| !cm.when.iter().any(|t| emitted.contains(t)))
        .map(|cm| cm.when.as_slice())
        .collect()
}

/// Returns the bundles that declare `features.memory` but that the active
/// scopes suppress via `disable_bundles`, when nothing else supplies a backend.
///
/// Every other memory check builds from the post-disable firing set, so the
/// entry is already gone before doctor looks: memory works in `~/` and silently
/// stops inside the project, with a green doctor (#1131).
fn memory_orphaned_by_disable_bundles(
    config: &Config,
    config_dir: &Path,
    active: &crate::scope::ActiveScopes,
    bundle_caps: &Capabilities,
) -> Vec<String> {
    // Tag-active, not merely present (#1140): a `features.memory` entry gated
    // on a `when` tag that isn't active right now supplies nothing, so its
    // mere existence must not mask the disabled bundle actually being the
    // only supplier the active scope has.
    let declares_active_memory = |caps: &Option<crate::config::Features>| {
        caps.as_ref().is_some_and(|f| {
            f.memory
                .iter()
                .any(|m| crate::mcp::resolve::memory_is_tag_active(m, &active.tags))
        })
    };
    if declares_active_memory(&config.features) || declares_active_memory(&bundle_caps.features) {
        return Vec::new();
    }
    crate::hook_run::suppressed_memory_bundles(config, config_dir, active)
}

/// Check whether a host address string is a loopback / local-only address.
fn is_local_addr(addr: &str) -> bool {
    matches!(addr, "localhost" | "0.0.0.0" | "::" | "::0")
        || addr
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether any memory block references a remote (non-local) server_host.
/// Returns `true` when a `server_host` maps to a non-local address in the host
/// table, or when the host table has no entry for it (assume remote).
fn has_remote_memory_host(config: &Config) -> bool {
    config.features.as_ref().is_some_and(|f| {
        f.memory.iter().any(|mem| {
            config
                .host
                .get(&mem.server_host)
                .is_none_or(|h| !is_local_addr(&h.addr))
        })
    })
}

/// Check that external tools referenced by the active config are available on
/// `$PATH`. Printed to stderr using the doctor pass/fail/info helpers inline
/// with the rest of `llmenv doctor`.
/// How a dependent tool is updated once it's installed (#1185).
///
/// The distinction is load-bearing, not cosmetic: `icm upgrade --apply`
/// installs the new binary itself, while `codebase-memory-mcp update` only
/// *prints* the install command for the current machine and exits — it can't
/// update itself. llmenv can therefore offer to run the first and can only
/// report the second, so the two can't be collapsed into one "update command"
/// string without the report claiming something false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdatePath {
    /// The tool installs its own update when this command is run.
    SelfApply(&'static str),
    /// The tool can't install its own update; this command reports how.
    Reports(&'static str),
}

impl UpdatePath {
    fn command(self) -> &'static str {
        match self {
            UpdatePath::SelfApply(c) | UpdatePath::Reports(c) => c,
        }
    }
}

/// External tools llmenv wires in but doesn't own the lifecycle of (#1185).
/// `llmenv upgrade` keeps the `llmenv` binary current; nothing kept these
/// current, so they drifted silently.
const DEPENDENT_TOOLS: &[(&str, UpdatePath)] = &[
    ("icm", UpdatePath::SelfApply("icm upgrade --apply")),
    (
        "codebase-memory-mcp",
        UpdatePath::Reports("codebase-memory-mcp update"),
    ),
];

/// The version a tool reports through `--version`, as the last
/// whitespace-separated token of its first output line.
///
/// Both tools answer `<name> <semver>` (`icm 0.10.61`), so one shared parse
/// covers them rather than a per-tool format string — the fragile thing the
/// issue was worried about. A tool whose output doesn't fit is reported as
/// "version unknown" rather than guessed at.
fn tool_version(binary: &str) -> Option<String> {
    let out = std::process::Command::new(binary)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version_line(&String::from_utf8_lossy(&out.stdout), binary)
}

/// The version token in `--version` output, or `None` when the output doesn't
/// look like a version at all.
///
/// Split out from [`tool_version`] so the parse is testable without depending
/// on whichever binaries happen to be installed — `true --version` prints a
/// real version string on a machine with GNU coreutils and nothing on one
/// without, which makes it useless as a fixture.
fn parse_version_line(output: &str, binary: &str) -> Option<String> {
    let token = output.lines().next()?.split_whitespace().last()?;
    // A `--version` that echoes only the binary name tells us nothing, and a
    // token with no digit in it isn't a version.
    (token != binary && token.chars().any(|c| c.is_ascii_digit())).then(|| token.to_string())
}

/// Report installed versions and update commands for the tools llmenv depends
/// on but doesn't ship (#1185). Tools that aren't installed are skipped —
/// `run_doctor_tool_availability` already reports those, and repeating it here
/// would say the same thing twice with less context.
///
/// Deliberately offline: no "an update is available" claim is made, because
/// checking would mean a network round trip per tool on every `doctor` run.
fn run_doctor_dependent_tools(use_color: bool) {
    let pass = super::doctor_pass(use_color);
    let info = super::doctor_info(use_color);

    let installed: Vec<_> = DEPENDENT_TOOLS
        .iter()
        .filter(|(bin, _)| crate::paths::binary_on_path(bin))
        .collect();
    if installed.is_empty() {
        return;
    }

    eprintln!();
    eprintln!("Dependent-tool versions:");
    for (bin, update) in installed {
        let version = tool_version(bin).unwrap_or_else(|| "version unknown".to_string());
        let how = match update {
            UpdatePath::SelfApply(_) => "update with",
            UpdatePath::Reports(_) => "check for updates with",
        };
        let marker = if version == "version unknown" {
            &info
        } else {
            &pass
        };
        eprintln!("{marker} {bin} {version} — {how} `{}`", update.command());
    }
}

fn run_doctor_tool_availability(use_color: bool, config: &Config) {
    let pass = super::doctor_pass(use_color);
    let fail = super::doctor_fail(use_color);
    let info = super::doctor_info(use_color);

    eprintln!();
    eprintln!("Tool-availability checks:");

    let has_memory = config
        .features
        .as_ref()
        .is_some_and(|f| !f.memory.is_empty());

    // icm — required when features.memory has entries
    // mcp-proxy or uvx — required when any memory server_host is remote
    if has_memory {
        if crate::paths::binary_on_path("icm") {
            eprintln!("{pass} icm found on PATH");
        } else {
            eprintln!("{fail} icm not found on PATH (required when features.memory is configured)");
        }

        if has_remote_memory_host(config) {
            if crate::paths::binary_on_path("mcp-proxy") || crate::paths::binary_on_path("uvx") {
                eprintln!("{pass} mcp-proxy or uvx found on PATH (remote memory server_host)");
            } else {
                eprintln!(
                    "{fail} neither mcp-proxy nor uvx on PATH \
                     (remote memory server_host requires one for TCP proxying)"
                );
            }
        }
    }

    // codebase-memory-mcp — required when features.codebase_memory has entries
    let has_codebase_memory = config
        .features
        .as_ref()
        .is_some_and(|f| !f.codebase_memory.is_empty());
    if has_codebase_memory {
        if crate::paths::binary_on_path("codebase-memory-mcp") {
            eprintln!("{pass} codebase-memory-mcp found on PATH");
        } else {
            eprintln!(
                "{fail} codebase-memory-mcp not found on PATH (required when \
                 features.codebase_memory is configured) — install: \
                 https://github.com/DeusData/codebase-memory-mcp"
            );
        }
    }

    // claude — required when claude_code engine is not disabled
    let claude_disabled = config
        .disabled_engines
        .iter()
        .any(|e| e.eq_ignore_ascii_case("claude_code"));
    if !claude_disabled {
        if crate::paths::binary_on_path("claude") {
            eprintln!("{pass} claude found on PATH");
        } else {
            eprintln!(
                "{fail} claude not found on PATH \
                 (claude_code engine is not disabled, but the `claude` binary is missing)"
            );
        }
    }

    // Every other registered engine — always optional. Derived from the adapter
    // registry rather than a hardcoded list, so a newly registered engine is
    // reported here without a second edit (#1032).
    for adapter in crate::adapter::registered_adapters() {
        if crate::adapter::engine_id(adapter.as_ref()) == "claude_code" {
            continue; // reported above, where it is required rather than optional
        }
        let bin = adapter.binary_name();
        if crate::paths::binary_on_path(bin) {
            eprintln!("{pass} {bin} found on PATH");
        } else {
            eprintln!("{info} {bin} not found on PATH (optional engine)");
        }
    }
}

/// Codex-specific doctor coverage, parallel to the `claude_code` lifecycle-hooks
/// section above (#1100). Gated on Codex being installed so a user who never
/// runs Codex sees no Codex-specific noise.
fn run_doctor_codex(
    use_color: bool,
    config: &Config,
    cache_dir: &Path,
    doctor_manifest: Option<&(crate::merge::MergedManifest, PathBuf)>,
) {
    use crate::adapter::AgentAdapter;
    use crate::adapter::codex::{
        CodexAdapter, PermissionProfileDecision, classify_permission_profile,
    };

    if !super::installed_adapters(config).any(|a| a.name() == "codex") {
        return;
    }

    let pass = super::doctor_pass(use_color);
    let warn = super::doctor_warning(use_color);
    let info = super::doctor_info(use_color);

    eprintln!();
    eprintln!("Codex adapter:");

    // #1102: proactively surface whether the permission profile will render
    // or be refused, without requiring an `export`/`regenerate` run — the
    // stderr warning at materialize time is easy to miss in a script.
    let caps = doctor_manifest.map_or(&config.capabilities, |(m, _)| &m.capabilities);
    match classify_permission_profile(&caps.permissions) {
        PermissionProfileDecision::Empty => {}
        PermissionProfileDecision::Rendered(entries) => {
            eprintln!(
                "{pass} permission profile: {} filesystem path rule(s) will render (activated \
                 via default_permissions)",
                entries.len()
            );
        }
        PermissionProfileDecision::Refused {
            unmappable_rule_count,
            mappable_rule_count,
        } => {
            eprintln!(
                "{warn} permission profile: refused to render — {unmappable_rule_count} rule(s) \
                 have no Codex equivalent (Bash, WebFetch/network, or ask-tier), which also \
                 drops {mappable_rule_count} otherwise-renderable path rule(s) (#1102's \
                 all-or-nothing rule). Codex runs under its own default approval policy and \
                 sandbox mode instead."
            );
        }
    }

    // MCP servers Codex cannot speak to at all (SSE transport, #233's
    // render_mcp_servers skips these with a stderr warning at materialize
    // time) — surfaced here too so `doctor` catches it without a render.
    if let Some((manifest, _)) = doctor_manifest {
        for mcp in &manifest.mcps {
            if let crate::mcp::resolve::ResolvedKind::Remote {
                transport: crate::config::McpTransport::Sse,
                ..
            } = &mcp.kind
            {
                eprintln!(
                    "{warn} MCP server '{}' uses the SSE transport, which Codex does not \
                     support (stdio and streamable HTTP only) — it will be skipped for Codex \
                     only",
                    mcp.name
                );
            }
        }
    }

    // Detect a materialized config.toml that exists but fails to parse — a
    // hand-edit or a bug in a future change to this adapter, not a config
    // llmenv itself would ever intentionally write.
    let config_path = cache_dir
        .join(CodexAdapter.name())
        .join(crate::adapter::codex::CODEX_CONFIG_FILE);
    match std::fs::read_to_string(&config_path) {
        Ok(raw) => match raw.parse::<toml::Table>() {
            Ok(_) => eprintln!("{pass} materialized config.toml is valid TOML"),
            Err(e) => eprintln!(
                "{warn} materialized config.toml at {} is not valid TOML: {e}",
                config_path.display()
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "{info} config.toml not yet materialized at {}",
                config_path.display()
            );
        }
        Err(e) => eprintln!(
            "{warn} could not read materialized config.toml at {}: {e}",
            config_path.display()
        ),
    }
}

/// Claude Code field-prefix patterns: a bare `<field>:<value>` filter, as
/// opposed to a command-prefix pattern. `WebFetch(domain:example.com)` is the
/// common one.
const CLAUDE_FIELD_PREFIXES: &[&str] = &["domain:", "url:"];

/// Whether `pattern` uses Claude Code's colon-prefix syntax rather than a plain
/// glob: either a trailing `:*` command-prefix match (`git commit:*`, `rg:*`) or
/// a leading field filter (`domain:example.com`).
///
/// Matching the actual grammar rather than "any colon after a word character"
/// matters in both directions. A pattern like `docker run -p 8080:8080 *` or
/// `awk -F: *` carries a literal colon that behaves identically under both
/// engines and must not be flagged; conversely a quoted or globbed token before
/// the colon (`"wip":*`, `*:*`) is still Claude's prefix syntax and must be.
fn uses_colon_prefix_syntax(pattern: &str) -> bool {
    pattern.ends_with(":*")
        || CLAUDE_FIELD_PREFIXES
            .iter()
            .any(|prefix| pattern.starts_with(prefix))
}

/// A permission rule naming a tool outside llmenv's neutral vocabulary (#1371).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnknownToolRule {
    /// The tool name as written, sanitized for terminal output.
    pub tool: String,
}

impl UnknownToolRule {
    /// The diagnostic for this rule.
    ///
    /// Deliberately *not* phrased as "this does nothing": Claude Code's adapter
    /// passes an unrecognized name through verbatim, so a rule naming a real
    /// Claude Code tool llmenv doesn't map still works there. What's certain is
    /// that the engines needing translation can only drop it.
    pub fn message(&self) -> String {
        format!(
            "permission rule tool `{}` isn't one of llmenv's neutral tool names ({}), so \
             opencode and crush have no key to render and drop the rule. Claude Code still \
             receives it verbatim. Fix the name, or use `native_permissions.<engine>` to \
             target an engine's own tool directly",
            self.tool,
            crate::adapter::tools::known_names().join(", ")
        )
    }
}

/// Returns each distinct tool name in the neutral permission rules that isn't in
/// llmenv's vocabulary (#1371).
///
/// Deduplicated by name: the same typo repeated across `allow`/`ask`/`deny`, or
/// across several rules, is one problem to fix and reporting it once is what
/// makes this printable from `export`/`regenerate` at all — the per-rule warning
/// this replaced fired once per rule per adapter.
///
/// Takes merged `capabilities` so a bad rule contributed by a `bundle.yaml` is
/// covered too.
pub(super) fn unknown_neutral_tools(capabilities: &Capabilities) -> Vec<UnknownToolRule> {
    let perms = &capabilities.permissions;
    let mut seen = std::collections::BTreeSet::new();
    [&perms.allow, &perms.ask, &perms.deny]
        .into_iter()
        .flatten()
        .filter(|rule| !crate::adapter::tools::is_known(&rule.tool))
        .filter(|rule| seen.insert(rule.tool.clone()))
        .map(|rule| UnknownToolRule {
            tool: crate::util::display_safe(&rule.tool).into_owned(),
        })
        .collect()
}

/// A neutral tool whose mapping onto an active engine isn't one-to-one (#1371).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InexactToolMapping {
    /// The neutral tool name the user wrote.
    pub tool: String,
    /// The engine id whose mapping differs.
    pub engine: &'static str,
    /// How it differs, from the mapping table.
    pub note: &'static str,
}

impl InexactToolMapping {
    pub fn message(&self) -> String {
        let Self { tool, engine, note } = self;
        format!("permission rule tool `{tool}` on {engine}: {note}")
    }
}

/// Returns every neutral tool the config actually uses whose mapping onto an
/// active engine is either broader than the neutral name implies or absent
/// (#1371).
///
/// `doctor`-only on purpose. Unlike [`unknown_neutral_tools`] this is not broken
/// config — it's a documented particularity of the adapter
/// (`website/docs/engines.md`) — and `export`/`regenerate` run from the shell
/// prompt on every invocation, so a note about working config has no business
/// printing there. That's the same line [`super::warn_dead_config`] draws for the
/// #975 legacy-tool lint.
///
/// `active_engines` is passed in rather than probed so this is testable without
/// depending on the host's `PATH`.
fn inexact_tool_mappings(
    capabilities: &Capabilities,
    active_engines: &[&str],
) -> Vec<InexactToolMapping> {
    use crate::adapter::tools;

    let perms = &capabilities.permissions;
    let mut seen = std::collections::BTreeSet::new();
    let mut hits = Vec::new();
    for rule in [&perms.allow, &perms.ask, &perms.deny]
        .into_iter()
        .flatten()
    {
        let Some(entry) = tools::lookup(&rule.tool) else {
            continue; // reported by `unknown_neutral_tools` instead
        };
        for (engine, mapping) in [("opencode", entry.opencode), ("crush", entry.crush)] {
            if !active_engines.contains(&engine) {
                continue;
            }
            let Some(note) = mapping.note() else {
                continue; // one-to-one rename; nothing to say
            };
            if seen.insert((rule.tool.clone(), engine)) {
                hits.push(InexactToolMapping {
                    tool: crate::util::display_safe(&rule.tool).into_owned(),
                    engine,
                    note,
                });
            }
        }
    }
    hits
}

/// A permission rule that is dead config under opencode (#838).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ColonPrefixRule {
    /// `"allow"`, `"ask"`, or `"deny"` — the tier the rule was declared in.
    pub tier: &'static str,
    /// `"<tool>(<pattern>)"`, as the user would recognize it.
    pub rule: String,
}

impl ColonPrefixRule {
    /// The diagnostic for this rule. A dead `deny` fails **open** — the rule the
    /// user wrote to block something doesn't block it — so it gets stronger
    /// wording than a dead `allow`, which merely fails closed into a prompt.
    pub fn message(&self) -> String {
        let Self { tier, rule } = self;
        let consequence = if *tier == "deny" {
            "so the deny is NOT enforced there — whatever it was meant to block is left \
             to opencode's default"
        } else {
            "so the grant never applies there"
        };
        format!(
            "permission rule {rule} ({tier}) uses Claude Code's colon-prefix syntax, which \
             opencode matches as a literal glob, {consequence}. Use a space-separated pattern \
             (e.g. `git commit *`) for a rule both engines honour, or move the Claude-only form \
             to `native_permissions.claude_code`"
        )
    }
}

/// Returns every neutral permission rule whose pattern uses Claude Code's
/// colon-prefix syntax, for a config where opencode is also materialized (#838).
///
/// opencode matches a permission pattern as a plain glob against the whole
/// command string, so `git commit:*` matches nothing there. Only the
/// engine-neutral `permissions` block is checked; `native_permissions.claude_code`
/// is Claude-scoped on purpose and correct as written.
///
/// Takes merged `capabilities` so rules contributed by a `bundle.yaml` are
/// covered — a dead `deny` shipped in a shared bundle is the case most worth
/// catching. `opencode_active` is passed in rather than probed so the check is
/// testable without depending on the host's `PATH`.
pub(super) fn claude_only_colon_permission_patterns(
    capabilities: &Capabilities,
    opencode_active: bool,
) -> Vec<ColonPrefixRule> {
    if !opencode_active {
        return Vec::new();
    }
    let perms = &capabilities.permissions;
    [
        ("allow", &perms.allow),
        ("ask", &perms.ask),
        ("deny", &perms.deny),
    ]
    .into_iter()
    .flat_map(|(tier, rules)| rules.iter().map(move |rule| (tier, rule)))
    .filter_map(|(tier, rule)| {
        let pattern = rule.pattern.as_deref()?;
        uses_colon_prefix_syntax(pattern).then(|| ColonPrefixRule {
            tier,
            rule: format!(
                "{}({})",
                crate::util::display_safe(&rule.tool),
                crate::util::display_safe(pattern)
            ),
        })
    })
    .collect()
}

/// Whether `matcher` is shaped like a file-extension glob (`*.rs`, `**/*.py`)
/// or a bare extension (`.rs`) rather than a tool-name pattern.
fn looks_like_file_glob(matcher: &str) -> bool {
    if let Some(ext) = matcher.strip_prefix('.') {
        return !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric());
    }
    matcher.match_indices("*.").any(|(idx, _)| {
        matcher[idx + 2..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// Returns `"{event} (matcher: '{matcher}')"` for each hook whose matcher is
/// shaped like a file-extension glob instead of a Claude Code tool-name
/// pattern — a common misconfiguration, since Claude Code matches
/// `hook.matcher` against tool name only, never file path.
fn hooks_with_glob_like_matchers(config: &Config) -> Vec<String> {
    config
        .capabilities
        .hooks
        .iter()
        .filter_map(|hook| {
            let matcher = hook.matcher.as_deref()?;
            looks_like_file_glob(matcher)
                .then(|| format!("{} (matcher: '{}')", hook.event, matcher))
        })
        .collect()
}

/// Legacy shell tool -> recommended replacement, per this project's own
/// bundled rules (`examples/config-llmenv-dir/bundles/base/AGENTS.md`'s "CLI
/// Tools" table). Only pairs that table names an explicit replacement for are
/// in scope — a tool like `cat` has no named replacement there and is
/// deliberately not checked (#975).
const LEGACY_TOOL_REPLACEMENTS: &[(&str, &str)] = &[("grep", "rg"), ("find", "fd")];

/// A config `allow` rule for a legacy tool with no matching allow rule for
/// its recommended replacement (#975).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LegacyToolRule {
    pub legacy: &'static str,
    pub replacement: &'static str,
}

impl LegacyToolRule {
    pub fn message(&self) -> String {
        let Self {
            legacy,
            replacement,
        } = self;
        format!(
            "config allows `{legacy}` but not its recommended replacement `{replacement}` — \
             this project's own bundled rules tell the agent to prefer `{replacement}`, so every \
             `{replacement}` invocation still hits a permission prompt. Add an allow rule for \
             `{replacement}` (e.g. `Bash({replacement} *)`), or set \
             `capabilities.permissions.preset: safe-readonly` to cover it"
        )
    }
}

/// The leading whitespace-delimited token of a Bash permission pattern, with
/// any Claude-style colon-subcommand suffix stripped — the command name a
/// glob like `"grep -r *"` or `"grep:*"` actually gates.
fn pattern_command(pattern: &str) -> &str {
    let first = pattern.split_whitespace().next().unwrap_or("");
    first.split(':').next().unwrap_or("")
}

/// The command a raw `native_permissions.<engine>.allow` string (Claude's
/// own `"Bash(<pattern>)"` grammar) grants, or `None` for a non-`Bash` entry
/// (`"WebFetch(domain:example.com)"`) or a malformed one.
fn native_bash_command(raw: &str) -> Option<&str> {
    let inner = raw.strip_prefix("Bash(")?.strip_suffix(')')?;
    Some(pattern_command(inner))
}

/// Returns every legacy/replacement pair (#975) where the merged config
/// grants the legacy tool but not its recommended replacement.
///
/// Scoped to `Bash` rules and the `allow` tier only: `ask`/`deny` don't grant
/// access, so they're not the "still gets a permission prompt" case this lint
/// targets. Takes merged `capabilities` so a bundle-contributed `allow` rule
/// for the replacement correctly silences the warning. Also checks every
/// engine's `native_permissions.<engine>.allow` — a replacement granted only
/// there (a documented, exercised pattern) must silence the warning too,
/// not just a neutral `permissions.allow` entry.
fn legacy_tools_missing_replacement(capabilities: &Capabilities) -> Vec<LegacyToolRule> {
    let neutral = capabilities
        .permissions
        .allow
        .iter()
        .filter(|r| r.tool == "Bash")
        .filter_map(|r| r.pattern.as_deref())
        .map(pattern_command);
    let native = capabilities
        .native_permissions
        .values()
        .flat_map(|rules| rules.allow.iter())
        .filter_map(|raw| native_bash_command(raw));
    let commands: std::collections::HashSet<&str> = neutral.chain(native).collect();

    LEGACY_TOOL_REPLACEMENTS
        .iter()
        .filter(|(legacy, replacement)| {
            commands.contains(legacy) && !commands.contains(replacement)
        })
        .map(|&(legacy, replacement)| LegacyToolRule {
            legacy,
            replacement,
        })
        .collect()
}

/// Drop the platform credential entry belonging to each cache folder GC just
/// deleted, and report how many went (#1057).
///
/// Keyed by folder path, so this only ever touches entries llmenv's own folders
/// owned — never the user's default `~/.claude` login. Deliberately confined to
/// the explicit `doctor --gc` path: removing credential entries as a side effect
/// of `export` would be the wrong default.
pub(crate) fn forget_credentials_for(removed: &[PathBuf]) -> usize {
    removed
        .iter()
        .filter(|path| {
            crate::auth::credentials::forget(path)
                .inspect_err(|e| {
                    tracing::debug!(
                        "could not drop credential entry for {}: {e}",
                        path.display()
                    );
                })
                .unwrap_or(false)
        })
        .count()
}

/// Report the durable OAuth credential cache (#1057): present or not, and
/// whether the cached token is still usable.
fn report_credential_cache(cache_dir: &Path, pass: &str, info: &str, warn: &str) {
    use crate::adapter::AgentAdapter;
    let adapter_root = cache_dir.join(crate::adapter::claude_code::ClaudeCodeAdapter.name());
    match crate::auth::credentials::load_cached(&adapter_root) {
        Ok(None) => eprintln!(
            "{info} No OAuth credential cached — a config or version change will prompt for \
             login. Run `llmenv login` to cache one."
        ),
        Ok(Some(creds)) if creds.is_expired_now() => eprintln!(
            "{warn} Cached OAuth credential has expired (access and refresh tokens both past \
             their expiry) — run `llmenv login` to refresh it"
        ),
        Ok(Some(creds)) => {
            let cache_file = crate::auth::credentials::cache_path(&adapter_root);
            let path = cache_file.display();
            // MCP server tokens ride in the same blob (#1058); report them so a
            // user can tell whether their Slack/Notion logins are covered too.
            match creds.mcp_server_count() {
                0 => eprintln!("{pass} OAuth credential cached at {path}"),
                1 => eprintln!("{pass} OAuth credential cached at {path} (+1 MCP server token)"),
                n => eprintln!("{pass} OAuth credential cached at {path} (+{n} MCP server tokens)"),
            }
        }
        Err(e) => eprintln!("{warn} Could not read the OAuth credential cache: {e}"),
    }
}

pub(super) fn run_doctor(gc: bool, all: bool, use_color: bool) -> anyhow::Result<()> {
    let pass = super::doctor_pass(use_color);
    let warn = super::doctor_warning(use_color);
    let info = super::doctor_info(use_color);

    eprintln!("Running llmenv doctor...");

    let config_path = paths::config_path()?;
    let config = Config::load(&config_path)?;
    let cm_enabled = config.context_mode_enabled();
    eprintln!("{pass} Configuration loaded from {}", config_path.display());

    // Check that config parses
    eprintln!("{pass} Config is valid YAML");

    // Structural validation: bundle directories, marketplace references, permission grants
    let config_dir = paths::config_dir()?;
    let bundles_dir = config_dir.join("bundles");

    for name in bundles_with_missing_dirs(&config.bundle, &bundles_dir)? {
        eprintln!(
            "{info} Bundle '{}' declared but directory does not exist at {}",
            crate::util::display_safe(name),
            bundles_dir.join(name).display(),
        );
    }

    for name in unused_marketplaces(&config) {
        eprintln!(
            "{warn} Marketplace '{}' is defined but not referenced by any plugin collection",
            crate::util::display_safe(name),
        );
    }

    for hit in hooks_with_glob_like_matchers(&config) {
        eprintln!(
            "{warn} hook {} looks like a file-extension glob, but Claude Code matches \
             hook.matcher against tool name only, never file path — use a `scope.content` \
             glob to gate the hook's bundle by file type instead",
            crate::util::display_safe(&hit),
        );
    }

    // Check cache directory is writable. Hardening only applies when this
    // check creates the dir; an existing one keeps whatever permissions its
    // owner set — forcing 0700 here would hard-fail on a cache dir shared
    // with a different uid, the same regression #1196 walked back for
    // codebase_memory.index_path (#1198).
    let cache_dir = PathBuf::from(crate::paths::expand_tilde(&config.cache.cache_dir));
    if cache_dir.exists() {
        let probe = cache_dir.join(".llmenv-doctor-probe");
        std::fs::write(&probe, b"").context("cache directory not writable")?;
        let _ = std::fs::remove_file(&probe);
    } else {
        crate::paths::create_dir_owner_only(&cache_dir).context("cache directory not writable")?;
    }
    eprintln!(
        "{pass} Cache directory is writable: {}",
        cache_dir.display()
    );

    // Report the active cache layout so `doctor` explains the folder shape on disk.
    match config.cache.hashing {
        crate::config::HashingMode::Loose => {
            eprintln!("{pass} Cache hashing: loose (folder: <shape>)");
        }
        crate::config::HashingMode::Normal => {
            eprintln!(
                "{pass} Cache hashing: normal (folder: {}/<shape>)",
                crate::materialize::cache::version_major()
            );
        }
        crate::config::HashingMode::Strict => {
            eprintln!("{pass} Cache hashing: strict (content-addressed folders)");
        }
    }

    // Check for version skew across all registered adapters
    let skew_relevant = !matches!(config.cache.hashing, crate::config::HashingMode::Loose);
    if skew_relevant {
        for adapter in crate::adapter::registered_adapters() {
            let adapter_cache = cache_dir.join(adapter.name());
            let mut cached_versions = match cached_version_folders(&adapter_cache) {
                Ok(Some(versions)) => versions,
                Ok(None) => continue,
                Err(e) => {
                    eprintln!(
                        "{warn} Could not check {} for version skew: {e:#}",
                        adapter_cache.display(),
                    );
                    continue;
                }
            };
            cached_versions.sort();
            cached_versions.dedup();
            let version_folder = crate::materialize::cache::version_major();
            let current_built = |v: &String| v == super::VERSION_TAG || *v == version_folder;
            if !cached_versions.is_empty() && !cached_versions.iter().any(current_built) {
                eprintln!(
                    "{warn} {} version skew detected: running llmenv {} but cache has versions [{}]",
                    adapter.name(),
                    super::VERSION_TAG,
                    cached_versions.join(", "),
                );
                eprintln!("{warn}   → Fix: cargo install --path . --force");
            }
        }
    }

    // Check git remote is reachable
    if super::is_git_repo(&config_dir) {
        match super::check_git_remote(&config_dir) {
            Ok(remote) => {
                let safe_url = crate::git::sanitize_git_url(&remote);
                eprintln!("{pass} Git remote reachable: {}", safe_url);
            }
            Err(e) => eprintln!("{warn} Git remote check failed: {}", e),
        }
    } else {
        eprintln!("{warn} Config directory is not a git repo");
    }

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    // Cross-engine hook compatibility (#543 follow-up): name any hook that will
    // be silently skipped when materializing for an installed adapter with a
    // narrower supported-hook-event set (e.g. Crush only supports PreToolUse).
    // Only checks adapters actually on PATH — an adapter you don't have
    // installed skipping a hook it could never run isn't worth flagging.
    let doctor_firing = super::firing_bundles(&config.bundle, &active, None);
    let doctor_manifest =
        super::build_manifest(&config, &config_dir, &active, &doctor_firing, false)?;

    // Dead per-engine keys (#1032) and permission patterns opencode can never
    // match (#838). Reported from the merged manifest so bundle-contributed
    // config is covered, which is why this sits below build_manifest rather than
    // with the other structural checks above.
    super::warn_dead_config(&config, doctor_manifest.as_ref().map(|(m, _)| m), &warn);

    // #1371: how each active engine actually renders the neutral tools this
    // config uses — the rules an engine widens, and the ones it drops for want
    // of an equivalent. Working config, so `export`/`regenerate` stay quiet
    // about it and this is where a user looks instead. Plain `doctor`, not
    // `--all`: "why isn't my permission rule working on opencode" is the
    // question doctor exists to answer.
    {
        let caps = doctor_manifest
            .as_ref()
            .map_or(&config.capabilities, |(m, _)| &m.capabilities);
        let active_engines: Vec<&str> = super::installed_adapters(&config)
            .map(|a| a.name())
            .collect();
        for hit in inexact_tool_mappings(caps, &active_engines) {
            eprintln!("{warn} {}", hit.message());
        }
    }

    // #741/#1435: which lifecycle hooks are actually wired for this scope.
    // Without this there was no way to confirm from inside llmenv that session
    // start/end, per-turn recall, or the Stop reminder would fire — the only
    // check was reading the generated settings.json/config.toml by hand.
    // `claude_code` and `codex` share the same engine-neutral gate
    // (`crate::adapter::lifecycle_hook_registrations`), so both get reported
    // the same way rather than only the first adapter to land this check.
    if let Some((manifest, _)) = &doctor_manifest {
        for engine in ["claude_code", "codex"] {
            if !super::installed_adapters(&config).any(|a| a.name() == engine) {
                continue;
            }
            eprintln!();
            eprintln!("Lifecycle hooks ({engine}):");
            for (event, registered, why) in crate::adapter::lifecycle_hook_registrations(manifest) {
                if registered {
                    eprintln!("{pass} {event}");
                } else {
                    eprintln!("{info} {event} not registered — {why}");
                }
            }
        }
    }

    if let Some((manifest, _)) = &doctor_manifest {
        for adapter in super::installed_adapters(&config) {
            let supported = adapter.supported_hook_events();
            for hook in &manifest.capabilities.hooks {
                if !supported.contains(&hook.event.as_str()) {
                    eprintln!(
                        "{warn} hook event '{}' is not supported by the {} adapter — \
                         it will be skipped, not materialized. Supported events: {}",
                        hook.event,
                        adapter.name(),
                        supported.join(", ")
                    );
                }
            }
        }
    }

    run_doctor_codex(use_color, &config, &cache_dir, doctor_manifest.as_ref());

    // Resolved native.claude_code.env, for the token-efficiency checks below
    // to treat as equally "set" alongside the process environment (#543 follow-up).
    let native_claude_env = doctor_manifest
        .as_ref()
        .and_then(|(manifest, _)| manifest.native.get("claude_code"))
        .and_then(|v| v.get("env"));

    if all {
        // Orphan detection
        let mut emitted = super::all_emitted_tags(&config);
        emitted.extend(active.tags.iter().cloned());
        let consumed = super::all_consumed_tags(&config);
        let marker_enabled = super::marker_enabled_bundle_names(&active);

        let mut orphan_count: usize = 0;
        for s in &config.scope.network {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope network:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
            if network_scope_cannot_match(&s.r#match) {
                eprintln!(
                    "{warn} orphan scope network:{}: match has no gateway_mac — only \
                     gateway_mac is evaluated today (ssid/cidr are accepted but ignored), \
                     so this scope can never activate; set gateway_mac or use a host scope \
                     instead",
                    s.id
                );
                orphan_count += 1;
            }
        }
        for s in &config.scope.host {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope host:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
        }
        for s in &config.scope.user {
            if !s.tags.iter().any(|t| consumed.contains(t)) {
                eprintln!(
                    "{warn} orphan scope user:{}: no bundle consumes its tags",
                    s.id
                );
                orphan_count += 1;
            }
        }

        let configured_bundle_names: std::collections::HashSet<&str> =
            config.bundle.iter().map(|b| b.name.as_str()).collect();
        for scope in &active.scopes {
            if scope.kind != "project" {
                continue;
            }
            for field in &scope.unknown_fields {
                eprintln!("{warn} unknown field in .llmenv.yaml: {field}");
                orphan_count += 1;
            }
            for bundle_name in &scope.enable_bundles {
                if !configured_bundle_names.contains(bundle_name.as_str()) {
                    eprintln!(
                        "{warn} .llmenv.yaml enable_bundles references unknown bundle: {bundle_name}"
                    );
                    orphan_count += 1;
                }
            }
            for bundle_name in &scope.disable_bundles {
                if !configured_bundle_names.contains(bundle_name.as_str()) {
                    eprintln!(
                        "{warn} .llmenv.yaml disable_bundles references unknown bundle: {bundle_name}"
                    );
                    orphan_count += 1;
                }
                // #194: same-scope enable+disable is contradictory intent —
                // disable wins at runtime, but flag it so the user notices
                // the enable_bundles entry is dead.
                if scope.enable_bundles.contains(bundle_name) {
                    eprintln!(
                        "{warn} .llmenv.yaml enables and disables the same bundle: {bundle_name} \
                         (disable wins; the enable_bundles entry has no effect)"
                    );
                    orphan_count += 1;
                }
            }
        }

        for b in &config.bundle {
            let has_emitted_tag = b.when.iter().any(|t| emitted.contains(t));
            let looks_marker = super::looks_marker_driven(&b.name, b);
            if !has_emitted_tag && !marker_enabled.contains(&b.name) && !looks_marker {
                eprintln!("{warn} orphan bundle {}: no scope emits its tags", b.name);
                orphan_count += 1;
            }
        }

        for m in &config.mcp {
            let has_emitted_tag = m.when.iter().any(|t| emitted.contains(t));
            let looks_marker = m.when.iter().any(|t| super::tag_looks_marker_sourced(t));
            if !has_emitted_tag && !looks_marker {
                eprintln!("{warn} orphan mcp {}: no scope emits its tags", m.name);
                orphan_count += 1;
            }
        }

        // Build merged host table for server_host checks
        let doctor_firing: Vec<_> = super::firing_bundles(&config.bundle, &active, None);

        let doctor_bundle_caps = {
            let refs = super::build_bundle_refs(&config_dir, &active, &doctor_firing);
            if refs.is_empty() {
                crate::config::Capabilities::default()
            } else {
                crate::merge::merge(&config.capabilities, &config.native, &refs)
                    .context("failed to merge bundle capabilities for orphan check")?
                    .capabilities
            }
        };

        for hit in legacy_tools_missing_replacement(&doctor_bundle_caps) {
            eprintln!("{warn} {}", hit.message());
        }

        let mut merged_host_for_doctor = doctor_bundle_caps.host.clone();
        for (k, v) in &config.host {
            merged_host_for_doctor.insert(k.clone(), v.clone());
        }

        // Check top-level memory entries
        if let Some(features) = &config.features {
            for mem in &features.memory {
                let has_emitted_tag = mem.when.iter().any(|t| emitted.contains(t));
                if !has_emitted_tag {
                    eprintln!(
                        "{warn} orphan memory (server_host '{}'): no scope emits its tags",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
                if !merged_host_for_doctor.contains_key(&mem.server_host) {
                    eprintln!(
                        "{warn} memory: server_host '{}' has no entry in the host: table",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
            }
        }

        // Check bundle-contributed memory entries
        if let Some(features) = &doctor_bundle_caps.features {
            for mem in &features.memory {
                let has_emitted_tag = mem.when.iter().any(|t| emitted.contains(t));
                if !has_emitted_tag {
                    eprintln!(
                        "{warn} orphan bundle memory (server_host '{}'): no scope emits its tags",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
                if !merged_host_for_doctor.contains_key(&mem.server_host) {
                    eprintln!(
                        "{warn} bundle memory: server_host '{}' has no entry in host: table",
                        mem.server_host
                    );
                    orphan_count += 1;
                }
            }
        }

        let orphaned_memory_bundles =
            memory_orphaned_by_disable_bundles(&config, &config_dir, &active, &doctor_bundle_caps);
        if !orphaned_memory_bundles.is_empty() {
            // One message naming every supplier (#1139): a per-name loop each
            // saying "only supplied by bundle X" is self-contradictory the
            // moment there are two.
            eprintln!(
                "{warn} features.memory is supplied only by disabled bundle(s) {}, which this \
                 project turns off via disable_bundles — memory recall/store and session \
                 logging are inactive here",
                orphaned_memory_bundles.join(", ")
            );
            orphan_count += 1;
        }

        // Check codebase_memory entries (top-level + bundle-contributed).
        for when in orphan_codebase_memory_entries(&config, &doctor_bundle_caps, &emitted) {
            eprintln!("{warn} orphan codebase_memory: no scope emits its tags {when:?}");
            orphan_count += 1;
        }

        // Plugin orphans
        {
            use crate::config::split_plugin_ref;

            let mut referenceable: HashSet<&str> = HashSet::new();
            for c in &config.plugin_collection {
                let selectable = c.when.iter().any(|t| emitted.contains(t));
                if !selectable {
                    eprintln!(
                        "{warn} orphan plugin-collection {}: no scope emits its tags",
                        c.name
                    );
                    orphan_count += 1;
                }
                if selectable {
                    referenceable.extend(
                        c.plugins
                            .iter()
                            .filter_map(|p| split_plugin_ref(p).map(|(m, _)| m)),
                    );
                }
            }
            for m in &config.marketplace {
                // When context-mode is enabled as a built-in feature the user
                // need not declare it in a plugin-collection — the built-in
                // injection covers it. Suppress the false orphan warning.
                let builtin_exempt =
                    cm_enabled && m.name == crate::config::CONTEXT_MODE_MARKETPLACE;
                if !builtin_exempt && !referenceable.contains(m.name.as_str()) {
                    eprintln!(
                        "{warn} orphan marketplace {}: no selectable plugin references it",
                        m.name
                    );
                    orphan_count += 1;
                }
            }
        }

        // Tag orphans
        let mut tag_universe: HashSet<String> = HashSet::new();
        tag_universe.extend(emitted.iter().cloned());
        tag_universe.extend(consumed.iter().cloned());
        tag_universe.extend(active.tags.iter().cloned());
        let mut tag_orphans: Vec<String> = tag_universe
            .into_iter()
            .filter(|t| {
                let emitted_anywhere = emitted.contains(t)
                    || active.tags.contains(t)
                    || super::tag_looks_marker_sourced(t);
                let consumed_anywhere = consumed.contains(t);
                !(emitted_anywhere && consumed_anywhere)
            })
            .collect();
        tag_orphans.sort();
        for t in &tag_orphans {
            let emitted_anywhere = emitted.contains(t)
                || active.tags.contains(t)
                || super::tag_looks_marker_sourced(t);
            let reason = if !emitted_anywhere {
                "no scope emits it"
            } else {
                "no bundle consumes it"
            };
            eprintln!("{warn} orphan tag {}: {}", t, reason);
            orphan_count += 1;
        }

        if orphan_count == 0 {
            eprintln!("{pass} No orphan scopes/tags/bundles/plugins");
        } else {
            eprintln!("{warn} Found {} orphan item(s)", orphan_count);
        }
    } // end if all

    // Lint for ${CLAUDE_PLUGIN_ROOT} in non-plugin hooks
    for hook in &config.capabilities.hooks {
        if let Some(cmd) = &hook.handler.command
            && cmd.contains("${CLAUDE_PLUGIN_ROOT}")
        {
            eprintln!(
                "{warn} Hook command references ${{CLAUDE_PLUGIN_ROOT}} but runs in top-level settings.json: {}",
                cmd
            );
            eprintln!(
                "{warn}   → ${{CLAUDE_PLUGIN_ROOT}} only works in plugin-scoped hooks/hooks.json files"
            );
            eprintln!("{warn}   → Move or rewrite this hook in your config or bundle YAML");
        }
    }

    // #1130 (silent-failure-hunter): `force_for_plugin` is only honored by
    // Claude Code for output styles shipped inside a plugin's own
    // `output-styles/` directory — set elsewhere it has no effect. llmenv
    // does not build the synthetic-plugin-promotion machinery that would
    // make it work outside a plugin context (unlike the LSP feature's
    // `LSP_PLUGIN_NAME` trick), so this is a global check (any plugin active
    // in this scope), not a per-bundle correlation. Reads from the merged
    // `doctor_manifest`, not raw `config.capabilities` — bundle-contributed
    // output styles (the common case) never appear on `Config` directly, so
    // checking the unmerged config would silently skip them.
    if let Some((manifest, _)) = &doctor_manifest {
        for style in &manifest.capabilities.output_styles {
            if style.force_for_plugin && manifest.capabilities.plugins.is_empty() {
                eprintln!(
                    "{warn} Output style '{}' sets force-for-plugin but no plugin is active \
                     in this scope: it has no effect outside a plugin's own output-styles/ \
                     directory",
                    style.name
                );
            }
        }
    }

    run_doctor_token_efficiency(use_color, &pass, &warn, cm_enabled, native_claude_env);

    run_doctor_tool_availability(use_color, &config);
    run_doctor_dependent_tools(use_color);

    // When context-mode is enabled, verify the marketplace clone exists so
    // inject_context_mode can actually resolve the plugin. A missing clone is
    // the most common reason the auto-wire looks correct in config but fails
    // at materialize time.
    if cm_enabled {
        let mkt_name = crate::config::CONTEXT_MODE_MARKETPLACE;
        let mkt_path = crate::plugins::cache::marketplace_path(&cache_dir, mkt_name);
        // #1436 variant: `exists()` would report an unreadable clone as
        // unsynced, sending the user to `plugin-sync` for a permissions problem.
        match mkt_path.try_exists() {
            Ok(true) => eprintln!("{pass} context-mode marketplace '{mkt_name}' synced and ready"),
            Ok(false) => eprintln!(
                "{warn} context-mode marketplace '{mkt_name}' not synced — \
                 run `llmenv plugin-sync` so the auto-wire can find it"
            ),
            Err(e) => eprintln!(
                "{warn} context-mode marketplace '{mkt_name}' could not be checked at {}: {e}",
                mkt_path.display(),
            ),
        }
    }

    // Verify pinned marketplaces: when a marketplace source includes a `#ref`
    // pin, the checked-out HEAD should match that pinned ref. Use `^{commit}`
    // dereferencing so annotated tags don't false-positive (#695).
    for m in &config.marketplace {
        let (_, pinned_ref) = cache::split_source_ref(&m.source);
        let Some(pinned_ref) = pinned_ref else {
            continue;
        };
        let mkt_path = cache::marketplace_path(&cache_dir, &m.name);
        // #1436 variant: an unstattable clone must not silently skip the pin
        // check — that reads as "pin verified" in the output.
        match mkt_path.join(".git").try_exists() {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                eprintln!(
                    "{warn} marketplace '{}' pin not verified — cannot check its clone at {}: {e}",
                    m.name,
                    mkt_path.display(),
                );
                continue;
            }
        }
        let Some(head) = cache::git_head(&mkt_path) else {
            // Clone exists but HEAD can't be resolved — let the `plugin-sync`
            // / materialize paths report the broken clone.
            continue;
        };
        let Some(pinned_sha) = cache::git_peeled_ref(&mkt_path, pinned_ref) else {
            eprintln!(
                "{warn} marketplace '{}' pinned to '{}' but that ref cannot be \
                 resolved in the local clone — run `llmenv plugin-sync` to repair",
                m.name, pinned_ref,
            );
            continue;
        };
        if head != pinned_sha {
            eprintln!(
                "{warn} marketplace '{}' pinned to '{}': HEAD ({}) does not match \
                 the pinned ref's commit ({}) — run `llmenv plugin-sync` to repair",
                m.name,
                pinned_ref,
                &head[..head.len().min(7)],
                &pinned_sha[..pinned_sha.len().min(7)],
            );
        }
    }

    report_credential_cache(&cache_dir, &pass, &info, &warn);

    eprintln!("{pass} Doctor check complete.");

    if gc {
        eprintln!("Running garbage collection...");
        match std::fs::metadata(&cache_dir) {
            Ok(meta) => {
                if meta.permissions().readonly() {
                    eprintln!("{warn} GC failed: cache directory is read-only");
                } else {
                    let cache_retention_hours = config.cache.cache_retention_hours.unwrap_or(168);
                    let retention = std::time::Duration::from_secs(cache_retention_hours * 3600);
                    match crate::materialize::cache::gc(&cache_dir, retention, config.cache.hashing)
                    {
                        Ok(report) => {
                            eprintln!(
                                "{pass} GC complete: removed {} entries, kept {}",
                                report.removed.len(),
                                report.kept
                            );
                            // #1372: a removal failure no longer aborts the walk,
                            // so the entries it skipped have to be reported here
                            // or they'd vanish from the summary entirely.
                            for p in &report.failed {
                                eprintln!("{warn} GC: could not remove {}", p.display());
                            }
                            let forgotten = forget_credentials_for(&report.removed);
                            if forgotten > 0 {
                                eprintln!(
                                    "{pass} GC: dropped {forgotten} orphaned OAuth credential \
                                     entries"
                                );
                            }
                        }
                        Err(e) => eprintln!("{warn} GC failed: {}", e),
                    }
                }
            }
            Err(e) => eprintln!("{warn} GC failed to stat cache directory: {}", e),
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
mod tests {
    use super::*;
    use crate::config::{
        Bundle, Capabilities, Features, Hook, HostEntry, Marketplace, Memory,
        NativePermissionRules, PermissionRule, Permissions, PluginCollection,
    };
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    // -- network_scope_cannot_match --

    // #1051: the matcher only evaluates gateway_mac; ssid/cidr are accepted
    // by the schema but never checked, so their presence alone can't save a
    // scope from being flagged.
    #[test]
    fn network_scope_cannot_match_without_gateway_mac() {
        use crate::config::NetworkMatch;
        for m in [
            NetworkMatch {
                gateway_mac: None,
                ssid: Some("MyWifi".into()),
                cidr: None,
            },
            NetworkMatch {
                gateway_mac: None,
                ssid: None,
                cidr: Some("10.0.0.0/24".into()),
            },
            NetworkMatch {
                gateway_mac: None,
                ssid: None,
                cidr: None,
            },
        ] {
            assert!(network_scope_cannot_match(&m), "{m:?} must be flagged");
        }
    }

    #[test]
    fn network_scope_can_match_with_gateway_mac() {
        use crate::config::NetworkMatch;
        let m = NetworkMatch {
            gateway_mac: Some("aa:bb:cc:dd:ee:ff".into()),
            ssid: None,
            cidr: None,
        };
        assert!(!network_scope_cannot_match(&m));
    }

    // -- memory_orphaned_by_disable_bundles --

    /// Config root with one bundle `b` whose `bundle.yaml` declares a memory
    /// backend, plus the `Config` selecting it and an active project scope that
    /// disables it.
    fn disabled_memory_bundle_fixture() -> (tempfile::TempDir, Config, crate::scope::ActiveScopes) {
        let root = tempfile::tempdir().unwrap();
        let bundle_dir = root.path().join("bundles").join("b");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join("bundle.yaml"),
            concat!(
                "features:\n",
                "  memory:\n",
                "    - server_host: still\n",
                "      port: 7878\n",
                "      when: [mytag]\n",
                "host:\n",
                "  still:\n",
                "    addr: still.local\n",
            ),
        )
        .unwrap();

        let config = Config {
            bundle: vec![Bundle {
                name: "b".into(),
                when: vec!["mytag".into()],
            }],
            ..Default::default()
        };
        let active = crate::scope::ActiveScopes {
            scopes: vec![crate::scope::ActiveScope {
                id: "project".into(),
                kind: "project",
                tags: vec![],
                project_root: None,
                enable_bundles: vec![],
                disable_bundles: vec!["b".into()],
                name: None,
                description: None,
                unknown_fields: vec![],
            }],
            tags: std::collections::BTreeSet::from(["mytag".to_string()]),
            ..Default::default()
        };
        (root, config, active)
    }

    // #1131: memory works in `~/` and silently stops the moment you `cd` into a
    // project that disables the only bundle supplying it — with a green doctor,
    // because every other check builds from the post-disable firing set.
    #[test]
    fn doctor_flags_memory_orphaned_by_disable_bundles() {
        let (root, config, active) = disabled_memory_bundle_fixture();
        assert_eq!(
            memory_orphaned_by_disable_bundles(
                &config,
                root.path(),
                &active,
                &Capabilities::default()
            ),
            vec!["b".to_string()]
        );
    }

    // An active source of memory means nothing is orphaned, even though the
    // same bundle is still disabled.
    #[test]
    fn doctor_does_not_flag_disabled_bundle_when_memory_is_active_anyway() {
        let (root, mut config, active) = disabled_memory_bundle_fixture();
        config.features = Some(Features {
            memory: vec![Memory {
                server_host: "still".into(),
                port: 7878,
                listen_host: "127.0.0.1".into(),
                when: vec!["mytag".into()],
                default_topics: vec![],
                default_type: None,
                default_importance: None,
                type_importance: BTreeMap::new(),
                retention: None,
                auto_prune: false,
                consolidation: None,
                mcp_permissions: None,
                wakeup_max_tokens: None,
            }],
            ..Default::default()
        });
        assert!(
            memory_orphaned_by_disable_bundles(
                &config,
                root.path(),
                &active,
                &Capabilities::default()
            )
            .is_empty(),
            "a top-level features.memory entry still supplies the backend"
        );
    }

    // #1140: a top-level features.memory entry exists but is gated on a tag
    // that isn't active — it supplies nothing for this scope, so it must not
    // mask the disabled bundle being the only *active* supplier. Before the
    // fix, `declares_memory` checked mere presence and returned empty here.
    #[test]
    fn doctor_still_flags_disabled_bundle_when_other_memory_entry_is_tag_inactive() {
        let (root, mut config, active) = disabled_memory_bundle_fixture();
        config.features = Some(Features {
            memory: vec![Memory {
                server_host: "elsewhere".into(),
                port: 7878,
                listen_host: "127.0.0.1".into(),
                when: vec!["othertag".into()],
                default_topics: vec![],
                default_type: None,
                default_importance: None,
                type_importance: BTreeMap::new(),
                retention: None,
                auto_prune: false,
                consolidation: None,
                mcp_permissions: None,
                wakeup_max_tokens: None,
            }],
            ..Default::default()
        });
        assert_eq!(
            memory_orphaned_by_disable_bundles(
                &config,
                root.path(),
                &active,
                &Capabilities::default()
            ),
            vec!["b".to_string()],
            "a tag-inactive features.memory entry supplies nothing and must not \
             mask the disabled bundle being the active scope's only supplier"
        );
    }

    // -- bundles_with_missing_dirs --

    #[test]
    fn bundles_missing_none_when_all_dirs_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(bundles_dir.join("home")).unwrap();
        std::fs::create_dir_all(bundles_dir.join("work")).unwrap();

        let bundles = vec![
            Bundle {
                name: "home".into(),
                when: vec!["local".into()],
            },
            Bundle {
                name: "work".into(),
                when: vec!["office".into()],
            },
        ];
        let missing = bundles_with_missing_dirs(&bundles, &bundles_dir).unwrap();
        assert!(missing.is_empty(), "expected empty: {missing:?}");
    }

    #[test]
    fn bundles_missing_reports_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(bundles_dir.join("existing")).unwrap();

        let bundles = vec![
            Bundle {
                name: "existing".into(),
                when: vec!["x".into()],
            },
            Bundle {
                name: "missing".into(),
                when: vec!["y".into()],
            },
        ];
        let mut missing = bundles_with_missing_dirs(&bundles, &bundles_dir).unwrap();
        missing.sort_unstable();
        assert_eq!(missing, vec!["missing"]);
    }

    // #1436: an unreadable bundles dir must not be reported as "bundle
    // directory does not exist" — that sends the user looking for a missing
    // folder that is actually right there, just unstattable.
    #[cfg(unix)]
    #[test]
    fn bundles_with_missing_dirs_propagates_stat_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let bundles_dir = tmp.path().join("bundles");
        std::fs::create_dir_all(bundles_dir.join("home")).unwrap();
        std::fs::set_permissions(&bundles_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let bundles = vec![Bundle {
            name: "home".into(),
            when: vec!["local".into()],
        }];
        let result = bundles_with_missing_dirs(&bundles, &bundles_dir);
        let readable_anyway = std::fs::metadata(bundles_dir.join("home")).is_ok();
        std::fs::set_permissions(&bundles_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "unreadable bundles dir must surface the stat error, got {result:?}"
        );
    }

    // -- cached_version_folders --

    #[test]
    fn cached_version_folders_none_when_adapter_cache_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            cached_version_folders(&tmp.path().join("nope"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cached_version_folders_strips_content_hash_suffix_and_skips_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("claude_code");
        std::fs::create_dir_all(cache.join(format!("v3-{}", "0123456789abcdef".repeat(4))))
            .unwrap();
        std::fs::create_dir_all(cache.join(format!("v3-{}", "a".repeat(64)))).unwrap();
        std::fs::create_dir_all(cache.join("v4")).unwrap();
        std::fs::create_dir_all(cache.join("v9.tmp")).unwrap();
        std::fs::write(cache.join("stray-file"), b"").unwrap();

        let mut versions = cached_version_folders(&cache).unwrap().unwrap();
        versions.sort();
        versions.dedup();
        assert_eq!(versions, vec!["v3".to_string(), "v4".to_string()]);
    }

    // #1436: the skew scan used to swallow every read error, so an unreadable
    // adapter cache silently reported "no cached builds" instead of "could not
    // check". The caller turns this error into a `warn` line.
    #[cfg(unix)]
    #[test]
    fn cached_version_folders_propagates_permission_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("claude_code");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = cached_version_folders(&cache);
        let readable_anyway = std::fs::read_dir(&cache).is_ok();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable_anyway {
            return; // running as root / FS ignores perms — can't exercise EACCES
        }
        assert!(
            result.is_err(),
            "unreadable adapter cache must surface the error, got {result:?}"
        );
    }

    // -- unused_marketplaces --

    #[test]
    fn unused_marketplaces_none_when_all_referenced() {
        let config = Config {
            marketplace: vec![Marketplace {
                name: "official".into(),
                source: "https://example.com".into(),
            }],
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec![],
                plugins: vec!["official:some-plugin".into()],
            }],
            ..Config::default()
        };
        let unused = unused_marketplaces(&config);
        assert!(unused.is_empty(), "expected empty: {unused:?}");
    }

    #[test]
    fn unused_marketplaces_reports_unreferenced() {
        let config = Config {
            marketplace: vec![
                Marketplace {
                    name: "used".into(),
                    source: "https://a.com".into(),
                },
                Marketplace {
                    name: "unused".into(),
                    source: "https://b.com".into(),
                },
            ],
            plugin_collection: vec![PluginCollection {
                name: "core".into(),
                when: vec![],
                plugins: vec!["used:plugin-a".into()],
            }],
            ..Config::default()
        };
        let mut unused = unused_marketplaces(&config);
        unused.sort_unstable();
        assert_eq!(unused, vec!["unused"]);
    }

    // -- orphan_codebase_memory_entries --

    #[test]
    fn orphan_codebase_memory_none_when_tag_emitted() {
        let config = Config {
            features: Some(crate::config::Features {
                codebase_memory: vec![crate::config::CodebaseMemory {
                    when: vec!["proj".to_string()],
                    index_path: None,
                    mcp_permissions: None,
                }],
                ..Default::default()
            }),
            ..Config::default()
        };
        let emitted: HashSet<String> = ["proj".to_string()].into_iter().collect();
        let bundle_caps = Capabilities::default();
        let orphans = orphan_codebase_memory_entries(&config, &bundle_caps, &emitted);
        assert!(orphans.is_empty(), "expected empty: {orphans:?}");
    }

    #[test]
    fn orphan_codebase_memory_reports_unreachable_tag() {
        let config = Config {
            features: Some(crate::config::Features {
                codebase_memory: vec![crate::config::CodebaseMemory {
                    when: vec!["never-emitted".to_string()],
                    index_path: None,
                    mcp_permissions: None,
                }],
                ..Default::default()
            }),
            ..Config::default()
        };
        let bundle_caps = Capabilities::default();
        let emitted = HashSet::new();
        let orphans = orphan_codebase_memory_entries(&config, &bundle_caps, &emitted);
        assert_eq!(orphans, vec![&["never-emitted".to_string()][..]]);
    }

    #[test]
    fn orphan_codebase_memory_checks_bundle_contributed_entries() {
        let bundle_caps = Capabilities {
            features: Some(crate::config::Features {
                codebase_memory: vec![crate::config::CodebaseMemory {
                    when: vec!["bundle-tag".to_string()],
                    index_path: None,
                    mcp_permissions: None,
                }],
                ..Default::default()
            }),
            ..Capabilities::default()
        };
        let config = Config::default();
        let emitted = HashSet::new();
        let orphans = orphan_codebase_memory_entries(&config, &bundle_caps, &emitted);
        assert_eq!(orphans, vec![&["bundle-tag".to_string()][..]]);
    }

    // -- claude_only_colon_permission_patterns --

    fn rule(tool: &str, pattern: Option<&str>, paths: Vec<String>) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            pattern: pattern.map(Into::into),
            paths,
        }
    }

    fn caps_with_allow(tool: &str, pattern: &str) -> Capabilities {
        Capabilities {
            permissions: Permissions {
                allow: vec![rule(tool, Some(pattern), vec![])],
                ..Default::default()
            },
            ..Capabilities::default()
        }
    }

    /// `"<tool>(<pattern>)"` strings, for comparing against expected rules
    /// without spelling out the tier on every assertion.
    fn rule_strings(found: &[ColonPrefixRule]) -> Vec<&str> {
        found.iter().map(|r| r.rule.as_str()).collect()
    }

    #[test]
    fn colon_syntax_detected_for_subcommand_and_field_forms() {
        for pattern in [
            "git commit:*",
            "rg:*",
            "domain:example.com",
            "url:https://example.com/x",
            "npm run:*",
            // A quoted or globbed token before the colon is still Claude's
            // prefix syntax — the old "word char before the colon" heuristic
            // failed open on both.
            "\"wip\":*",
            "*:*",
        ] {
            assert!(
                uses_colon_prefix_syntax(pattern),
                "expected colon-prefix: {pattern}"
            );
        }
    }

    #[test]
    fn colon_syntax_not_detected_for_portable_patterns() {
        for pattern in [
            "git commit *",
            "*",
            "rg *",
            "https://example.com/*",
            "src/**/*.rs",
            "mcp__server__tool",
            ":leading-colon",
            "",
            ":",
            // Literal colons that mean the same thing to both engines. The old
            // heuristic flagged every one of these and told the user to rewrite
            // a pattern that was already correct.
            "docker run -p 8080:8080 *",
            "kubectl port-forward svc/api 8080:80",
            "awk -F: *",
            "git log --pretty=format:%h *",
            "curl http://localhost:3000/*",
        ] {
            assert!(
                !uses_colon_prefix_syntax(pattern),
                "unexpected colon-prefix: {pattern}"
            );
        }
    }

    #[test]
    fn colon_patterns_flagged_when_opencode_active() {
        let caps = caps_with_allow("Bash", "git commit:*");
        let found = claude_only_colon_permission_patterns(&caps, true);
        assert_eq!(rule_strings(&found), vec!["Bash(git commit:*)"]);
        assert_eq!(found[0].tier, "allow");
    }

    // #1076: a permission pattern can arrive from a shared bundle.yaml — an
    // ANSI escape in it must not reach the terminal verbatim via the
    // rendered rule string, or a bundle author could rewrite/hide doctor's
    // other output.
    #[test]
    fn colon_pattern_rule_string_escapes_control_characters() {
        let caps = caps_with_allow("Bash", "evil\x1b[2K:*");
        let found = claude_only_colon_permission_patterns(&caps, true);
        assert_eq!(found.len(), 1);
        assert!(!found[0].rule.contains('\x1b'), "{}", found[0].rule);
        assert!(found[0].rule.contains("\\u{001b}"), "{}", found[0].rule);
    }

    #[test]
    fn colon_patterns_silent_when_opencode_inactive() {
        let caps = caps_with_allow("Bash", "git commit:*");
        assert!(
            claude_only_colon_permission_patterns(&caps, false).is_empty(),
            "Claude-only patterns are correct when opencode won't be materialized"
        );
    }

    #[test]
    fn colon_patterns_silent_for_portable_pattern_with_opencode_active() {
        let caps = caps_with_allow("Bash", "git commit *");
        assert!(claude_only_colon_permission_patterns(&caps, true).is_empty());
    }

    #[test]
    fn colon_patterns_cover_ask_and_deny_tiers() {
        let caps = Capabilities {
            permissions: Permissions {
                ask: vec![rule("Bash", Some("docker run:*"), vec![])],
                deny: vec![rule("WebFetch", Some("domain:evil.test"), vec![])],
                ..Default::default()
            },
            ..Capabilities::default()
        };
        let found = claude_only_colon_permission_patterns(&caps, true);
        assert_eq!(
            rule_strings(&found),
            vec!["Bash(docker run:*)", "WebFetch(domain:evil.test)"]
        );
        assert_eq!(found[0].tier, "ask");
        assert_eq!(found[1].tier, "deny");
    }

    /// A dead `deny` fails open, so its diagnostic must say the deny is not
    /// enforced rather than reuse the allow-tier "grant" wording.
    #[test]
    fn deny_tier_message_says_the_deny_is_not_enforced() {
        let deny = ColonPrefixRule {
            tier: "deny",
            rule: "WebFetch(domain:evil.test)".into(),
        };
        let msg = deny.message();
        assert!(msg.contains("NOT enforced"), "{msg}");

        let allow = ColonPrefixRule {
            tier: "allow",
            rule: "Bash(git commit:*)".into(),
        };
        assert!(!allow.message().contains("NOT enforced"), "{msg}");
    }

    /// `paths` entries are file paths, not command patterns — a colon in one is
    /// not Claude's subcommand syntax and must not be flagged.
    #[test]
    fn colon_patterns_ignore_path_rules() {
        let caps = Capabilities {
            permissions: Permissions {
                allow: vec![rule("Read", None, vec!["notes/todo:urgent.md".into()])],
                ..Default::default()
            },
            ..Capabilities::default()
        };
        assert!(claude_only_colon_permission_patterns(&caps, true).is_empty());
    }

    /// `native_permissions.claude_code` is deliberately Claude-scoped, so its
    /// colon-prefix rules are correct config and must stay unflagged.
    #[test]
    fn colon_patterns_ignore_claude_scoped_native_permissions() {
        let caps = Capabilities {
            native_permissions: BTreeMap::from([(
                "claude_code".into(),
                NativePermissionRules {
                    allow: vec!["Bash(git commit:*)".into()],
                    ..Default::default()
                },
            )]),
            ..Capabilities::default()
        };
        assert!(claude_only_colon_permission_patterns(&caps, true).is_empty());
    }

    proptest! {
        /// A strict cache folder is `{version}-{64 hex}`, so whatever version
        /// tag llmenv stamped, scanning recovers it exactly — including tags
        /// that themselves contain `-`, which `rsplit_once` has to get right.
        #[test]
        fn cached_version_folders_recovers_any_version_tag(
            version in "[A-Za-z0-9][-.A-Za-z0-9]{0,20}",
            hash in "[0-9a-f]{64}",
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let cache = tmp.path().join("engine");
            std::fs::create_dir_all(cache.join(format!("{version}-{hash}"))).unwrap();
            prop_assert_eq!(
                cached_version_folders(&cache).unwrap(),
                Some(vec![version])
            );
        }

        /// A folder name that is not `{something}-{64 hex}` is the version tag
        /// itself, so it survives the scan untouched.
        #[test]
        fn cached_version_folders_keeps_a_plain_version_folder_verbatim(
            version in "[A-Za-z0-9][.A-Za-z0-9]{0,20}",
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let cache = tmp.path().join("engine");
            std::fs::create_dir_all(cache.join(&version)).unwrap();
            prop_assert_eq!(
                cached_version_folders(&cache).unwrap(),
                Some(vec![version])
            );
        }

        /// `.tmp` folders are half-written builds, never a version to compare
        /// the running binary against.
        #[test]
        fn cached_version_folders_always_skips_tmp_folders(
            version in "[A-Za-z0-9][.A-Za-z0-9]{0,20}",
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let cache = tmp.path().join("engine");
            std::fs::create_dir_all(cache.join(format!("{version}.tmp"))).unwrap();
            prop_assert_eq!(cached_version_folders(&cache).unwrap(), Some(vec![]));
        }

        /// Never panics, whatever a config author writes — patterns are
        /// arbitrary user strings, including non-ASCII and lone colons.
        #[test]
        fn colon_syntax_never_panics(pattern in ".*") {
            let _ = uses_colon_prefix_syntax(&pattern);
        }

        /// A pattern with no colon at all can never be colon-prefix syntax.
        #[test]
        fn colon_syntax_false_without_a_colon(pattern in "[^:]*") {
            prop_assert!(!uses_colon_prefix_syntax(&pattern));
        }

        /// The command-prefix form is exactly "ends with `:*`", so appending
        /// `:*` to any pattern makes it colon-prefix syntax.
        #[test]
        fn colon_syntax_true_for_any_colon_star_suffix(prefix in ".*") {
            let pattern = format!("{prefix}:*");
            prop_assert!(uses_colon_prefix_syntax(&pattern));
        }

        /// A trailing `*` alone must not be enough — only the `:*` pair counts,
        /// so a pattern whose colon is followed by anything else stays portable.
        #[test]
        fn colon_syntax_false_for_space_separated_glob(cmd in "[a-z]{1,8}", sub in "[a-z]{1,8}") {
            let pattern = format!("{cmd} {sub} *");
            prop_assert!(!uses_colon_prefix_syntax(&pattern));
        }
    }

    // -- unknown_neutral_tools / inexact_tool_mappings (#1371) --

    #[test]
    fn flags_a_tool_name_outside_the_vocabulary() {
        // The reported case: `Create` is not a tool on any engine llmenv maps.
        let caps = caps_with_allow("Create", ".tmp/**");
        let found = unknown_neutral_tools(&caps);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].tool, "Create");
        let msg = found[0].message();
        assert!(msg.contains("Create"), "{msg}");
        assert!(
            msg.contains("Bash"),
            "the diagnostic must list the valid names: {msg}"
        );
    }

    #[test]
    fn known_tool_names_are_not_flagged() {
        assert!(unknown_neutral_tools(&caps_with_allow("Bash", "ls *")).is_empty());
        // Mapped for opencode but not crush — still a known neutral name, so
        // this check stays quiet and the docs cover the per-engine gap.
        assert!(unknown_neutral_tools(&caps_with_allow("Task", "*")).is_empty());
    }

    #[test]
    fn repeated_unknown_tool_is_reported_once() {
        // Why this check can print from `export`/`regenerate` at all: the
        // per-rule warning it replaced fired once per rule *per adapter*, so two
        // rules naming the same bad tool produced four lines (#1371).
        let caps = Capabilities {
            permissions: Permissions {
                allow: vec![
                    rule("Create", Some(".tmp/**"), vec![]),
                    rule("Create", Some("./.tmp/**"), vec![]),
                ],
                deny: vec![rule("Create", None, vec![])],
                ..Default::default()
            },
            ..Capabilities::default()
        };
        let found = unknown_neutral_tools(&caps);
        assert_eq!(found.len(), 1, "one problem to fix, one line");
    }

    #[test]
    fn inexact_mapping_is_reported_only_for_an_active_engine() {
        // `Write` reaches opencode as `edit`, which also covers `Edit`.
        let caps = caps_with_allow("Write", "./src/**");
        let found = inexact_tool_mappings(&caps, &["opencode"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].engine, "opencode");
        assert!(found[0].note.contains("edit"), "{}", found[0].note);

        // crush renders `Write` as its own `write` tool, so with only crush
        // active there's nothing to say.
        assert!(inexact_tool_mappings(&caps, &["crush"]).is_empty());
        assert!(inexact_tool_mappings(&caps, &[]).is_empty());
    }

    #[test]
    fn inexact_mapping_covers_a_dropped_tool() {
        // `Task` has no crush equivalent — the rule is dropped there, which is
        // exactly what a user needs told.
        let found = inexact_tool_mappings(&caps_with_allow("Task", "*"), &["crush"]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].engine, "crush");
    }

    #[test]
    fn unknown_tool_is_not_reported_as_an_inexact_mapping() {
        // The two checks must not double-report: an unknown name has no table
        // entry, so it belongs to `unknown_neutral_tools` alone.
        let caps = caps_with_allow("Create", ".tmp/**");
        assert!(inexact_tool_mappings(&caps, &["opencode", "crush"]).is_empty());
    }

    // -- legacy_tools_missing_replacement --

    #[test]
    fn flags_grep_without_rg() {
        let caps = caps_with_allow("Bash", "grep -r *");
        let found = legacy_tools_missing_replacement(&caps);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].legacy, "grep");
        assert_eq!(found[0].replacement, "rg");
    }

    #[test]
    fn flags_find_without_fd() {
        let caps = caps_with_allow("Bash", "find *");
        let found = legacy_tools_missing_replacement(&caps);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].legacy, "find");
        assert_eq!(found[0].replacement, "fd");
    }

    #[test]
    fn silent_when_replacement_already_allowed() {
        let caps = Capabilities {
            permissions: Permissions {
                allow: vec![
                    rule("Bash", Some("grep -r *"), vec![]),
                    rule("Bash", Some("rg *"), vec![]),
                ],
                ..Default::default()
            },
            ..Capabilities::default()
        };
        assert!(legacy_tools_missing_replacement(&caps).is_empty());
    }

    #[test]
    fn silent_when_neither_legacy_nor_replacement_allowed() {
        assert!(legacy_tools_missing_replacement(&Capabilities::default()).is_empty());
    }

    #[test]
    fn cat_is_not_flagged_no_named_replacement() {
        // cat has no explicit replacement in this project's own bundled rules
        // — only pairs the AGENTS.md table names outright (grep->rg, find->fd)
        // are in scope for this lint.
        let caps = caps_with_allow("Bash", "cat *");
        assert!(legacy_tools_missing_replacement(&caps).is_empty());
    }

    #[test]
    fn does_not_match_substring_of_a_longer_command() {
        // "grepper *" is not "grep" — the leading token must match exactly.
        let caps = caps_with_allow("Bash", "grepper *");
        assert!(legacy_tools_missing_replacement(&caps).is_empty());
    }

    #[test]
    fn non_bash_tool_rules_are_ignored() {
        let caps = caps_with_allow("Read", "grep");
        assert!(legacy_tools_missing_replacement(&caps).is_empty());
    }

    /// #975 pre-pr-review finding: a replacement granted only through
    /// `native_permissions.<engine>.allow` (a documented, exercised pattern
    /// — `examples/config-llmenv-dir/config.yaml`) must still silence the
    /// warning, not just a neutral `permissions.allow` entry.
    #[test]
    fn silent_when_replacement_allowed_via_native_permissions() {
        let mut caps = caps_with_allow("Bash", "grep -r *");
        caps.native_permissions.insert(
            "claude_code".into(),
            NativePermissionRules {
                allow: vec!["Bash(rg *)".into()],
                ..Default::default()
            },
        );
        assert!(legacy_tools_missing_replacement(&caps).is_empty());
    }

    #[test]
    fn both_legacy_tools_flagged_independently() {
        let caps = Capabilities {
            permissions: Permissions {
                allow: vec![
                    rule("Bash", Some("grep -r *"), vec![]),
                    rule("Bash", Some("find *"), vec![]),
                ],
                ..Default::default()
            },
            ..Capabilities::default()
        };
        let found = legacy_tools_missing_replacement(&caps);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn message_names_both_tools() {
        let rule = LegacyToolRule {
            legacy: "grep",
            replacement: "rg",
        };
        let msg = rule.message();
        assert!(msg.contains("grep"), "{msg}");
        assert!(msg.contains("rg"), "{msg}");
    }

    proptest! {
        /// Never panics on arbitrary pattern text — patterns are user-authored
        /// config strings, including non-ASCII, empty, and colon-only input.
        #[test]
        fn pattern_command_never_panics(pattern in ".*") {
            let _ = pattern_command(&pattern);
        }

        /// Never panics on arbitrary native-permission strings, including
        /// malformed ones missing the `Bash(...)` wrapper entirely.
        #[test]
        fn native_bash_command_never_panics(raw in ".*") {
            let _ = native_bash_command(&raw);
        }

        /// A well-formed `Bash(<pattern>)` string round-trips to the same
        /// command `pattern_command` would extract from `<pattern>` directly
        /// — the native-permission parser must agree with the neutral one.
        #[test]
        fn native_bash_command_matches_pattern_command_for_wrapped_input(
            pattern in "[a-zA-Z][a-zA-Z0-9_-]{0,10}( [^()]{0,20})?"
        ) {
            let raw = format!("Bash({pattern})");
            prop_assert_eq!(native_bash_command(&raw), Some(pattern_command(&pattern)));
        }
    }

    // -- hooks_with_glob_like_matchers --

    fn hook_with_matcher(event: &str, matcher: &str) -> Hook {
        Hook {
            event: event.into(),
            matcher: Some(matcher.into()),
            handler: crate::config::HookHandler {
                kind: crate::config::HookHandlerKind::Command,
                command: Some("echo hi".into()),
                tool: None,
            },
            bundle_origin: None,
        }
    }

    #[test]
    fn glob_matchers_flags_file_extension_glob() {
        let config = Config {
            capabilities: Capabilities {
                hooks: vec![hook_with_matcher("PreToolUse", "*.rs")],
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let flagged = hooks_with_glob_like_matchers(&config);
        assert_eq!(flagged, vec!["PreToolUse (matcher: '*.rs')".to_string()]);
    }

    #[test]
    fn glob_matchers_accepts_known_tool_name_alternation() {
        let config = Config {
            capabilities: Capabilities {
                hooks: vec![hook_with_matcher("PreToolUse", "^(Write|Edit|MultiEdit)$")],
                ..Capabilities::default()
            },
            ..Config::default()
        };
        let flagged = hooks_with_glob_like_matchers(&config);
        assert!(flagged.is_empty(), "expected empty: {flagged:?}");
    }

    // -- is_local_addr --

    #[test]
    fn is_local_addr_accepts_localhost() {
        assert!(is_local_addr("localhost"));
    }

    #[test]
    fn is_local_addr_accepts_ipv4_loopback() {
        assert!(is_local_addr("127.0.0.1"));
    }

    #[test]
    fn is_local_addr_accepts_ipv6_loopback() {
        assert!(is_local_addr("::1"));
    }

    #[test]
    fn is_local_addr_rejects_remote_ip() {
        assert!(!is_local_addr("10.0.0.4"));
    }

    #[test]
    fn is_local_addr_rejects_hostname() {
        assert!(!is_local_addr("still.local"));
    }

    #[test]
    fn is_local_addr_accepts_ipv6_unspecified() {
        assert!(is_local_addr("::"));
        assert!(is_local_addr("::0"));
    }

    #[test]
    fn is_local_addr_accepts_broader_loopback() {
        assert!(is_local_addr("127.0.0.2"));
        assert!(is_local_addr("127.255.255.254"));
        assert!(!is_local_addr("128.0.0.1"));
    }

    #[test]
    fn is_local_addr_accepts_unspecified_v4() {
        assert!(is_local_addr("0.0.0.0"));
    }

    // -- run_doctor_tool_availability --

    #[test]
    fn tool_avail_no_crash_default_config() {
        let config = Config::default();
        // Should not panic: checks claude + crush (both may warn), no memory entries
        run_doctor_tool_availability(false, &config);
    }

    #[test]
    fn tool_avail_no_crash_with_memory() {
        let config = Config {
            features: Some(Features {
                memory: vec![Memory {
                    server_host: "local".into(),
                    port: 4343,
                    listen_host: "127.0.0.1".into(),
                    when: vec!["local".into()],
                    default_topics: vec![],
                    default_type: None,
                    default_importance: None,
                    type_importance: BTreeMap::new(),
                    retention: None,
                    auto_prune: false,
                    consolidation: None,
                    mcp_permissions: None,
                    wakeup_max_tokens: None,
                }],
                ..Features::default()
            }),
            host: BTreeMap::from([(
                "local".into(),
                HostEntry {
                    addr: "127.0.0.1".into(),
                },
            )]),
            ..Config::default()
        };
        run_doctor_tool_availability(false, &config);
    }

    #[test]
    fn tool_avail_no_crash_with_remote_memory() {
        let config = Config {
            features: Some(Features {
                memory: vec![Memory {
                    server_host: "remote".into(),
                    port: 4343,
                    listen_host: "0.0.0.0".into(),
                    when: vec!["remote".into()],
                    default_topics: vec![],
                    default_type: None,
                    default_importance: None,
                    type_importance: BTreeMap::new(),
                    retention: None,
                    auto_prune: false,
                    consolidation: None,
                    mcp_permissions: None,
                    wakeup_max_tokens: None,
                }],
                ..Features::default()
            }),
            host: BTreeMap::from([(
                "remote".into(),
                HostEntry {
                    addr: "10.0.0.4".into(),
                },
            )]),
            ..Config::default()
        };
        run_doctor_tool_availability(false, &config);
    }

    #[test]
    fn tool_avail_no_crash_claude_disabled() {
        let config = Config {
            disabled_engines: vec!["claude_code".into()],
            ..Config::default()
        };
        run_doctor_tool_availability(false, &config);
    }

    // #1185: the two tools differ in what llmenv can actually do for them —
    // icm installs its own update, codebase-memory-mcp only prints how. The
    // report has to say which, or it claims something false about one of them.
    #[test]
    fn dependent_tool_update_paths_distinguish_self_apply_from_report_only() {
        let by_name = |name: &str| {
            DEPENDENT_TOOLS
                .iter()
                .find(|(bin, _)| *bin == name)
                .map(|(_, u)| *u)
                .expect("tool listed")
        };
        assert!(matches!(by_name("icm"), UpdatePath::SelfApply(_)));
        assert!(matches!(
            by_name("codebase-memory-mcp"),
            UpdatePath::Reports(_)
        ));
        for (bin, update) in DEPENDENT_TOOLS {
            assert!(
                update.command().starts_with(bin),
                "{bin}'s update command should invoke it, got {}",
                update.command()
            );
        }
    }

    #[test]
    fn parse_version_line_reads_the_trailing_token_and_rejects_noise() {
        // The real shapes, as reported by the installed binaries.
        assert_eq!(
            parse_version_line("icm 0.10.61\n", "icm").as_deref(),
            Some("0.10.61")
        );
        assert_eq!(
            parse_version_line("codebase-memory-mcp 0.10.2\n", "codebase-memory-mcp").as_deref(),
            Some("0.10.2")
        );
        // Only the first line matters — a tool that prints a banner after it
        // must not have the banner's last word read as a version.
        assert_eq!(
            parse_version_line("icm 1.2.3\nbuilt from source\n", "icm").as_deref(),
            Some("1.2.3")
        );
        for junk in ["", "\n", "icm", "some words with no digits"] {
            assert_eq!(
                parse_version_line(junk, "icm"),
                None,
                "{junk:?} is not a version"
            );
        }
    }

    #[test]
    fn tool_version_of_a_missing_binary_is_none_not_a_panic() {
        assert_eq!(tool_version("this-binary-does-not-exist-llmenv"), None);
    }
}
