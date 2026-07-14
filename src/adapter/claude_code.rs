use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::json;

use super::AgentAdapter;
use super::resolve_bundle_relative_paths;
use super::resolve_command_paths_against_files;
use super::skills::{create_dir_owner_only, reject_hardcoded_config_path};
use crate::mcp::resolve::MEMORY_MCP_NAME;
use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
use crate::merge::MergedManifest;
use crate::plugins::resolve::ResolvedMarketplace;
use crate::util::{dedup, merge_json};

/// Engine identifier baked into hook command lines so subcommands invoked by
/// Command the auto-emitted SessionStart hook runs (#121/#85). It shells back
/// into `llmenv` so the runtime check can compare the booted content hash (the
/// `CLAUDE_CONFIG_DIR` folder name the session launched with) against what
/// llmenv would materialize now, and warn the user to restart on drift. Kept as
/// a bare command (resolved off `PATH`) so it works regardless of install dir.
const STALE_CHECK_COMMAND: &str = "llmenv check-stale --engine claude_code";

/// Command the auto-emitted SessionStart hook runs to inject source config paths
/// into agent context (#289). Outputs `hookSpecificOutput.additionalContext` JSON
/// so the agent always knows where to edit config rather than touching the cache.
const CONFIG_CONTEXT_COMMAND: &str = "llmenv config-context --engine claude_code";

/// Command the auto-emitted PreToolUse hook runs to guard against writes to the
/// managed cache directory (#289). Reads the Write/Edit/MultiEdit tool call from
/// stdin and prints a redirection hint if the target is a cache path. Exits 0
/// (fail-soft) so the write still proceeds; the hint keeps agents oriented.
const CONFIG_GUARD_COMMAND: &str = "llmenv config-guard --engine claude_code";

/// Command the auto-emitted throttle hooks run. Throttle hooks fire on
/// PreToolUse and UserPromptSubmit to poll the usage backend and sleep a
/// capped adaptive delay to avoid rate limits.
const THROTTLE_COMMAND: &str = "llmenv throttle";

/// Prefix of the auto-emitted lifecycle/session-log hook commands. The full
/// command is `HOOK_RUN_COMMAND <neutral_event>`, e.g.
/// `llmenv hook-run --engine claude_code session_start`. Dispatches ICM memory
/// wake-up/store (#197/#228) and, per `session_log` config, the session-log
/// file/transcript sinks (#382). Always fail-soft (exit 0).
const HOOK_RUN_COMMAND: &str = "llmenv hook-run --engine claude_code";

/// #317: fragment appended to CLAUDE.md when slippage control is enabled with
/// compact_survival. Guides agent behavior after context compaction.
const COMPACT_SURVIVAL_FRAGMENT: &str = concat!(
    "# Compaction Survival Guide\n",
    "\n",
    "After context compaction (memory summarization), rules and instructions\n",
    "from earlier may be lost. Before acting on any task:\n",
    "\n",
    "1. Re-read the generated CLAUDE.md and settings files to restore rules.\n",
    "2. Verify your understanding of the current state — don't assume prior\n",
    "   context survived compaction.\n",
    "3. State your key assumptions before executing commands.\n",
    "4. Use the available tools to re-gather context if needed.\n",
    "\n",
    "Slippage control layers (read-before-edit, self-critique) remain active\n",
    "across compactions to catch gaps your restored context might miss.\n",
);

/// `(engine-neutral event, native Claude event)` pairs for the always-on
/// baseline hooks. Registered unconditionally — `hook-run` itself no-ops
/// cheaply when neither memory nor session logging is configured — so this
/// also closes the long-standing gap where `hook-run` existed but was never
/// wired into settings.json (memory wake-up/store never fired). Continuous
/// per-prompt memory recall (`turn_start` / `UserPromptSubmit`, #499) is wired
/// separately in `generate_settings_json`, gated on `icm_active` rather than
/// unconditional like these two (performance-sensitive: runs on every prompt).
const BASELINE_HOOK_EVENTS: &[(&str, &str)] = &[
    ("session_start", "SessionStart"),
    ("session_end", "SessionEnd"),
];

/// `(engine-neutral event, native Claude event)` pairs registered when any
/// session-log sink is enabled — per-hook prompt/tool-use capture (#382).
const SESSION_LOG_HOOK_EVENTS: &[(&str, &str)] = &[
    ("user_prompt_submit", "UserPromptSubmit"),
    ("pre_tool_use", "PreToolUse"),
    ("post_tool_use", "PostToolUse"),
    ("notification", "Notification"),
    ("stop", "Stop"),
    ("subagent_stop", "SubagentStop"),
    ("pre_compact", "PreCompact"),
];

/// #694: Built-in ICM MCP server tool tiers.
/// Read-only tools → allow, mutation tools → ask, destructive → deny.
const ICM_READ_ONLY: &[&str] = &[
    "icm_wake_up",
    "icm_memory_recall",
    "icm_memory_stats",
    "icm_memory_health",
    "icm_memory_list_topics",
    "icm_feedback_stats",
    "icm_feedback_search",
    "icm_transcript_search",
    "icm_transcript_stats",
    "icm_transcript_show",
    "icm_memoir_search",
    "icm_memoir_search_all",
    "icm_memoir_show",
    "icm_memoir_inspect",
    "icm_memoir_list",
];

const ICM_MUTATION: &[&str] = &[
    "icm_memory_store",
    "icm_memory_update",
    "icm_memory_consolidate",
    "icm_memory_embed_all",
    "icm_memory_extract_patterns",
    "icm_learn",
    "icm_transcript_start_session",
    "icm_transcript_record",
    "icm_feedback_record",
    "icm_memoir_create",
    "icm_memoir_add_concept",
    "icm_memoir_export",
    "icm_memoir_refine",
    "icm_memoir_link",
];

const ICM_DESTRUCTIVE: &[&str] = &["icm_memory_forget", "icm_memory_forget_topic"];

/// #694: Built-in context-mode MCP plugin tool tiers (without the common prefix).
const CTX_READ_ONLY: &[&str] = &["ctx_search", "ctx_stats", "ctx_doctor", "ctx_insight"];

const CTX_MUTATION: &[&str] = &[
    "ctx_index",
    "ctx_execute",
    "ctx_execute_file",
    "ctx_fetch_and_index",
    "ctx_batch_execute",
];

const CTX_DESTRUCTIVE: &[&str] = &["ctx_purge", "ctx_upgrade"];

/// Adapter for Claude Code: writes `CLAUDE.md` (from `agents_md`) and copies
/// all merged files into `out`. Sets `CLAUDE_CONFIG_DIR` so Claude Code uses
/// `out` as its config root.
///
/// Skills are structured as directories with a `SKILL.md` file containing YAML
/// frontmatter (at minimum `name` and `description`).
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeCodeAdapter;

/// Native hook events that Claude Code actually emits. Kept as a named
/// constant so `supported_hook_events()` and callers that gate on this set
/// share a single source of truth.
const CLAUDE_CODE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "SubagentStop",
    "PreCompact",
];

impl AgentAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn binary_name(&self) -> &'static str {
        "claude"
    }

    fn supports_plugins(&self) -> bool {
        true
    }

    fn supports_lsp(&self) -> bool {
        true
    }

    fn supports_model_providers(&self) -> bool {
        false
    }

    fn supported_hook_events(&self) -> &'static [&'static str] {
        CLAUDE_CODE_HOOK_EVENTS
    }

    fn env_vars(
        &self,
        cache_dir: &Path,
        state_dir: &Path,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        let mut vars = vec![("CLAUDE_CONFIG_DIR".into(), dir.to_owned())];

        // Per-hash temp dir: CLAUDE_CODE_TMPDIR + standard POSIX temp vars for
        // subprocess isolation. Claude Code appends /claude-{uid}/ to the value
        // on Unix; the tmp/ folder is cleaned when the parent hash dir is pruned.
        let tmp_dir = cache_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        let tmp_str = tmp_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "cache_dir tmp dir is not valid UTF-8: {}",
                tmp_dir.display()
            )
        })?;
        vars.push(("CLAUDE_CODE_TMPDIR".into(), tmp_str.to_owned()));
        vars.push(("TMPDIR".into(), tmp_str.to_owned()));
        vars.push(("TMP".into(), tmp_str.to_owned()));
        vars.push(("TEMP".into(), tmp_str.to_owned()));

        // Durable plugin root in the state dir (#632): despite the misleading
        // "CACHE" in its name, CLAUDE_CODE_PLUGIN_CACHE_DIR controls the ENTIRE
        // plugins directory (marketplaces/ + cache/ live under it). Pointing it
        // at the state dir (stable across hash changes) avoids re-downloading
        // plugins on every scope change.
        let plugins_dir = state_dir.join("plugins");
        create_dir_owner_only(&plugins_dir)?;
        let plugins_str = plugins_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "state_dir plugins dir is not valid UTF-8: {}",
                plugins_dir.display()
            )
        })?;
        vars.push((
            "CLAUDE_CODE_PLUGIN_CACHE_DIR".into(),
            plugins_str.to_owned(),
        ));

        Ok(vars)
    }

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
        // Every path llmenv writes into `out`, relative to `out`. Returned as
        // the owned set so the orchestrator can reconcile ghost files on a
        // version-mode re-render (#196) without touching foreign state.
        let mut owned: Vec<PathBuf> = Vec::new();

        std::fs::create_dir_all(out)?;
        reject_hardcoded_config_path(&manifest.agents_md, "CLAUDE.md")?;

        // #317: build CLAUDE.md content, appending compact_survival fragment
        // when slippage is enabled with compact_survival on.
        let mut claude_md_content = manifest.agents_md.clone();
        if let Some(s) = manifest
            .capabilities
            .features
            .as_ref()
            .and_then(|f| f.slippage.as_ref())
            && s.enabled
            && s.compact_survival
        {
            claude_md_content.push_str("\n\n<!-- from slippage control: compact_survival -->\n");
            claude_md_content.push_str(COMPACT_SURVIVAL_FRAGMENT);
        }
        crate::paths::write_owner_only(&out.join("CLAUDE.md"), claude_md_content.as_bytes())?;
        owned.push(PathBuf::from("CLAUDE.md"));

        // Claude Code has a native rules-directory convention, so write each
        // `rules/*.md` file verbatim (frontmatter preserved) into `<out>/rules/`.
        // Adapters that lack this convention should instead use
        // `merge::agents_md::concat_with_rules` to inline the bodies.
        for r in &manifest.rules {
            if crate::paths::is_unsafe_join_target(r.rel.to_string_lossy().as_ref()) {
                anyhow::bail!("path traversal in rules file: {}", r.rel.display());
            }
            reject_hardcoded_config_path(&r.raw, &r.rel.to_string_lossy())?;
            let dest = out.join(&r.rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::paths::write_owner_only(&dest, r.raw.as_bytes())?;
            owned.push(r.rel.clone());
        }

        // Copy all files from the manifest. JSON hook templates get
        // `{{ICM_MCP}}` substituted so bundle hooks can reference the MCP
        // server by name without hard-coding it.
        for (rel, abs) in &manifest.files {
            if crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()) {
                anyhow::bail!("path traversal in bundle file: {}", rel.display());
            }
            let dest = out.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if is_hook_json(rel) {
                let raw = std::fs::read_to_string(abs)?;
                let rendered = raw.replace("{{ICM_MCP}}", MEMORY_MCP_NAME);
                crate::paths::write_owner_only(&dest, rendered.as_bytes())?;
            } else {
                std::fs::copy(abs, &dest)?;
            }
            owned.push(rel.clone());
        }

        // Write first-class skills (declared via `capabilities.skills`) before
        // validating, so `validate_skills` covers them along with plugin-sourced ones.
        let skill_owned =
            crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills)?;
        owned.extend(skill_owned);

        // #317: write /diagnose skill when slippage is enabled with diagnose_command.
        if let Some(s) = manifest
            .capabilities
            .features
            .as_ref()
            .and_then(|f| f.slippage.as_ref())
            && s.enabled
            && s.diagnose_command
        {
            let diagnose_dir = out.join("skills").join("diagnose");
            crate::adapter::skills::create_dir_owner_only(&diagnose_dir)?;
            crate::paths::write_owner_only(
                &diagnose_dir.join("SKILL.md"),
                DIAGNOSE_SKILL_CONTENT.as_bytes(),
            )?;
            owned.push(PathBuf::from("skills").join("diagnose"));
        }

        // #556: LSP servers render into a synthetic skills-directory plugin named
        // `LSP_PLUGIN_NAME`. A first-class skill of the same name would silently
        // lose its SKILL.md to this directory (validate_skills treats any
        // `LSP_PLUGIN_NAME` dir as the LSP plugin, not a skill) — reject it instead.
        if manifest
            .capabilities
            .skills
            .iter()
            .any(|s| s.name == LSP_PLUGIN_NAME)
        {
            anyhow::bail!(
                "skill name '{LSP_PLUGIN_NAME}' is reserved for llmenv's synthetic \
                 LSP plugin; rename the skill to avoid the conflict"
            );
        }

        // #556: LSP servers render into a synthetic skills-directory plugin. Written
        // before validate_skills so the plugin dir it creates is in place first.
        if let Some(lsp_owned) = write_lsp_plugin(out, &manifest.capabilities.lsp)? {
            owned.push(lsp_owned);
        }

        // Validate that skills are properly structured with SKILL.md frontmatter
        crate::adapter::skills::validate_skills(out)?;

        // Generate settings.json from hook/permission bundles
        generate_settings_json(out, manifest)?;
        owned.push(PathBuf::from("settings.json"));

        // Write installed_plugins.json for external-sourced plugins so Claude Code
        // treats them as pre-installed and loads them from the stable cache path.
        // First-party plugins (install_path is None) are served directly from the
        // marketplace directory and don't need an installed_plugins.json entry.
        let external_plugins: Vec<_> = manifest
            .plugins
            .iter()
            .filter(|p| p.install_path.is_some())
            .collect();
        if !external_plugins.is_empty() {
            generate_installed_plugins_json(out, &external_plugins)?;
        }

        // #244: merge resolved MCP servers (and any per-engine `native_mcp`
        // fragment, #97) into the top-level `mcpServers` of `.claude.json` — the
        // only surface Claude Code actually reads for user-scoped servers. The
        // legacy `mcp.json` was never ingested. `.claude.json` is overwhelmingly
        // foreign Claude state, so it is deliberately NOT added to the owned set:
        // llmenv only upserts `mcpServers`, and must never reconcile-delete the
        // file.
        let native_mcp = manifest.capabilities.native_mcp.get("claude_code");
        // Always called — the companion file may have previously-owned servers
        // that need pruning even when the current server set is empty.
        merge_mcp_into_claude_json(out, &manifest.mcps, native_mcp)?;

        crate::materialize::prune_empty_dirs(out)?;

        Ok(owned)
    }

    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }

        // Store-only events (SessionStart, SessionEnd) have no model turn to inject context
        // into, and Claude Code's hook schema rejects additionalContext in their
        // hookSpecificOutput. Return empty so these events emit no output. (#558)
        if matches!(hook_event_name, "SessionStart" | "SessionEnd") {
            return String::new();
        }

        // Wrap in a system barrier to prevent prompt injection: the MCP response
        // (possibly from an untrusted memory backend) is wrapped so any attempts
        // to escape the context block are trapped as unparseable markdown.
        let wrapped = format!("[ICM MEMORY CONTEXT (auto-injected)]\n{}", text);
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event_name,
                "additionalContext": wrapped
            }
        })
        .to_string()
    }
}

/// Deep-merge a per-engine `native_*` fragment (opaque YAML) into an
/// already-built JSON config subtree. The fragment is converted to JSON and
/// overlaid via [`merge_json`], so it is the higher-precedence contributor
/// (native overrides win on scalar collision). A `None` fragment is a no-op.
fn overlay_native(
    dst: &mut serde_json::Value,
    fragment: Option<&serde_yaml::Value>,
) -> anyhow::Result<()> {
    if let Some(frag) = fragment {
        let as_json: serde_json::Value =
            serde_json::to_value(frag).context("converting native fragment to JSON")?;
        merge_json(dst, as_json);
    }
    Ok(())
}

/// Top-level settings.json keys that a modeled capability renders. The
/// top-level `native` catch-all (D3) is for keys NO modeled feature owns, so
/// these must never appear there — they belong in the `native_<feature>`
/// sibling, which merges in the safe direction (e.g. native deny can't weaken a
/// neutral deny). `enabledPlugins`/`extraKnownMarketplaces` (plugins) and the
/// separate `mcp.json` doc use distinct keys and aren't catch-all collisions.
const MODELED_SETTINGS_KEYS: [&str; 2] = ["permissions", "hooks"];

/// Reject a top-level `native.<engine>` catch-all fragment that contains a
/// modeled-feature key. Overlaying such a key last would silently clobber the
/// security-rendered output (see the call site). Returns an error naming the
/// offending key and pointing at the correct `native_<feature>` sibling.
fn reject_modeled_keys_in_catch_all(fragment: &serde_yaml::Value) -> anyhow::Result<()> {
    let Some(map) = fragment.as_mapping() else {
        return Ok(());
    };
    for key in MODELED_SETTINGS_KEYS {
        if map.contains_key(serde_yaml::Value::String(key.into())) {
            anyhow::bail!(
                "top-level `native.claude_code` carries the modeled-feature key \
                 `{key}`, which would silently clobber the rendered `{key}` \
                 (a security regression for permissions). Move it to the \
                 `native_{key}` sibling instead, which merges in the safe direction."
            );
        }
    }
    Ok(())
}

/// True if `rel` is a JSON file under the bundle's `hooks/` subtree —
/// these files are template-rendered rather than byte-copied so bundle hooks
/// can reference the ICM MCP via `{{ICM_MCP}}`.
fn is_hook_json(rel: &Path) -> bool {
    rel.starts_with("hooks") && rel.extension().is_some_and(|e| e == "json")
}

/// File Claude Code reads for user-scoped (cross-project) MCP servers: the
/// top-level `mcpServers` key of `$CLAUDE_CONFIG_DIR/.claude.json` (#244). The
/// legacy `mcp.json` was never a config surface Claude ingested.
const CLAUDE_JSON_FILE: &str = ".claude.json";

/// Companion file to `.claude.json` tracking which mcpServers llmenv wrote on
/// the previous render. A JSON array of server name strings. Used to prune
/// servers that llmenv no longer resolves while preserving foreign entries.
const CLAUDE_JSON_OWNED_SERVERS_FILE: &str = ".claude.json.llmenv-owned";

/// Build the `mcpServers` object for every resolved server, keyed by name.
/// Stdio entries carry `command`/`args`/`env`; remote entries carry
/// `{"type", "url"}` — the transport discriminator Claude Code requires (#244).
///
/// #103: detects true same-identity-different-content conflicts: if two MCP
/// server definitions share a name but differ in content, hard-errors naming
/// both contributors and the conflicting name, preventing silent overwrites.
fn build_mcp_servers(
    mcps: &[ResolvedMcp],
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let mut servers = serde_json::Map::new();
    // Track which server came from which resolved entry for conflict reporting.
    let mut server_sources: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    for (idx, m) in mcps.iter().enumerate() {
        let entry = match &m.kind {
            ResolvedKind::Stdio { command, args, env } => {
                let mut obj = json!({ "command": command, "args": args });
                if !env.is_empty() {
                    obj["env"] = json!(env);
                }
                // #506: disabled_tools consumed by CrushAdapter
                obj
            }
            ResolvedKind::Remote { url, transport } => {
                let mut obj =
                    json!({ "type": super::remote_transport_type_str(*transport), "url": url });
                if !m.headers.is_empty() {
                    obj["headers"] = json!(m.headers);
                }
                if let Some(secs) = m.timeout {
                    obj["timeout"] = json!(secs);
                }
                // #506: disabled_tools consumed by CrushAdapter
                obj
            }
        };

        // #103: detect true same-identity-different-content conflicts.
        // If the server name already exists and the content differs, hard-error.
        if let Some(&prev_idx) = server_sources.get(&m.name)
            && let Some(existing_entry) = servers.get(&m.name)
            && existing_entry != &entry
        {
            anyhow::bail!(
                "true semantic conflict: MCP server '{}' defined twice with \
                 different content. First definition (entry #{}) differs from \
                 second definition (entry #{}). Resolve by removing or renaming \
                 one server definition.",
                m.name,
                prev_idx,
                idx,
            );
        }

        server_sources.insert(m.name.clone(), idx);
        servers.insert(m.name.clone(), entry);
    }
    Ok(servers)
}

/// Merge llmenv's resolved MCP servers into the top-level `mcpServers` of
/// `$CLAUDE_CONFIG_DIR/.claude.json` (#244) — the only surface Claude Code reads
/// for user-scoped servers.
///
/// `.claude.json` is overwhelmingly foreign state (oauthAccount, projects,
/// numStartups, …) that Claude mutates constantly, so this is a
/// read-merge-write, never a clobber:
/// - read the existing doc (absent → start from `{}`);
/// - upsert each llmenv server into `mcpServers` by name — foreign server
///   entries and every other top-level key are preserved verbatim;
/// - write back owner-only (0o600 — entries may carry credentials / URLs).
///
/// A present-but-unparseable `.claude.json` is a hard error: silently replacing
/// it would destroy the user's Claude state, so llmenv refuses rather than
/// clobber.
///
/// #97: a per-engine `native_mcp` fragment is overlaid onto the server set
/// before the merge, so engine-specific server entries still flow through. Only
/// its `mcpServers` are propagated — `enabledMcpjsonServers` is a project
/// `.mcp.json` approval gate, irrelevant for the auto-trusted user-scoped
/// servers in `.claude.json`, and is intentionally dropped (#244, relates #122).
///
/// Stale-server pruning (#739): llmenv tracks which server names it wrote in a
/// companion file (`CLAUDE_JSON_OWNED_SERVERS_FILE`). On each render it removes
/// previously-owned servers no longer in the resolved set, while preserving
/// foreign (non-llmenv) entries.
fn merge_mcp_into_claude_json(
    out: &Path,
    mcps: &[ResolvedMcp],
    native: Option<&serde_yaml::Value>,
) -> anyhow::Result<()> {
    // Build llmenv's server set, then overlay the native fragment so engine-only
    // server entries merge in. Only `mcpServers` is carried into `.claude.json`.
    let servers = build_mcp_servers(mcps)?;
    let mut doc = json!({ "mcpServers": servers });
    overlay_native(&mut doc, native)?;
    let llmenv_servers = doc
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    // Read previously-owned server names from the companion tracking file.
    let owned_path = out.join(CLAUDE_JSON_OWNED_SERVERS_FILE);
    let previously_owned = read_owned_servers(&owned_path);

    // Nothing to update or prune.
    if llmenv_servers.is_empty() && previously_owned.is_empty() {
        return Ok(());
    }

    let path = out.join(CLAUDE_JSON_FILE);
    let mut claude = read_claude_json(&path)?;
    let Some(obj) = claude.as_object_mut() else {
        anyhow::bail!(
            "existing {} is not a JSON object; refusing to overwrite (would \
             destroy Claude state). Fix or remove the file and re-run.",
            path.display()
        );
    };

    let servers_val = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    match servers_val.as_object_mut() {
        Some(servers_obj) => {
            // Prune previously-owned servers no longer in the current set.
            for stale_name in &previously_owned {
                if !llmenv_servers.contains_key(stale_name.as_str()) {
                    servers_obj.remove(stale_name.as_str());
                }
            }
            // Upsert current servers.
            for (name, entry) in &llmenv_servers {
                servers_obj.insert(name.clone(), entry.clone());
            }
        }
        // Foreign `mcpServers` was a non-object (malformed). Replace it with
        // llmenv's set rather than error — the servers key is llmenv's domain.
        None => {
            *servers_val = serde_json::Value::Object(llmenv_servers.clone());
        }
    }

    crate::paths::write_owner_only_atomic(
        &path,
        serde_json::to_string_pretty(&claude)?.as_bytes(),
    )?;

    // Write companion file with current owned server names.
    let current_names: Vec<String> = llmenv_servers.keys().cloned().collect();
    if current_names.is_empty() {
        if let Err(e) = std::fs::remove_file(&owned_path) {
            tracing::warn!(
                "failed to remove stale owned MCP server tracking file {}: {e}",
                owned_path.display(),
            );
        }
    } else {
        crate::paths::write_owner_only_atomic(
            &owned_path,
            serde_json::to_string_pretty(&current_names)?.as_bytes(),
        )?;
    }

    Ok(())
}

/// Read the llmenv-owned MCP server tracking companion file.
///
/// Returns an empty set when the file is absent or corrupt — a bad companion
/// file must never prevent `.claude.json` from being written.
fn read_owned_servers(path: &Path) -> std::collections::BTreeSet<String> {
    let s = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return std::collections::BTreeSet::new();
        }
        Err(e) => {
            tracing::warn!(
                "failed to read owned MCP server tracking file {} \
                 (treated as empty): {e}",
                path.display(),
            );
            return std::collections::BTreeSet::new();
        }
    };
    match serde_json::from_str::<Vec<String>>(&s) {
        Ok(names) => names.into_iter().collect(),
        Err(e) => {
            tracing::warn!(
                "failed to parse owned MCP server tracking file {} \
                 (treated as empty): {e}",
                path.display(),
            );
            std::collections::BTreeSet::new()
        }
    }
}

/// Read `.claude.json`, returning an empty object when the file is absent. A
/// present-but-unparseable file is a hard error — llmenv must never destroy the
/// user's Claude state by overwriting corrupt JSON with a fresh doc.
fn read_claude_json(path: &Path) -> anyhow::Result<serde_json::Value> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "existing {} is not valid JSON; refusing to overwrite (would \
                 destroy Claude state). Fix or remove the file and re-run.",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::Value::Object(serde_json::Map::new()))
        }
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

/// Copy files from a source directory into a destination recursively, writing
/// each file owner-only (0o600). Non-UTF-8 paths are skipped (same policy as
/// `scan_skill_files_for_hardcoded_paths`). Returns the list of relative paths
/// written (relative to `dest_dir`), for inclusion in the `owned` set.
pub(crate) fn copy_dir_owner_only(src: &Path, dest: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut written: Vec<PathBuf> = Vec::new();
    create_dir_owner_only(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let meta = std::fs::symlink_metadata(&src_path)?;
        if meta.file_type().is_symlink() {
            // Skip symlinks: no TOCTOU-safe way to follow them into a bounded dir.
            tracing::debug!(path = %src_path.display(), "copy_dir_owner_only: skipping symlink");
            continue;
        }
        let file_name = entry.file_name();
        let dest_path = dest.join(&file_name);
        if meta.is_dir() {
            let sub_written = copy_dir_owner_only(&src_path, &dest_path)?;
            written.extend(sub_written);
        } else if meta.is_file() {
            let content = std::fs::read(&src_path)?;
            crate::paths::write_owner_only(&dest_path, &content)?;
            written.push(dest_path);
        }
    }
    Ok(written)
}

// write_first_class_skills, validate_skills, validate_skill_frontmatter, and
// scan_skill_files_for_hardcoded_paths live in crate::adapter::skills — shared with CrushAdapter.

/// Generates settings.json from the already-merged hook + permission
/// capabilities in the manifest.
///
/// Hooks (#90): `Vec<Hook>` → `{ EventName: [{ matcher?, hooks: [handler] }] }`.
///
/// Permissions (#34): neutral `{tool, pattern|paths}` rules render into Claude's
/// `Tool(pattern)` string grammar and land in `permissions.{allow,ask,deny}`
/// alongside the verbatim `native.claude_code` rule strings (one flat array per
/// action — not a nested `native` object). `default_mode` maps to `defaultMode`.
/// Native rules win in the safe direction only — deny is authoritative
/// (authority runs deny > ask > allow). A native `deny` suppresses a neutral
/// `allow`/`ask` of the same string, but a native `allow` never suppresses a
/// neutral `deny`: silently weakening a deny would be a security regression.
/// Cross-bundle merge (concat + dedup, scope-ordered) already happened in
/// [`crate::merge`]; this function only renders.
///
/// Write `plugins/installed_plugins.json` for external-sourced plugins so Claude
/// Code treats them as pre-installed and loads from the stable cache path.
///
/// Only called when at least one plugin has a non-None `install_path`; first-party
/// plugins (served from the marketplace clone via `directory` source) are excluded.
/// The file follows Claude Code's v2 schema exactly.
///
/// A present-but-unparseable existing file is a hard error — matches
/// [`read_claude_json`]'s convention: llmenv must never destroy plugin version
/// pins by silently overwriting corrupt JSON with a fresh doc.
fn generate_installed_plugins_json(
    out: &Path,
    plugins: &[&crate::plugins::resolve::ResolvedPlugin],
) -> anyhow::Result<()> {
    let plugins_dir = out.join("plugins");
    create_dir_owner_only(&plugins_dir)?;
    let path = plugins_dir.join("installed_plugins.json");

    // A fixed epoch timestamp is acceptable: CC uses installedAt/lastUpdated
    // for display only, not for any functional decision.
    let now = "1970-01-01T00:00:00.000Z";

    let mut existing: serde_json::Map<String, serde_json::Value> = match std::fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw).with_context(|| {
            format!(
                "existing {} is not valid JSON; refusing to overwrite (would \
                 destroy plugin version pins). Fix or remove the file and re-run.",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => anyhow::bail!("reading {}: {e}", path.display()),
    };

    let entries = existing.entry("plugins").or_insert_with(|| json!({}));
    let map = entries
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("installed_plugins.json: `plugins` is not an object"))?;

    for p in plugins {
        let Some(install_path) = &p.install_path else {
            continue;
        };
        let sha = p.git_commit_sha.as_deref().unwrap_or_default();
        let version = if sha.len() >= 12 { &sha[..12] } else { sha };
        let key = format!("{}@{}", p.plugin, p.marketplace);
        map.insert(
            key,
            json!([{
                "scope": "user",
                "installPath": install_path,
                "version": version,
                "installedAt": now,
                "lastUpdated": now,
                "gitCommitSha": sha,
            }]),
        );
    }

    existing.insert("version".into(), json!(2));
    let json_str = serde_json::to_string_pretty(&serde_json::Value::Object(existing))?;
    crate::paths::write_owner_only_atomic(&path, json_str.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Name of the synthetic skills-directory plugin (#556) that carries `lsp:`
/// entries into Claude Code. Any folder under `skills/` containing a
/// `.claude-plugin/plugin.json` auto-loads as a plugin named `<name>@skills-dir`
/// with no marketplace and no install step — this is Claude Code's only LSP
/// surface (a plugin manifest's `lspServers` key); there is no bare top-level
/// config key the way MCP has `mcpServers`.
pub(crate) const LSP_PLUGIN_NAME: &str = "llmenv-lsp";

/// #317: skill content for the `/diagnose` slash command, written as a
/// first-class skill when slippage control is enabled with diagnose_command.
const DIAGNOSE_SKILL_CONTENT: &str = concat!(
    "---\n",
    "name: diagnose\n",
    "description: Structured evidence-first debugging checklist\n",
    "---\n",
    "\n",
    "Structured evidence-first debugging. Follow each step in order.\n",
    "\n",
    "## 1. Collect Symptoms\n",
    "\n",
    "- What exactly happened? (exact error message, behavior, output)\n",
    "- When did it start? (after a change, deploy, or time-based)\n",
    "- Is it reproducible? (always, sometimes, specific inputs)\n",
    "\n",
    "## 2. Gather Evidence\n",
    "\n",
    "- Check recent changes (git log, deploy history)\n",
    "- Examine relevant logs, metrics, or state\n",
    "- Check for known issues or recent regressions\n",
    "\n",
    "## 3. Form Hypotheses\n",
    "\n",
    "- List 2-3 possible root causes based on evidence\n",
    "- Rank by likelihood given the evidence\n",
    "- State what would confirm or rule out each\n",
    "\n",
    "## 4. Test Per Hypothesis\n",
    "\n",
    "For each hypothesis (highest likelihood first):\n",
    "- Design a specific test to confirm or rule it out\n",
    "- Run the test\n",
    "- Record the result\n",
    "\n",
    "## 5. Act\n",
    "\n",
    "Only after a hypothesis is confirmed:\n",
    "- Apply the targeted fix\n",
    "- Verify the fix resolves the original symptom\n",
    "- Add a regression guard if appropriate\n",
);

/// Renders `manifest.capabilities.lsp` into `skills/llmenv-lsp/.claude-plugin/plugin.json`.
/// Returns the relative path written, or `None` if nothing rendered (mirrors how the
/// Crush adapter omits its `lsp` key entirely when every server is disabled/skipped).
///
/// Claude Code's `lspServers` schema requires `extensionToLanguage` (file extension →
/// language id). The neutral `filetypes` field (language ids only, e.g. `"rust"`) can't
/// be reliably converted into that — a language id is often not its own extension
/// (`rust` → `.rs`, `python` → `.py`) — so a server without `extension_to_language` set
/// is skipped for Claude Code with a warning, the same "skip + warn loudly" pattern
/// `CrushAdapter` uses for hooks it can't express, rather than a hard error that would
/// break a bundle shared with an engine (Crush) that renders `filetypes` directly.
/// `root_markers` and `timeout` have no Claude Code equivalent (a single `workspaceFolder`
/// path and a startup-only `startupTimeout` respectively, not per-request) and are left
/// unrendered rather than guessed at.
fn write_lsp_plugin(
    out: &Path,
    servers: &[crate::config::LspServer],
) -> anyhow::Result<Option<PathBuf>> {
    let mut lsp_servers = serde_json::Map::new();
    for srv in servers {
        if srv.disabled {
            continue;
        }
        if srv.extension_to_language.is_empty() {
            eprintln!(
                "warning: Claude Code requires an extensionToLanguage map for LSP servers — \
                 skipping '{}' for Claude Code. Add capabilities.lsp[].extension_to_language \
                 (e.g. {{\".rs\": \"rust\"}}) to enable it there.",
                srv.name
            );
            continue;
        }
        let mut entry = serde_json::Map::new();
        entry.insert("command".into(), json!(srv.command));
        if !srv.args.is_empty() {
            entry.insert("args".into(), json!(srv.args));
        }
        if !srv.env.is_empty() {
            entry.insert("env".into(), json!(srv.env));
        }
        entry.insert(
            "extensionToLanguage".into(),
            json!(srv.extension_to_language),
        );
        if let Some(opts) = &srv.init_options {
            let as_json = serde_json::to_value(opts).map_err(|err| {
                anyhow::anyhow!(
                    "LSP server '{}': failed to convert init_options to JSON: {err}",
                    srv.name
                )
            })?;
            entry.insert("initializationOptions".into(), as_json);
        }
        lsp_servers.insert(srv.name.clone(), serde_json::Value::Object(entry));
    }

    if lsp_servers.is_empty() {
        return Ok(None);
    }

    let plugin_dir = out
        .join("skills")
        .join(LSP_PLUGIN_NAME)
        .join(".claude-plugin");
    create_dir_owner_only(&plugin_dir)?;
    let manifest = json!({ "name": LSP_PLUGIN_NAME, "lspServers": lsp_servers });
    let rel_path = PathBuf::from("skills")
        .join(LSP_PLUGIN_NAME)
        .join(".claude-plugin")
        .join("plugin.json");
    crate::paths::write_owner_only_atomic(
        &plugin_dir.join("plugin.json"),
        serde_json::to_string_pretty(&manifest)?.as_bytes(),
    )
    .with_context(|| format!("writing {}", rel_path.display()))?;

    Ok(Some(rel_path))
}

/// SessionStart (#85): the hook object shape supports it; hash-comparison logic
/// lives in the runtime hook script.
fn generate_settings_json(out: &Path, manifest: &MergedManifest) -> anyhow::Result<()> {
    let mut settings = serde_json::Map::new();

    // #499: whether a memory backend (the `icm` MCP) resolved for this scope —
    // reused below both to gate turn_start/UserPromptSubmit and to decide
    // autoMemoryEnabled, per the design's "Auto-wiring (config gating)" section
    // (no new config field; the existing `memory:` block already gates this).
    let icm_active = manifest.mcps.iter().any(|m| m.name == MEMORY_MCP_NAME);

    // #90: Transform hooks: Vec<Hook> into { EventName: [{ matcher, hooks: [...] }] }
    // Design: https://github.com/phaedrus1992/llmenv/blob/main/docs/design/engine-capabilities.md
    let mut hooks_by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for hook in &manifest.capabilities.hooks {
        // Resolve bundle-relative paths against the cache directory so hook
        // commands reference the materialized files, not the source bundle
        // location (issue #162). Files are already copied into `out` by the
        // caller; a relative path like `hooks/guard.sh` must resolve to
        // `{cache_dir}/hooks/guard.sh`, not the original bundle directory.
        //
        // Two-pass resolution:
        // 1. Clean relative paths (e.g. `bash hooks/guard.sh`) — direct join.
        // 2. Shell-var / absolute prefixes (e.g.
        //    `bash ${HOME}/.../hooks/guard.sh`) — suffix-match against the
        //    files we already copied into `out`.
        let resolved_command = if let Some(cmd) = &hook.handler.command {
            if hook.bundle_origin.is_some() {
                let resolved = resolve_bundle_relative_paths(cmd, out)
                    .or_else(|| resolve_command_paths_against_files(cmd, out, &manifest.files));
                if resolved.is_none() && cmd.contains('/') {
                    tracing::debug!(
                        command = %cmd,
                        "bundle hook path could not be re-anchored to cache directory"
                    );
                }
                resolved.or_else(|| Some(cmd.clone()))
            } else {
                Some(cmd.clone())
            }
        } else {
            None
        };

        // Build handler as a Map so null-valued keys (e.g. "tool": null for
        // command-type hooks) are omitted rather than serialized. The json!
        // macro would produce `"tool": null` for None, which later differs
        // from absent in JSON PartialEq — causing duplicate hooks across
        // renders when reconcile_settings merges fresh with existing disk
        // state that happens to lack the null key.
        let handler = {
            let mut m = serde_json::Map::new();
            if let Some(ref cmd) = resolved_command {
                m.insert("command".into(), serde_json::Value::String(cmd.clone()));
            }
            if let Some(ref tool) = hook.handler.tool {
                m.insert("tool".into(), serde_json::Value::String(tool.clone()));
            }
            m.insert(
                "type".into(),
                serde_json::Value::String(
                    match hook.handler.kind {
                        crate::config::HookHandlerKind::Command => "command",
                        crate::config::HookHandlerKind::McpTool => "mcp_tool",
                    }
                    .into(),
                ),
            );
            serde_json::Value::Object(m)
        };

        let mut hook_entry = serde_json::Map::new();
        if let Some(matcher) = &hook.matcher {
            hook_entry.insert("matcher".into(), json!(matcher));
        }
        hook_entry.insert("hooks".into(), json!([handler]));

        hooks_by_event
            .entry(hook.event.clone())
            .or_default()
            .push(serde_json::Value::Object(hook_entry));
    }

    // #121/#85: always register a SessionStart stale-context check. It concats
    // with any bundle- or native-declared SessionStart entries (events union),
    // so a user's own SessionStart hook is never clobbered. The runtime command
    // reads the booted hash off CLAUDE_CONFIG_DIR and recomputes the current one.
    hooks_by_event
        .entry("SessionStart".to_string())
        .or_default()
        .push(json!({
            "hooks": [{ "type": "command", "command": STALE_CHECK_COMMAND }],
        }));

    // #289: inject source config paths at session start so the agent knows where
    // to edit llmenv config rather than touching managed cache files.
    hooks_by_event
        .entry("SessionStart".to_string())
        .or_default()
        .push(json!({
            "hooks": [{ "type": "command", "command": CONFIG_CONTEXT_COMMAND }],
        }));

    // #289: warn the agent when it tries to write inside the managed cache dir.
    // Anchored regex so only exact tool names match, not substrings like BatchEdit.
    // Exits 0 (fail-soft, never blocks the write).
    hooks_by_event
        .entry("PreToolUse".to_string())
        .or_default()
        .push(json!({
            "matcher": "^(Write|Edit|MultiEdit)$",
            "hooks": [{ "type": "command", "command": CONFIG_GUARD_COMMAND }],
        }));

    // #318: read-once file dedup hook — warn or deny repeated file reads.
    // Registered unconditionally (no config gating). The hook-run handler in
    // `run_inner` checks `features.read_once.enabled` and returns empty
    // (pass-through) when disabled, so the regex match is the only cost when
    // the feature is off.
    hooks_by_event
        .entry("PreToolUse".to_string())
        .or_default()
        .push(json!({
            "matcher": "^Read$",
            "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} pre_tool_use") }],
        }));

    // Throttle hooks: poll usage backend and sleep adaptive delay to avoid rate limits.
    if manifest.throttle.is_some() {
        hooks_by_event
            .entry("PreToolUse".to_string())
            .or_default()
            .push(json!({
                "hooks": [{ "type": "command", "command": format!("{THROTTLE_COMMAND} pre-tool") }],
            }));
        hooks_by_event
            .entry("UserPromptSubmit".to_string())
            .or_default()
            .push(json!({
                "hooks": [{ "type": "command", "command": format!("{THROTTLE_COMMAND} prompt") }],
            }));
    }

    // Baseline lifecycle hooks: ICM memory wake-up/store + session-log
    // lifecycle/scope events (#382). Always registered; `hook-run` itself
    // no-ops cheaply when nothing is configured for either.
    for (neutral_event, native_event) in BASELINE_HOOK_EVENTS {
        hooks_by_event
            .entry((*native_event).to_string())
            .or_default()
            .push(json!({
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} {neutral_event}") }],
            }));
    }

    // #499: continuous per-prompt memory recall. Gated on icm_active (unlike the
    // always-on baseline events above) because this runs on every prompt, not
    // just session start/end — an unconditional per-turn network-backed hook
    // would add latency for every scope, including ones with no memory backend
    // configured at all.
    if icm_active {
        hooks_by_event
            .entry("UserPromptSubmit".to_string())
            .or_default()
            .push(json!({
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} turn_start") }],
            }));
    }

    // Session-log turn hooks: per-prompt/tool-use capture, registered when any
    // sink is enabled (#382). The hook-run binary filters by per-sink level.
    if manifest.session_log.any_sink_enabled() {
        for (neutral_event, native_event) in SESSION_LOG_HOOK_EVENTS {
            hooks_by_event
                .entry((*native_event).to_string())
                .or_default()
                .push(json!({
                    "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} {neutral_event}") }],
                }));
        }
    }

    let mut hooks_obj = serde_json::Map::new();
    for (event, entries) in hooks_by_event {
        hooks_obj.insert(event, json!(entries));
    }
    // #97: overlay the per-engine `native_hooks` fragment (a `hooks`-shaped
    // settings.json object) so engine-only events and handlers merge in. Shared
    // events concat their entry arrays; native is the higher-precedence overlay.
    let mut hooks_value = serde_json::Value::Object(hooks_obj);
    overlay_native(
        &mut hooks_value,
        manifest.capabilities.native_hooks.get("claude_code"),
    )?;
    settings.insert("hooks".into(), hooks_value);

    // #34: Render neutral permission rules into Claude's string grammar
    // (`Tool(pattern)` / `Tool(path)` / bare `Tool`), then append the per-engine
    // `native.claude_code` rule strings verbatim into the same allow/ask/deny
    // arrays. Native rules are not a separate object — Claude Code reads one flat
    // array per action (see docs/reference/claude-code/permissions.md). They
    // share the array because both are just permission rule strings; the only
    // difference is neutral rules are generated and native ones are authored.
    let perms = &manifest.capabilities.permissions;
    let native = manifest.capabilities.native_permissions.get("claude_code");

    // Native rules win over neutral ones, but only in the safe direction: deny is
    // authoritative. Authority runs deny > ask > allow (most restrictive wins). A
    // neutral string is dropped only when a *more authoritative* native action
    // claims it — so a native `deny: ["WebFetch(domain:x)"]` suppresses a neutral
    // `allow`/`ask` of the same string (native deny wins), but a native `allow`
    // never suppresses a neutral `deny`. Silently weakening a deny would be a
    // security regression. Within the same action, agreeing native+neutral strings
    // simply dedupe (the native list is appended below).
    // Only deny and ask can outrank a neutral rule (deny > ask > allow), so a
    // native allow set is never a suppressor and isn't collected.
    let native_ask: std::collections::BTreeSet<&str> = native.map_or_else(Default::default, |n| {
        n.ask.iter().map(String::as_str).collect()
    });
    let native_deny: std::collections::BTreeSet<&str> = native.map_or_else(Default::default, |n| {
        n.deny.iter().map(String::as_str).collect()
    });

    // For a neutral rule in `action`, the set of native strings that outrank it.
    let suppressors = |action: PermissionAction| -> Vec<&std::collections::BTreeSet<&str>> {
        match action {
            PermissionAction::Allow => vec![&native_deny, &native_ask],
            PermissionAction::Ask => vec![&native_deny],
            PermissionAction::Deny => Vec::new(),
        }
    };

    let render_action = |neutral: &[crate::config::PermissionRule],
                         native_rules: &[String],
                         action: PermissionAction| {
        let outranking = suppressors(action);
        let mut out: Vec<String> = Vec::new();
        for rule in neutral {
            for s in render_permission_rule(rule) {
                // Drop the neutral string only when a more authoritative native
                // action asserts it — unless this action's own native list also
                // asserts it (appended below, so an agreeing pair still emits).
                let outranked = outranking.iter().any(|set| set.contains(s.as_str()));
                if outranked && !native_rules.contains(&s) {
                    continue;
                }
                out.push(s);
            }
        }
        out.extend(native_rules.iter().cloned());
        dedup(&mut out);
        out
    };

    let mut allow = render_action(
        &perms.allow,
        native.map_or(&[], |n| &n.allow),
        PermissionAction::Allow,
    );
    let mut ask = render_action(
        &perms.ask,
        native.map_or(&[], |n| &n.ask),
        PermissionAction::Ask,
    );
    let mut deny = render_action(
        &perms.deny,
        native.map_or(&[], |n| &n.deny),
        PermissionAction::Deny,
    );

    // #694/#273: Tiered MCP permission rules for built-in servers.
    // Read-only tools → allow, mutation → ask, destructive → deny.
    // Each tiered rule is suppressed when a more authoritative native rule
    // already covers it (deny > ask > allow), matching the `render_action`
    // native-wins invariant — see the suppressors closure at the top of this
    // function.
    if icm_active {
        let icm_prefix = format!("mcp__{}__", MEMORY_MCP_NAME);
        for &tool in ICM_READ_ONLY {
            let s = format!("{icm_prefix}{tool}");
            if !native_deny.contains(s.as_str()) && !native_ask.contains(s.as_str()) {
                allow.push(s);
            }
        }
        for &tool in ICM_MUTATION {
            let s = format!("{icm_prefix}{tool}");
            if !native_deny.contains(s.as_str()) {
                ask.push(s);
            }
        }
        for &tool in ICM_DESTRUCTIVE {
            deny.push(format!("{icm_prefix}{tool}"));
        }
    }

    if manifest.plugins.iter().any(|p| {
        p.marketplace == crate::config::CONTEXT_MODE_MARKETPLACE
            && p.plugin == crate::config::CONTEXT_MODE_PLUGIN
    }) {
        let prefix = crate::config::CONTEXT_MODE_MCP_PREFIX;
        for &tool in CTX_READ_ONLY {
            let s = format!("{prefix}{tool}");
            if !native_deny.contains(s.as_str()) && !native_ask.contains(s.as_str()) {
                allow.push(s);
            }
        }
        for &tool in CTX_MUTATION {
            let s = format!("{prefix}{tool}");
            if !native_deny.contains(s.as_str()) {
                ask.push(s);
            }
        }
        for &tool in CTX_DESTRUCTIVE {
            deny.push(format!("{prefix}{tool}"));
        }
    }
    dedup(&mut allow);
    dedup(&mut ask);
    dedup(&mut deny);

    let has_perms =
        !allow.is_empty() || !ask.is_empty() || !deny.is_empty() || perms.default_mode.is_some();
    if has_perms {
        let mut perm_obj = serde_json::Map::new();
        if let Some(mode) = perms.default_mode {
            perm_obj.insert("defaultMode".into(), json!(permission_mode_str(mode)));
        }
        // Always emit the three arrays when any permission config exists, so the
        // shape matches Claude Code's object schema even if one action is empty.
        perm_obj.insert("allow".into(), json!(allow));
        perm_obj.insert("ask".into(), json!(ask));
        perm_obj.insert("deny".into(), json!(deny));
        settings.insert("permissions".into(), serde_json::Value::Object(perm_obj));
    }

    // #227/#123: manage auto memory enablement. When llmenv's ICM memory backend
    // is active, disable Claude's auto memory to prevent competition. Only emit
    // the key if: (1) explicitly set in config, or (2) ICM is active and we need
    // to disable it. Emitted before native overlays so `native.claude_code.autoMemoryEnabled`
    // can still override if set (native is the highest-precedence layer).
    if let Some(configured) = manifest.capabilities.auto_memory_enabled {
        settings.insert("autoMemoryEnabled".into(), json!(configured));
    } else if icm_active {
        settings.insert("autoMemoryEnabled".into(), json!(false));
    }

    // #221: Render first-class capability fields (effort level, advisor size)
    if let Some(effort_level) = &manifest.capabilities.effort_level {
        settings.insert("effortLevel".into(), json!(effort_level));
    }
    if let Some(advisor_size) = &manifest.capabilities.advisor_size {
        settings.insert("advisorSize".into(), json!(advisor_size));
    }

    // Plugins (#59): declare marketplaces + enabled plugins into settings.json.
    // llmenv owns the marketplace clone in its cache, so each marketplace points
    // Claude at that checkout via a `directory` source (no re-fetch). Plugins are
    // keyed `<plugin>@<marketplace>` and force-enabled.
    render_plugins(&mut settings, manifest);

    // #97: overlay the per-engine `native_plugins` fragment at the settings top
    // level (plugin-related keys Claude understands but llmenv has no neutral
    // form for, e.g. extra `enabledPlugins` entries).
    let mut settings_value = serde_json::Value::Object(settings);
    overlay_native(
        &mut settings_value,
        manifest.capabilities.native_plugins.get("claude_code"),
    )?;

    // #96: overlay the top-level `native.claude_code` catch-all last — opaque
    // keys that belong to no modeled feature (e.g. `alwaysThinkingEnabled`).
    // It is the highest-precedence layer, applied after every modeled render.
    //
    // Security guard (#102): the catch-all is for keys NO modeled feature owns.
    // A modeled-feature key here (`permissions`, `hooks`) would overlay LAST over
    // the security-rendered output, silently clobbering it — e.g. erasing the
    // permission `deny` array, bypassing the deny-never-weakened invariant. Per
    // design D3 ("Layer 1 wins, or hard-error"), reject it loudly. The key
    // belongs in the `native_<feature>` sibling, which merges in the safe
    // direction.
    if let Some(native) = manifest.native.get("claude_code") {
        reject_modeled_keys_in_catch_all(native)?;
    }
    overlay_native(&mut settings_value, manifest.native.get("claude_code"))?;

    let settings_path = out.join("settings.json");

    // #196/#175: in version mode `out` is the agent's live config dir for the
    // whole session, so a plugin may have self-registered hooks (or other keys)
    // into settings.json after llmenv last wrote it. A wholesale overwrite would
    // strand that registration. Reconcile instead: preserve any foreign keys
    // already on disk, while making llmenv authoritative over the keys it owns.
    // In strict mode the file never pre-exists (fresh content-hashed folder), so
    // this is a no-op there.
    let reconciled = reconcile_settings(&settings_path, settings_value)?;
    let json_str = serde_json::to_string_pretty(&reconciled)?;

    crate::paths::write_owner_only_atomic(&settings_path, json_str.as_bytes()).with_context(
        || {
            format!(
                "Failed to write settings.json at {}",
                settings_path.display()
            )
        },
    )?;

    Ok(())
}

/// Top-level settings.json keys llmenv renders authoritatively. On a re-render
/// these are **replaced** with llmenv's freshly-computed value — a rule llmenv
/// dropped from config must actually disappear, and `permissions` must never be
/// weakened by a stale union. The one shared key, `hooks`, is handled specially
/// (see [`reconcile_settings`]) so a plugin's self-registered hook survives.
pub(crate) const LLMENV_OWNED_SETTINGS_KEYS: [&str; 9] = [
    "permissions",
    "enabledPlugins",
    "extraKnownMarketplaces",
    "autoMemoryEnabled",
    "effortLevel",
    "advisorSize",
    "hooks",
    // Security: never allow these to be seeded from ~/.claude/settings.json —
    // they bypass all tool-call confirmations across every environment.
    "bypassPermissions",
    "dangerouslySkipPermissions",
];

/// Merge user-elected seeded keys into `out/settings.json` after the adapter
/// has already written the file (#172). Runs on every render, not just new
/// folders: `reconcile_settings` already preserves existing foreign keys, so
/// for re-renders this is nearly always a no-op (all seeded keys already
/// present). For a fresh folder, this adds user defaults that would otherwise
/// be absent from the first-rendered `settings.json`.
///
/// **Must be called after `adapter.materialize()`** so that if materialize
/// fails, settings.json is left either absent (new folder, no partial state)
/// or in its prior good state (re-render, reconcile is atomic). Calling before
/// materialize can leave a seeded-only settings.json (no llmenv-owned keys)
/// if materialize subsequently errors.
///
/// # Errors
/// Returns an error if serialization or the atomic write fails.
pub(crate) fn apply_seeded_settings(
    out: &Path,
    seeded: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if seeded.is_empty() {
        return Ok(());
    }
    let path = out.join("settings.json");
    // Read whatever materialize wrote; no-op if file absent (materialize
    // failed or skipped — don't create a seeded-only file in that case).
    let existing: serde_json::Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .inspect_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse settings.json")
            })
            .unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading {} for seeding: {e}",
                path.display()
            ));
        }
    };
    let serde_json::Value::Object(mut obj) = existing else {
        return Ok(());
    };
    let mut changed = false;
    for (k, v) in seeded {
        // Never add llmenv-owned keys — reconcile_settings owns those.
        if !LLMENV_OWNED_SETTINGS_KEYS.contains(&k.as_str()) && !obj.contains_key(k) {
            obj.insert(k.clone(), v.clone());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    crate::paths::write_owner_only_atomic(&path, json.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing seeded settings {}: {e}", path.display()))
}

/// Classify a `claude` binary path as `"homebrew"`, `"npm"`, or `"native"`.
fn classify_claude_path(path: &str) -> &'static str {
    let lc = path.to_ascii_lowercase();
    if lc.contains("/homebrew/") || lc.contains("/cellar/") || lc.contains("/linuxbrew/") {
        "homebrew"
    } else if lc.contains("node_modules")
        || lc.contains("/.npm")
        || lc.contains("/.nvm")
        || lc.contains("/npm/")
        || lc.contains("/.volta/")
        || lc.contains("/.fnm/")
        || lc.contains("/.local/share/pnpm/")
        || lc.contains("/library/pnpm/")
    {
        "npm"
    } else {
        "native"
    }
}

fn find_claude_binary() -> Option<String> {
    let out = std::process::Command::new("which")
        .arg("claude")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Seed `installMethod` into `out/settings.json` if absent (#346).
///
/// Detects how `claude` was installed by inspecting its binary path, then
/// writes the result as a foreign key so it survives every re-render.
/// No-op if `settings.json` does not exist (materialize hasn't run yet) or if
/// `installMethod` is already present.
///
/// # Errors
/// Returns an error if the file exists but cannot be read or written.
pub(crate) fn seed_install_method(out: &std::path::Path) -> anyhow::Result<()> {
    let settings_path = out.join("settings.json");

    // Skip fork if installMethod already present.
    match std::fs::read(&settings_path) {
        Ok(bytes) => {
            if let Ok(serde_json::Value::Object(obj)) =
                serde_json::from_slice::<serde_json::Value>(&bytes)
                && obj.contains_key("installMethod")
            {
                return Ok(());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File doesn't exist yet, that's fine.
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading {} for seeding installMethod: {e}",
                settings_path.display()
            ));
        }
    }

    let method = find_claude_binary()
        .as_deref()
        .map_or("native", classify_claude_path);
    let mut seeded = serde_json::Map::new();
    seeded.insert("installMethod".to_string(), serde_json::Value::from(method));
    apply_seeded_settings(out, &seeded)
}

/// Merge llmenv's freshly-rendered settings (`fresh`) onto whatever already
/// exists at `path`, preserving foreign in-session state (#175, #196).
///
/// Strategy:
/// - Start from the on-disk doc (or an empty object when absent / unparseable —
///   a corrupt file must not abort the render or silently drop llmenv config).
/// - **Foreign keys** (anything not in [`LLMENV_OWNED_SETTINGS_KEYS`]) are left
///   exactly as they were on disk — that is what protects a plugin's own
///   top-level keys.
/// - **`hooks`** is *merged* (per-event arrays concat + dedup via
///   [`merge_json`]), so a plugin's self-registered SessionStart entry survives
///   alongside llmenv's. Dedup keeps llmenv's own re-rendered entries from
///   accumulating across renders.
/// - **Every other owned key** is *replaced* with llmenv's value (authoritative;
///   removals propagate, `permissions` is never weakened by a stale union).
/// - An owned key llmenv does *not* render this round (e.g. no plugins → no
///   `enabledPlugins`) is removed from the on-disk doc, so dropping all plugins
///   actually clears the key rather than leaving a stale one.
fn reconcile_settings(path: &Path, fresh: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let existing = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .inspect_err(|e| tracing::warn!("failed to parse {}: {e:#}", path.display()))
            .ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading existing settings.json {}: {e}",
                path.display()
            ));
        }
    };

    // No prior file (strict mode, or first version-mode render): llmenv's doc is
    // the whole truth.
    let Some(mut merged) = existing else {
        return Ok(fresh);
    };
    // A non-object on disk (corrupt/hand-edited) can't carry foreign keys worth
    // preserving — llmenv's render wins outright.
    let Some(merged_obj) = merged.as_object_mut() else {
        return Ok(fresh);
    };
    let fresh_obj = match &fresh {
        serde_json::Value::Object(o) => o,
        // llmenv always renders an object; defend against a future change.
        _ => return Ok(fresh),
    };

    for key in LLMENV_OWNED_SETTINGS_KEYS {
        match fresh_obj.get(key) {
            Some(fresh_val) if key == "hooks" => {
                // Union so a plugin's foreign hook entries survive; dedup keeps
                // llmenv's own entries from piling up across re-renders.
                // merge_json mutates in-place via &mut; the Option result is
                // intentionally discarded after the mutation completes.
                merged_obj
                    .get_mut(key)
                    .map(|v| {
                        merge_json(v, fresh_val.clone());
                        // Null-valued keys (e.g. "tool": null) differ from
                        // absent keys in JSON PartialEq, so hook entries that
                        // differ only by null vs absent don't dedup inside
                        // merge_json. Strip nulls then re-dedup so entries
                        // from different render generations converge.
                        strip_json_nulls(v);
                        if let Some(obj) = v.as_object_mut() {
                            for entries in obj.values_mut() {
                                if let Some(arr) = entries.as_array_mut() {
                                    dedup(arr);
                                }
                            }
                        }
                    })
                    .or_else(|| {
                        merged_obj.insert(key.to_string(), fresh_val.clone());
                        Some(())
                    });
            }
            Some(fresh_val) => {
                // Authoritative replace.
                merged_obj.insert(key.to_string(), fresh_val.clone());
            }
            None => {
                // llmenv rendered nothing for this owned key this round → drop
                // any stale value so removals (e.g. all plugins removed) clear.
                merged_obj.remove(key);
            }
        }
    }

    // Native passthrough keys: any key llmenv computed into `fresh` (e.g. via
    // overlay_native) that is not a modeled-feature key gets written through on
    // every render. Plugin-foreign keys that are on disk but absent from `fresh`
    // are left untouched — they aren't touched by this loop.
    for (key, val) in fresh_obj {
        if !LLMENV_OWNED_SETTINGS_KEYS.contains(&key.as_str()) {
            merged_obj.insert(key.clone(), val.clone());
        }
    }

    Ok(merged)
}

/// Recursively remove null-valued keys from every JSON object in `value`.
///
/// This makes objects that differ only by null vs absent key compare equal
/// under [`PartialEq`], which [`merge_json`]'s array dedup uses. Needed
/// because `generate_settings_json` conditionally omits keys when their
/// value is `None` (e.g. `"tool"` for command-type hooks), but older
/// on-disk copies may have `"tool": null` from a previous serialization
/// path — the two compare unequal and pile up as duplicates across renders.
fn strip_json_nulls(value: &mut serde_json::Value) {
    strip_json_nulls_depth(value, 0);
}

/// Depth-limited implementation of [`strip_json_nulls`].
///
/// The depth guard prevents stack overflow on pathological JSON nesting
/// (config depth is normally <10 levels). The serde_json parser has its
/// own recursion limit, but that guards _parsing_ — the value tree can
/// be arbitrarily nested after deserialization.
fn strip_json_nulls_depth(value: &mut serde_json::Value, depth: usize) {
    if depth > 64 {
        tracing::warn!("strip_json_nulls: max depth exceeded, bailing");
        return;
    }
    match value {
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                strip_json_nulls_depth(item, depth + 1);
            }
        }
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for v in map.values_mut() {
                strip_json_nulls_depth(v, depth + 1);
            }
        }
        _ => {}
    }
}

/// Render one marketplace's `extraKnownMarketplaces` entry body, or `None` if it
/// should be skipped.
///
/// Every entry value wraps the source object under a `source` key, matching the
/// `extraKnownMarketplaces` shape Claude Code reads/writes:
/// `{ "source": { "source": "github" | "directory", ... } }`.
///
/// - **Reserved official marketplaces** (#190): Claude Code rejects the reserved
///   name unless it is sourced from a `github.com/anthropics` repo, so a
///   `directory` clone is never accepted for these. Emit a github source
///   (`{source: {source: "github", repo: "<owner>/<repo>"}}`) parsed from the
///   configured source. This needs no local clone, so it renders even unsynced.
/// - **Ordinary marketplaces**: emit a directory source pointing at llmenv's
///   local clone (`install_location`). A marketplace never synced (no install
///   location) is skipped.
fn render_marketplace_source(mk: &ResolvedMarketplace) -> Option<serde_json::Value> {
    if crate::config::is_reserved_official_marketplace(&mk.name) {
        // Validation guarantees a reserved marketplace's source is an
        // anthropics GitHub repo; render it as a github source. If parsing
        // somehow fails (e.g. resolution bypassed validation), skip rather than
        // emit a malformed entry.
        let (owner, repo) = crate::config::github_owner_repo(&mk.source)?;
        return Some(json!({
            "source": { "source": "github", "repo": format!("{owner}/{repo}") }
        }));
    }
    let location = mk.install_location.as_ref()?;
    Some(json!({ "source": { "source": "directory", "path": location } }))
}

/// Render the manifest's resolved marketplaces + plugins into `settings`.
///
/// - `extraKnownMarketplaces`: keyed by marketplace name; the per-marketplace
///   body comes from [`render_marketplace_source`] (directory clone for ordinary
///   marketplaces, github source for reserved official ones, #190).
/// - `enabledPlugins`: keyed `<plugin>@<marketplace>`, all `true`. llmenv only
///   emits plugins it wants on; it never authors a `false` (disabled) entry.
///
/// Both keys are omitted entirely when empty so a plugin-free scope produces no
/// plugin settings.
fn render_plugins(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    manifest: &MergedManifest,
) {
    if manifest.marketplaces.is_empty() && manifest.plugins.is_empty() {
        return;
    }

    let mut markets = serde_json::Map::new();
    for mk in &manifest.marketplaces {
        let Some(body) = render_marketplace_source(mk) else {
            continue;
        };
        markets.insert(mk.name.clone(), body);
    }
    if !markets.is_empty() {
        settings.insert(
            "extraKnownMarketplaces".into(),
            serde_json::Value::Object(markets),
        );
    }

    let mut enabled = serde_json::Map::new();
    for p in &manifest.plugins {
        enabled.insert(format!("{}@{}", p.plugin, p.marketplace), json!(true));
    }
    if !enabled.is_empty() {
        settings.insert("enabledPlugins".into(), serde_json::Value::Object(enabled));
    }
}

/// Render a neutral permission rule into Claude Code's string grammar.
///
/// - `{tool: Bash, pattern: "cargo *"}` → `["Bash(cargo *)"]`
/// - `{tool: Read, paths: ["./.env", "./.env.*"]}` → `["Read(./.env)", "Read(./.env.*)"]`
///   (one string per path — Claude has no multi-path rule form).
/// - `{tool: Bash}` (no pattern, no paths) → `["Bash"]` (tool-wide rule).
///
/// `pattern` and `paths` are mutually exclusive by the neutral schema's
/// intent; if both are somehow set, `pattern` wins and `paths` is ignored — the
/// neutral form documents pattern as the scalar case.
fn render_permission_rule(rule: &crate::config::PermissionRule) -> Vec<String> {
    if let Some(pattern) = &rule.pattern {
        return vec![format!("{}({})", rule.tool, pattern)];
    }
    if !rule.paths.is_empty() {
        return rule
            .paths
            .iter()
            .map(|p| format!("{}({})", rule.tool, p))
            .collect();
    }
    vec![rule.tool.clone()]
}

/// Map the neutral `PermissionMode` onto Claude Code's `defaultMode` string.
fn permission_mode_str(mode: crate::config::PermissionMode) -> &'static str {
    use crate::config::PermissionMode;
    match mode {
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Default => "default",
        PermissionMode::BypassPermissions => "bypassPermissions",
    }
}

/// Which permission action a neutral rule belongs to. Authority for native-wins
/// suppression runs deny > ask > allow (most restrictive wins), so a neutral
/// rule is only ever suppressed by a native rule in a *more* authoritative
/// action — a native deny can suppress a neutral allow, never the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::AgentAdapter;
    use super::{
        CLAUDE_JSON_FILE, CLAUDE_JSON_OWNED_SERVERS_FILE, CONFIG_CONTEXT_COMMAND,
        CONFIG_GUARD_COMMAND, CTX_DESTRUCTIVE, CTX_MUTATION, CTX_READ_ONLY, ClaudeCodeAdapter,
        HOOK_RUN_COMMAND, ICM_DESTRUCTIVE, ICM_MUTATION, ICM_READ_ONLY, MODELED_SETTINGS_KEYS,
        STALE_CHECK_COMMAND, classify_claude_path, generate_installed_plugins_json,
        generate_settings_json, is_hook_json, merge_mcp_into_claude_json, overlay_native,
        read_owned_servers, reconcile_settings, reject_modeled_keys_in_catch_all,
        render_marketplace_source, render_permission_rule, seed_install_method, strip_json_nulls,
    };
    use crate::adapter::skills::{arb_yaml_value, reject_hardcoded_config_path, validate_skills};
    use crate::config::PermissionRule;
    use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
    use crate::plugins::resolve::{ResolvedMarketplace, ResolvedPlugin};
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn marketplace(name: &str, source: &str, install: Option<&str>) -> ResolvedMarketplace {
        ResolvedMarketplace {
            name: name.into(),
            source: source.into(),
            install_location: install.map(Into::into),
            head: None,
        }
    }

    #[test]
    fn reserved_marketplace_renders_github_source_not_directory() {
        // A reserved official marketplace must be wired as a github source under
        // anthropics; a `directory` source (llmenv's normal clone) is rejected by
        // Claude Code for reserved names (#190).
        let mk = marketplace(
            "claude-plugins-official",
            "https://github.com/anthropics/claude-code",
            Some("/cache/marketplaces/claude-plugins-official"),
        );
        let src = render_marketplace_source(&mk).expect("reserved renders a source");
        // Claude Code's extraKnownMarketplaces nests the source object under a
        // `source` key, verified against a real settings.json: the github entry is
        // `{source: {source: "github", repo: "owner/repo"}}` (#190).
        assert_eq!(src["source"]["source"], serde_json::json!("github"));
        assert_eq!(
            src["source"]["repo"],
            serde_json::json!("anthropics/claude-code")
        );
        assert!(
            src["source"].get("path").is_none(),
            "no directory path for github source"
        );
    }

    #[test]
    fn reserved_marketplace_entry_matches_claude_code_shape_exactly() {
        // Pin the full entry value against the exact shape Claude Code itself
        // writes into extraKnownMarketplaces (verified against a real
        // settings.json). A flat `{source:"github",repo:...}` would be rejected
        // by Claude Code, silently defeating #190 — assert the whole object so a
        // regression to the flat form fails here, not at the user's load time.
        let mk = marketplace(
            "claude-plugins-official",
            "https://github.com/anthropics/claude-code",
            None,
        );
        let src = render_marketplace_source(&mk).expect("reserved renders");
        assert_eq!(
            src,
            serde_json::json!({
                "source": { "source": "github", "repo": "anthropics/claude-code" }
            })
        );
    }

    #[test]
    fn non_reserved_marketplace_renders_directory_source() {
        // Ordinary marketplaces keep the directory-clone behavior.
        let mk = marketplace(
            "superpowers",
            "https://github.com/example/superpowers",
            Some("/cache/marketplaces/superpowers"),
        );
        let src = render_marketplace_source(&mk).expect("synced marketplace renders");
        assert_eq!(src["source"]["source"], serde_json::json!("directory"));
        assert_eq!(
            src["source"]["path"],
            serde_json::json!("/cache/marketplaces/superpowers")
        );
    }

    #[test]
    fn non_reserved_marketplace_without_install_location_is_skipped() {
        let mk = marketplace(
            "superpowers",
            "https://github.com/example/superpowers",
            None,
        );
        assert!(render_marketplace_source(&mk).is_none());
    }

    #[test]
    fn reserved_marketplace_renders_github_even_without_install_location() {
        // The github source needs no local clone, so a reserved marketplace
        // renders regardless of whether it was synced into the cache (#190).
        let mk = marketplace(
            "claude-plugins-official",
            "git@github.com:anthropics/claude-code.git",
            None,
        );
        let src = render_marketplace_source(&mk).expect("reserved renders without sync");
        assert_eq!(
            src["source"]["repo"],
            serde_json::json!("anthropics/claude-code")
        );
    }

    proptest! {
        // A rule with a `pattern` always renders to exactly one `Tool(pattern)`
        // string, regardless of any `paths` (pattern wins per the neutral schema).
        #[test]
        fn pattern_renders_single_tool_pattern_string(
            tool in "[A-Za-z]{1,12}",
            pattern in "[^()]{0,20}",
            paths in proptest::collection::vec("[^()]{0,10}", 0..3),
        ) {
            let rule = PermissionRule { tool: tool.clone(), pattern: Some(pattern.clone()), paths };
            let out = render_permission_rule(&rule);
            prop_assert_eq!(out, vec![format!("{tool}({pattern})")]);
        }

        // With no pattern, each path yields one `Tool(path)` string, in order.
        #[test]
        fn paths_render_one_string_each_in_order(
            tool in "[A-Za-z]{1,12}",
            paths in proptest::collection::vec("[^()]{1,10}", 1..5),
        ) {
            let rule = PermissionRule { tool: tool.clone(), pattern: None, paths: paths.clone() };
            let out = render_permission_rule(&rule);
            let expected: Vec<String> = paths.iter().map(|p| format!("{tool}({p})")).collect();
            prop_assert_eq!(out, expected);
        }

        // No pattern and no paths → a bare tool-wide rule.
        #[test]
        fn bare_tool_renders_tool_name(tool in "[A-Za-z]{1,12}") {
            let rule = PermissionRule { tool: tool.clone(), pattern: None, paths: Vec::new() };
            prop_assert_eq!(render_permission_rule(&rule), vec![tool]);
        }

        // Rendering is deterministic: same input, same output, never panics.
        #[test]
        fn rendering_is_deterministic(
            tool in "[A-Za-z]{1,12}",
            pattern in proptest::option::of("[^()]{0,20}"),
            paths in proptest::collection::vec("[^()]{0,10}", 0..4),
        ) {
            let rule = PermissionRule { tool, pattern, paths };
            prop_assert_eq!(render_permission_rule(&rule), render_permission_rule(&rule));
        }

        // #107 overlay_native: a `None` fragment leaves the destination untouched.
        #[test]
        fn overlay_native_none_is_noop(seed in 0u64..1000) {
            let mut dst = serde_json::json!({ "k": seed, "nested": { "a": [1, 2] } });
            let before = dst.clone();
            overlay_native(&mut dst, None).unwrap();
            prop_assert_eq!(dst, before);
        }

        // #107 overlay_native idempotence: overlaying the same fragment twice
        // equals overlaying it once, for ANY fragment. merge_json normalizes
        // arrays on every path (insert and recursive-merge alike), so a
        // duplicate-laden source array is deduped on first overlay and the
        // second overlay is a no-op.
        #[test]
        fn overlay_native_is_idempotent(frag in arb_yaml_value(3)) {
            let mut base = serde_json::json!({ "existing": "value", "list": ["x"] });
            let mut once = base.clone();
            overlay_native(&mut once, Some(&frag)).unwrap();
            overlay_native(&mut base, Some(&frag)).unwrap();
            overlay_native(&mut base, Some(&frag)).unwrap();
            prop_assert_eq!(base, once);
        }

        // #107 overlay_native no-crash: arbitrary YAML never panics and the
        // converted fragment's own keys win on scalar collision (native is the
        // higher-precedence overlay).
        #[test]
        fn overlay_native_never_panics(frag in arb_yaml_value(4)) {
            let mut dst = serde_json::json!({});
            // Must not panic regardless of fragment shape.
            let _ = overlay_native(&mut dst, Some(&frag));
        }

        // #109 reject_modeled_keys: a fragment that is not a mapping (scalar,
        // sequence, null) is always accepted — there are no top-level keys to
        // collide with a modeled feature.
        #[test]
        fn reject_modeled_keys_accepts_non_mappings(frag in arb_non_mapping_yaml()) {
            prop_assert!(reject_modeled_keys_in_catch_all(&frag).is_ok());
        }

        // #109 reject_modeled_keys acceptance: a mapping built only from keys that
        // are NOT modeled-feature keys always passes.
        #[test]
        fn reject_modeled_keys_accepts_unmodeled_mappings(
            keys in proptest::collection::vec("[a-z]{1,10}", 0..6),
        ) {
            let mut map = serde_yaml::Mapping::new();
            for k in keys {
                if MODELED_SETTINGS_KEYS.contains(&k.as_str()) {
                    continue; // never inject a modeled key in this acceptance case
                }
                map.insert(serde_yaml::Value::String(k), serde_yaml::Value::Bool(true));
            }
            let frag = serde_yaml::Value::Mapping(map);
            prop_assert!(reject_modeled_keys_in_catch_all(&frag).is_ok());
        }

        // #109 reject_modeled_keys rejection completeness: a mapping containing ANY
        // modeled key is always rejected, regardless of other keys present.
        #[test]
        fn reject_modeled_keys_rejects_any_modeled_key(
            modeled_idx in 0usize..MODELED_SETTINGS_KEYS.len(),
            extra_keys in proptest::collection::vec("[a-z]{1,8}", 0..4),
        ) {
            let mut map = serde_yaml::Mapping::new();
            for k in extra_keys {
                map.insert(serde_yaml::Value::String(k), serde_yaml::Value::Null);
            }
            let modeled = MODELED_SETTINGS_KEYS[modeled_idx];
            map.insert(
                serde_yaml::Value::String(modeled.to_owned()),
                serde_yaml::Value::Null,
            );
            let frag = serde_yaml::Value::Mapping(map);
            let err = reject_modeled_keys_in_catch_all(&frag);
            prop_assert!(err.is_err());
            prop_assert!(err.unwrap_err().to_string().contains(modeled));
        }

        // #110 is_hook_json correctness: returns true iff the path starts with the
        // `hooks` component AND has a `.json` extension. Built from components so
        // the property holds across separators and arbitrary names.
        #[test]
        fn is_hook_json_matches_spec(
            first in "[a-z]{1,8}",
            mid in proptest::collection::vec("[a-z]{1,6}", 0..3),
            stem in "[a-z]{1,8}",
            ext in proptest::option::of("[a-z]{1,5}"),
        ) {
            let mut p = PathBuf::from(&first);
            for c in &mid {
                p.push(c);
            }
            let file = match &ext {
                Some(e) => format!("{stem}.{e}"),
                None => stem.clone(),
            };
            p.push(&file);

            let expected = first == "hooks" && ext.as_deref() == Some("json");
            prop_assert_eq!(is_hook_json(&p), expected);
        }

        // #110 is_hook_json determinism + no-panic: arbitrary path strings
        // (including special chars) classify consistently and never panic.
        #[test]
        fn is_hook_json_is_deterministic(raw in ".{0,40}") {
            let p = PathBuf::from(&raw);
            prop_assert_eq!(is_hook_json(&p), is_hook_json(&p));
        }

        // #244 producibility + roundtrip: every distinctly-named resolved MCP
        // appears under `.claude.json` → top-level `mcpServers` in valid,
        // re-parseable JSON. Remote entries carry the `type` discriminator.
        #[test]
        fn merge_mcp_roundtrips_distinct_servers(mcps in arb_distinct_mcps()) {
            let dir = tempfile::tempdir().unwrap();
            merge_mcp_into_claude_json(dir.path(), &mcps, None).unwrap();

            // No servers and no native fragment → `.claude.json` is never written.
            if mcps.is_empty() {
                prop_assert!(!dir.path().join(CLAUDE_JSON_FILE).exists());
                return Ok(());
            }

            let raw = std::fs::read_to_string(dir.path().join(CLAUDE_JSON_FILE)).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
            let servers = doc.get("mcpServers").and_then(|v| v.as_object()).unwrap();

            prop_assert_eq!(servers.len(), mcps.len());
            for m in &mcps {
                let entry = servers.get(&m.name).unwrap();
                match &m.kind {
                    ResolvedKind::Stdio { command, args, env } => {
                        prop_assert_eq!(entry.get("command").unwrap(), command);
                        // args always serialize as an array (possibly empty).
                        let got_args: Vec<&str> = entry
                            .get("args")
                            .and_then(|v| v.as_array())
                            .unwrap()
                            .iter()
                            .map(|v| v.as_str().unwrap())
                            .collect();
                        prop_assert_eq!(got_args, args.iter().map(String::as_str).collect::<Vec<_>>());
                        // env is present iff non-empty; when present, every pair
                        // round-trips.
                        if env.is_empty() {
                            prop_assert!(entry.get("env").is_none());
                        } else {
                            let got_env = entry.get("env").and_then(|v| v.as_object()).unwrap();
                            prop_assert_eq!(got_env.len(), env.len());
                            for (k, v) in env {
                                prop_assert_eq!(got_env.get(k).unwrap().as_str().unwrap(), v);
                            }
                        }
                    }
                    ResolvedKind::Remote { url, transport } => {
                        prop_assert_eq!(entry.get("url").unwrap(), url);
                        // #244: remote entries MUST carry the transport type, or
                        // Claude Code drops them.
                        let want = match transport {
                            crate::config::McpTransport::Sse => "sse",
                            _ => "http",
                        };
                        prop_assert_eq!(entry.get("type").unwrap().as_str().unwrap(), want);
                    }
                }
            }
        }

        // #244 overlay determinism: an empty native overlay onto the server set
        // is a deterministic no-op on the merged `.claude.json` content.
        #[test]
        fn merge_mcp_empty_overlay_is_deterministic(mcps in arb_distinct_mcps()) {
            let empty = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());

            let dir_a = tempfile::tempdir().unwrap();
            merge_mcp_into_claude_json(dir_a.path(), &mcps, Some(&empty)).unwrap();
            let a = std::fs::read_to_string(dir_a.path().join(CLAUDE_JSON_FILE)).ok();

            let dir_b = tempfile::tempdir().unwrap();
            merge_mcp_into_claude_json(dir_b.path(), &mcps, Some(&empty)).unwrap();
            let b = std::fs::read_to_string(dir_b.path().join(CLAUDE_JSON_FILE)).ok();

            prop_assert_eq!(a, b);
        }

        // #150/#244: the merged `.claude.json` must be mode 0o600 — same
        // owner-only invariant as ICM state and settings.json. Critical because
        // it carries the user's Claude state plus server credentials / URLs.
        #[cfg(unix)]
        #[test]
        fn merge_mcp_writes_owner_only_permissions(mcps in arb_distinct_mcps()) {
            use std::os::unix::fs::PermissionsExt;
            prop_assume!(!mcps.is_empty());
            let dir = tempfile::tempdir().unwrap();
            merge_mcp_into_claude_json(dir.path(), &mcps, None).unwrap();
            let mode = std::fs::metadata(dir.path().join(CLAUDE_JSON_FILE))
                .unwrap()
                .permissions()
                .mode();
            prop_assert_eq!(mode & 0o077, 0, "group/other bits set: {:o}", mode);
        }

        // #151/#244: merged output round-trips through serde_json — every byte
        // written deserializes back to a parsable Value with identical structure.
        #[test]
        fn merge_mcp_serde_roundtrip(mcps in arb_distinct_mcps()) {
            prop_assume!(!mcps.is_empty());
            let dir = tempfile::tempdir().unwrap();
            merge_mcp_into_claude_json(dir.path(), &mcps, None).unwrap();
            let raw = std::fs::read_to_string(dir.path().join(CLAUDE_JSON_FILE)).unwrap();
            let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse");
            // Reserialize and reparse — must produce identical structure.
            let reserialized = serde_json::to_string_pretty(&doc).expect("reserialize");
            let doc2: serde_json::Value = serde_json::from_str(&reserialized).expect("reparse");
            prop_assert_eq!(doc, doc2);
        }
    }

    // Arbitrary YAML that is never a top-level mapping (the early-return path of
    // reject_modeled_keys_in_catch_all).
    fn arb_non_mapping_yaml() -> impl Strategy<Value = serde_yaml::Value> {
        prop_oneof![
            Just(serde_yaml::Value::Null),
            any::<bool>().prop_map(serde_yaml::Value::Bool),
            any::<i64>().prop_map(|n| serde_yaml::Value::Number(n.into())),
            "[a-z]{0,8}".prop_map(serde_yaml::Value::String),
            proptest::collection::vec("[a-z]{0,6}".prop_map(serde_yaml::Value::String), 0..4)
                .prop_map(serde_yaml::Value::Sequence),
        ]
    }

    // A vector of ResolvedMcp with unique names (write_mcp_json hard-errors on
    // same-name-different-content, so the roundtrip properties require distinct
    // names to stay in the success path).
    fn arb_distinct_mcps() -> impl Strategy<Value = Vec<ResolvedMcp>> {
        proptest::collection::vec(arb_mcp(), 0..5).prop_map(|mcps| {
            let mut seen = std::collections::BTreeSet::new();
            mcps.into_iter()
                .filter(|m| seen.insert(m.name.clone()))
                .collect()
        })
    }

    fn arb_mcp() -> impl Strategy<Value = ResolvedMcp> {
        let stdio = (
            "[a-z][a-z0-9_-]{0,10}",
            "[a-z]{1,8}",
            proptest::collection::vec("[a-z]{0,6}", 0..3),
            // Sometimes empty, sometimes populated — exercises both the
            // env-omitted and env-serialized branches of write_mcp_json.
            proptest::collection::btree_map("[A-Z][A-Z_]{0,5}", "[a-z0-9]{0,8}", 0..3),
        )
            .prop_map(|(name, command, args, env)| ResolvedMcp {
                name,
                kind: ResolvedKind::Stdio { command, args, env },
                headers: std::collections::BTreeMap::new(),
                timeout: None,
                disabled_tools: vec![],
            });
        let remote =
            ("[a-z][a-z0-9_-]{0,10}", "https://[a-z]{1,8}\\.test").prop_map(|(name, url)| {
                ResolvedMcp {
                    name,
                    kind: ResolvedKind::Remote {
                        url,
                        transport: crate::config::McpTransport::Http,
                    },
                    headers: std::collections::BTreeMap::new(),
                    timeout: None,
                    disabled_tools: vec![],
                }
            });
        prop_oneof![stdio, remote]
    }

    // ---- generate_settings_json: permission render ----

    fn render_settings_for_test(manifest: &crate::merge::MergedManifest) -> serde_json::Value {
        let tmp = tempfile::tempdir().unwrap();
        // generate_settings_json takes a directory and writes settings.json inside it.
        generate_settings_json(tmp.path(), manifest).unwrap();
        let bytes = std::fs::read(tmp.path().join("settings.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Every `command` string registered for a native hook event (across all
    /// matcher-group entries), flattened for easy `contains`/`any` assertions.
    fn hook_commands_for(settings: &serde_json::Value, event: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .flat_map(|e| e["hooks"].as_array().cloned().unwrap_or_default())
                    .filter_map(|h| h["command"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn baseline_injects_sessionstart_sessionend_only() {
        // Default SessionLog has transcript enabled at info, so turn hooks
        // register. Explicitly disable all sinks for the baseline check.
        let manifest = crate::merge::MergedManifest {
            session_log: crate::config::SessionLog {
                transcript: Some(crate::config::TranscriptSinkConfig {
                    enabled: false,
                    level: crate::config::LogLevel::Info,
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);

        assert!(
            hook_commands_for(&settings, "SessionStart")
                .contains(&format!("{HOOK_RUN_COMMAND} session_start"))
        );
        assert!(
            hook_commands_for(&settings, "SessionEnd")
                .contains(&format!("{HOOK_RUN_COMMAND} session_end"))
        );
        // PreToolUse now always has a hook-run command for the read-once hook
        // (#318 unconditional registration).
        assert!(
            hook_commands_for(&settings, "PreToolUse")
                .iter()
                .any(|c| c.starts_with(HOOK_RUN_COMMAND)),
            "PreToolUse must carry a hook-run command for read-once"
        );
        for event in [
            "PostToolUse",
            "UserPromptSubmit",
            "Stop",
            "SubagentStop",
            "Notification",
            "PreCompact",
        ] {
            assert!(
                hook_commands_for(&settings, event)
                    .iter()
                    .all(|c| !c.starts_with(HOOK_RUN_COMMAND)),
                "{event} must not carry a hook-run command when all sinks are disabled; got {:?}",
                hook_commands_for(&settings, event)
            );
        }
    }

    #[test]
    fn turn_start_wired_when_memory_backend_active() {
        // #499: UserPromptSubmit gets the turn_start hook-run command only when
        // a memory backend (the `icm` MCP) resolved for this scope — reuses the
        // same manifest.mcps signal as autoMemoryEnabled, no new config field.
        let manifest = crate::merge::MergedManifest {
            mcps: vec![crate::mcp::resolve::ResolvedMcp {
                name: crate::mcp::resolve::MEMORY_MCP_NAME.to_string(),
                kind: crate::mcp::resolve::ResolvedKind::Remote {
                    url: "http://localhost:9999".into(),
                    transport: crate::config::McpTransport::Http,
                },
                headers: Default::default(),
                timeout: None,
                disabled_tools: vec![],
            }],
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        assert!(
            hook_commands_for(&settings, "UserPromptSubmit")
                .contains(&format!("{HOOK_RUN_COMMAND} turn_start"))
        );
    }

    #[test]
    fn turn_start_not_wired_without_memory_backend() {
        // No memory MCP resolved for this scope → no per-prompt hook-run call,
        // avoiding the latency cost on every turn when nothing would use it.
        let manifest = crate::merge::MergedManifest::default();
        let settings = render_settings_for_test(&manifest);
        assert!(
            hook_commands_for(&settings, "UserPromptSubmit")
                .iter()
                .all(|c| !c.contains("turn_start")),
        );
    }

    #[test]
    fn session_log_injects_all_turn_hooks_when_sink_enabled() {
        let manifest = crate::merge::MergedManifest {
            session_log: crate::config::SessionLog {
                transcript: Some(crate::config::TranscriptSinkConfig {
                    enabled: true,
                    level: crate::config::LogLevel::Info,
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);

        for (event, neutral) in [
            ("UserPromptSubmit", "user_prompt_submit"),
            ("PreToolUse", "pre_tool_use"),
            ("PostToolUse", "post_tool_use"),
            ("Notification", "notification"),
            ("Stop", "stop"),
            ("SubagentStop", "subagent_stop"),
            ("PreCompact", "pre_compact"),
        ] {
            let expected = format!("{HOOK_RUN_COMMAND} {neutral}");
            assert!(
                hook_commands_for(&settings, event).contains(&expected),
                "{event} missing {expected:?}; got {:?}",
                hook_commands_for(&settings, event)
            );
        }
        // Baseline hooks remain present too.
        assert!(
            hook_commands_for(&settings, "SessionStart")
                .contains(&format!("{HOOK_RUN_COMMAND} session_start"))
        );
    }

    #[test]
    fn context_mode_plugin_grants_tiered_mcp_permissions() {
        // #694: context-mode plugin in manifest → read-only tools in allow,
        // mutation in ask, destructive in deny.
        let manifest = crate::merge::MergedManifest {
            plugins: vec![crate::plugins::resolve::ResolvedPlugin {
                marketplace: crate::config::CONTEXT_MODE_MARKETPLACE.into(),
                plugin: crate::config::CONTEXT_MODE_PLUGIN.into(),
                collection: "context_mode (built-in)".into(),
                install_path: None,
                git_commit_sha: None,
            }],
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        let ask = settings["permissions"]["ask"].as_array().unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        let prefix = crate::config::CONTEXT_MODE_MCP_PREFIX;
        for &tool in CTX_READ_ONLY {
            let expected = format!("{prefix}{tool}");
            assert!(
                allow.iter().any(|v| v == &expected),
                "expected {expected:?} in allow, got {allow:?}"
            );
        }
        for &tool in CTX_MUTATION {
            let expected = format!("{prefix}{tool}");
            assert!(
                ask.iter().any(|v| v == &expected),
                "expected {expected:?} in ask, got {ask:?}"
            );
        }
        for &tool in CTX_DESTRUCTIVE {
            let expected = format!("{prefix}{tool}");
            assert!(
                deny.iter().any(|v| v == &expected),
                "expected {expected:?} in deny, got {deny:?}"
            );
        }
    }

    #[test]
    fn context_mode_absent_no_tiered_permissions() {
        // #694: no context-mode plugin → no ctx_* rules in any permission array.
        let manifest = crate::merge::MergedManifest::default();
        let settings = render_settings_for_test(&manifest);
        let allow = settings
            .get("permissions")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        let ask = settings
            .get("permissions")
            .and_then(|p| p.get("ask"))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        let deny = settings
            .get("permissions")
            .and_then(|p| p.get("deny"))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        let prefix = crate::config::CONTEXT_MODE_MCP_PREFIX;
        for v in allow.iter().chain(ask.iter()).chain(deny.iter()) {
            assert!(
                !v.as_str().is_some_and(|s| s.starts_with(prefix)),
                "ctx tool rule found when plugin absent: {v:?}"
            );
        }
    }

    #[test]
    fn icm_active_grants_tiered_permissions() {
        // #694: ICM MCP active → read-only tools in allow, mutation in ask,
        // destructive in deny.
        let manifest = crate::merge::MergedManifest {
            mcps: vec![crate::mcp::resolve::ResolvedMcp {
                name: crate::mcp::resolve::MEMORY_MCP_NAME.to_string(),
                kind: crate::mcp::resolve::ResolvedKind::Remote {
                    url: "http://localhost:9999".into(),
                    transport: crate::config::McpTransport::Http,
                },
                headers: Default::default(),
                timeout: None,
                disabled_tools: vec![],
            }],
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        let ask = settings["permissions"]["ask"].as_array().unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        let icm_prefix = format!("mcp__{}__", crate::mcp::resolve::MEMORY_MCP_NAME);
        for &tool in ICM_READ_ONLY {
            let expected = format!("{icm_prefix}{tool}");
            assert!(
                allow.iter().any(|v| v == &expected),
                "expected ICM read-only {expected:?} in allow, got {allow:?}"
            );
        }
        for &tool in ICM_MUTATION {
            let expected = format!("{icm_prefix}{tool}");
            assert!(
                ask.iter().any(|v| v == &expected),
                "expected ICM mutation {expected:?} in ask, got {ask:?}"
            );
        }
        for &tool in ICM_DESTRUCTIVE {
            let expected = format!("{icm_prefix}{tool}");
            assert!(
                deny.iter().any(|v| v == &expected),
                "expected ICM destructive {expected:?} in deny, got {deny:?}"
            );
        }
    }

    #[test]
    fn icm_absent_no_tiered_rules() {
        // #694: no ICM MCP → no icm_* rules in any permission array.
        let manifest = crate::merge::MergedManifest::default();
        let settings = render_settings_for_test(&manifest);
        let allow = settings
            .get("permissions")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        let ask = settings
            .get("permissions")
            .and_then(|p| p.get("ask"))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        let deny = settings
            .get("permissions")
            .and_then(|p| p.get("deny"))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        for v in allow.iter().chain(ask.iter()).chain(deny.iter()) {
            assert!(
                !v.as_str().is_some_and(|s| s.starts_with("mcp__icm__")),
                "ICM tool rule found when MCP absent: {v:?}"
            );
        }
    }

    #[test]
    fn icm_and_context_mode_both_grant_tiered_rules() {
        // #694: both ICM and context-mode active → tools from both in their
        // correct arrays.
        let manifest = crate::merge::MergedManifest {
            mcps: vec![crate::mcp::resolve::ResolvedMcp {
                name: crate::mcp::resolve::MEMORY_MCP_NAME.to_string(),
                kind: crate::mcp::resolve::ResolvedKind::Remote {
                    url: "http://localhost:9999".into(),
                    transport: crate::config::McpTransport::Http,
                },
                headers: Default::default(),
                timeout: None,
                disabled_tools: vec![],
            }],
            plugins: vec![crate::plugins::resolve::ResolvedPlugin {
                marketplace: crate::config::CONTEXT_MODE_MARKETPLACE.into(),
                plugin: crate::config::CONTEXT_MODE_PLUGIN.into(),
                collection: "context_mode (built-in)".into(),
                install_path: None,
                git_commit_sha: None,
            }],
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        let ask = settings["permissions"]["ask"].as_array().unwrap();
        let deny = settings["permissions"]["deny"].as_array().unwrap();
        let icm_prefix = format!("mcp__{}__", crate::mcp::resolve::MEMORY_MCP_NAME);
        let ctx_prefix = crate::config::CONTEXT_MODE_MCP_PREFIX;
        // ICM tools
        for &tool in ICM_READ_ONLY {
            let expected = format!("{icm_prefix}{tool}");
            assert!(
                allow.iter().any(|v| v == &expected),
                "ICM read-only {expected:?} not in allow"
            );
        }
        for &tool in ICM_MUTATION {
            let expected = format!("{icm_prefix}{tool}");
            assert!(
                ask.iter().any(|v| v == &expected),
                "ICM mutation {expected:?} not in ask"
            );
        }
        for &tool in ICM_DESTRUCTIVE {
            let expected = format!("{icm_prefix}{tool}");
            assert!(
                deny.iter().any(|v| v == &expected),
                "ICM destructive {expected:?} not in deny"
            );
        }
        // Context-mode tools
        for &tool in CTX_READ_ONLY {
            let expected = format!("{ctx_prefix}{tool}");
            assert!(
                allow.iter().any(|v| v == &expected),
                "ctx read-only {expected:?} not in allow"
            );
        }
        for &tool in CTX_MUTATION {
            let expected = format!("{ctx_prefix}{tool}");
            assert!(
                ask.iter().any(|v| v == &expected),
                "ctx mutation {expected:?} not in ask"
            );
        }
        for &tool in CTX_DESTRUCTIVE {
            let expected = format!("{ctx_prefix}{tool}");
            assert!(
                deny.iter().any(|v| v == &expected),
                "ctx destructive {expected:?} not in deny"
            );
        }
    }

    #[test]
    fn bash_ban_env_no_longer_adds_deny_rules() {
        // Regression guard (#490 / #464): LLMENV_BASH_BAN wiring was removed; a
        // default manifest with no deny config must produce no Bash deny rules.
        // (Can't set the env var in tests — unsafe_code is forbidden project-wide.)
        let manifest = crate::merge::MergedManifest::default();
        let settings = render_settings_for_test(&manifest);
        let deny = settings
            .get("permissions")
            .and_then(|p| p.get("deny"))
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            !deny
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.starts_with("Bash("))),
            "no Bash deny rules expected from empty manifest; got {deny:?}"
        );
    }

    // ---- reconcile_settings (#196 / #175): settings.json is shared, not owned ----

    fn write_json(path: &std::path::Path, v: &serde_json::Value) {
        std::fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    }

    #[test]
    fn reconcile_absent_file_returns_fresh_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let fresh = serde_json::json!({ "permissions": { "deny": ["X"] } });
        let out = reconcile_settings(&path, fresh.clone()).unwrap();
        assert_eq!(
            out, fresh,
            "no prior file → llmenv's render is the whole truth"
        );
    }

    #[test]
    fn reconcile_preserves_foreign_top_level_keys() {
        // #175: a plugin self-registered a top-level key. A re-render must keep it.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        write_json(
            &path,
            &serde_json::json!({
                "permissions": { "deny": ["STALE"] },
                "contextModeState": { "session": "abc" }
            }),
        );
        let fresh = serde_json::json!({ "permissions": { "deny": ["FRESH"] } });
        let out = reconcile_settings(&path, fresh).unwrap();
        // Owned key replaced authoritatively; foreign key untouched.
        assert_eq!(out["permissions"]["deny"], serde_json::json!(["FRESH"]));
        assert_eq!(out["contextModeState"]["session"], "abc");
    }

    #[test]
    fn reconcile_unions_hooks_so_plugin_registration_survives() {
        // A plugin self-registered a SessionStart hook into settings.json after
        // llmenv last wrote it. llmenv's re-render must merge, not clobber.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        write_json(
            &path,
            &serde_json::json!({
                "hooks": { "SessionStart": [{ "command": "plugin-hook" }] }
            }),
        );
        let fresh = serde_json::json!({
            "hooks": { "SessionStart": [{ "command": "llmenv-hook" }] }
        });
        let out = reconcile_settings(&path, fresh).unwrap();
        let entries = out["hooks"]["SessionStart"].as_array().unwrap();
        let cmds: Vec<&str> = entries
            .iter()
            .filter_map(|e| e["command"].as_str())
            .collect();
        assert!(
            cmds.contains(&"plugin-hook"),
            "plugin hook survives: {cmds:?}"
        );
        assert!(
            cmds.contains(&"llmenv-hook"),
            "llmenv hook present: {cmds:?}"
        );
    }

    #[test]
    fn reconcile_hooks_union_dedups_across_renders() {
        // Re-rendering the same llmenv hook must not pile up duplicates.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let llmenv_hook = serde_json::json!({
            "hooks": { "SessionStart": [{ "command": "llmenv-hook" }] }
        });
        write_json(&path, &llmenv_hook);
        let out = reconcile_settings(&path, llmenv_hook.clone()).unwrap();
        let entries = out["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "identical hook deduped, not doubled");
    }

    #[test]
    fn reconcile_hooks_dedups_cross_render_null_vs_absent_tool() {
        // #699: A hook entry on disk with `"tool": null` (from an older render
        // that serialized the Option as JSON null) must dedup against a fresh
        // hook that omits `"tool"` entirely (the current
        // generate_settings_json). The difference between null and absent
        // makes JSON PartialEq consider them unequal — strip_json_nulls + re-
        // dedup after merge_json must handle this.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // Existing on disk: has "tool": null in the inner handler.
        write_json(
            &path,
            &serde_json::json!({
                "hooks": {
                    "PostToolUse": [
                        {
                            "hooks": [{ "command": "lint.sh", "tool": null, "type": "command" }],
                            "matcher": "Edit|Write"
                        }
                    ]
                }
            }),
        );
        // Fresh render: same hook, but "tool" omitted entirely (not null).
        let fresh = serde_json::json!({
            "hooks": {
                "PostToolUse": [
                    {
                        "hooks": [{ "command": "lint.sh", "type": "command" }],
                        "matcher": "Edit|Write"
                    }
                ]
            }
        });
        let out = reconcile_settings(&path, fresh).unwrap();
        let entries = out["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "null-vs-absent tool deduped, not doubled");
    }

    #[test]
    fn reconcile_hooks_dedups_with_native_overlay_nulls() {
        // #699: Same as the null-vs-absent test but also verifies that
        // nested null keys in the inner handler and outer entry are all
        // stripped — the dedup must handle objects with null at any depth.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        write_json(
            &path,
            &serde_json::json!({
                "hooks": {
                    "SessionStart": [
                        {
                            "hooks": [{ "command": "check.sh", "tool": null, "type": "command" }],
                            "tool": null
                        }
                    ]
                }
            }),
        );
        let fresh = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [{ "command": "check.sh", "type": "command" }]
                    }
                ]
            }
        });
        let out = reconcile_settings(&path, fresh).unwrap();
        let entries = out["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "nulls at any depth stripped before dedup");
    }

    #[test]
    fn strip_json_nulls_removes_null_vals() {
        let mut v = serde_json::json!({
            "a": null,
            "b": 1,
            "c": { "d": null, "e": [{"f": null, "g": 2}] }
        });
        strip_json_nulls(&mut v);
        assert_eq!(
            v,
            serde_json::json!({
                "b": 1,
                "c": { "e": [{ "g": 2 }] }
            })
        );
    }

    fn contains_no_nulls(v: &serde_json::Value) -> bool {
        match v {
            // Only check for null-valued *keys in objects* — that's what
            // strip_json_nulls removes. Bare null or null array elements
            // are not touched, so don't flag them.
            serde_json::Value::Array(items) => items.iter().all(contains_no_nulls),
            serde_json::Value::Object(map) => {
                !map.values().any(|v| v.is_null()) && map.values().all(contains_no_nulls)
            }
            _ => true,
        }
    }

    fn count_non_null_leaves(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Null => 0,
            serde_json::Value::Array(items) => items.iter().map(count_non_null_leaves).sum(),
            serde_json::Value::Object(map) => map.values().map(count_non_null_leaves).sum(),
            _ => 1,
        }
    }

    fn arb_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i32>().prop_map(serde_json::Value::from),
            "[a-z]{0,4}".prop_map(serde_json::Value::String),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
                prop::collection::vec(("[a-z]{1,4}", inner), 0..4)
                    .prop_map(|kvs| serde_json::Value::Object(kvs.into_iter().collect())),
            ]
        })
    }

    proptest! {
        // strip_json_nulls never panics on arbitrary JSON input.
        #[test]
        fn strip_json_nulls_total(mut v in arb_json()) {
            strip_json_nulls(&mut v);
        }

        // Idempotency: applying strip_json_nulls twice equals applying it once.
        #[test]
        fn strip_json_nulls_idempotent(v in arb_json()) {
            let mut once = v.clone();
            strip_json_nulls(&mut once);
            let mut twice = once.clone();
            strip_json_nulls(&mut twice);
            prop_assert_eq!(once, twice);
        }

        // Completeness: after strip_json_nulls, no Value::Null exists at any depth.
        #[test]
        fn strip_json_nulls_no_nulls_remain(mut v in arb_json()) {
            strip_json_nulls(&mut v);
            prop_assert!(contains_no_nulls(&v), "null values remain after strip_json_nulls");
        }

        // Non-null preservation: non-null leaf values are structurally preserved.
        #[test]
        fn strip_json_nulls_preserves_non_null(mut v in arb_json()) {
            let expected = count_non_null_leaves(&v);
            strip_json_nulls(&mut v);
            let actual = count_non_null_leaves(&v);
            prop_assert_eq!(expected, actual,
                "strip_json_nulls should not remove non-null values");
        }
    }

    #[test]
    fn reconcile_drops_owned_key_llmenv_no_longer_renders() {
        // All plugins removed → llmenv renders no `enabledPlugins`; a stale value
        // on disk must be cleared, not left to keep enabling a dropped plugin.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        write_json(
            &path,
            &serde_json::json!({ "enabledPlugins": { "old@market": true } }),
        );
        let fresh = serde_json::json!({ "permissions": { "deny": [] } });
        let out = reconcile_settings(&path, fresh).unwrap();
        assert!(
            out.get("enabledPlugins").is_none(),
            "stale owned key cleared on re-render"
        );
    }

    #[test]
    fn reconcile_corrupt_file_falls_back_to_fresh() {
        // A hand-corrupted settings.json must not abort the render or strand
        // llmenv config — llmenv's render wins outright.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let fresh = serde_json::json!({ "permissions": { "deny": ["X"] } });
        let out = reconcile_settings(&path, fresh.clone()).unwrap();
        assert_eq!(out, fresh);
    }

    #[test]
    fn reconcile_native_passthrough_written_on_rerender() {
        // Native-overlay keys (e.g. `statusLine`, `cleanupPeriodDays`) that llmenv
        // computes into `fresh` but that are not in LLMENV_OWNED_SETTINGS_KEYS must
        // be written through on every re-render, not silently dropped because the
        // file already exists.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // Simulate an existing file that has no statusLine yet.
        write_json(&path, &serde_json::json!({ "permissions": { "deny": [] } }));
        let fresh = serde_json::json!({
            "permissions": { "deny": [] },
            "statusLine": { "type": "command", "command": "my-status-script" },
            "cleanupPeriodDays": 365,
        });
        let out = reconcile_settings(&path, fresh).unwrap();
        assert_eq!(
            out["statusLine"]["command"], "my-status-script",
            "native passthrough key must survive re-render"
        );
        assert_eq!(out["cleanupPeriodDays"], 365);
    }

    // ---- merge_mcp_into_claude_json (#244): mcpServers into .claude.json ----

    fn stdio_mcp(name: &str, command: &str) -> ResolvedMcp {
        ResolvedMcp {
            name: name.into(),
            kind: ResolvedKind::Stdio {
                command: command.into(),
                args: vec![],
                env: std::collections::BTreeMap::new(),
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
        }
    }

    fn remote_mcp(name: &str, url: &str, transport: crate::config::McpTransport) -> ResolvedMcp {
        ResolvedMcp {
            name: name.into(),
            kind: ResolvedKind::Remote {
                url: url.into(),
                transport,
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
        }
    }

    #[test]
    fn merge_mcp_preserves_foreign_keys_and_servers() {
        // #244 acceptance: a pre-existing .claude.json carries Claude's own
        // state (oauthAccount, numStartups) plus a user-added MCP server. A
        // re-export must upsert llmenv's server WITHOUT disturbing any of it.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_FILE);
        write_json(
            &path,
            &serde_json::json!({
                "oauthAccount": { "email": "x@y.z" },
                "numStartups": 42,
                "mcpServers": { "user-added": { "command": "foo" } }
            }),
        );
        merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("icm", "icm-bin")], None).unwrap();

        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // Foreign top-level keys untouched.
        assert_eq!(doc["oauthAccount"]["email"], "x@y.z");
        assert_eq!(doc["numStartups"], 42);
        // Foreign server preserved alongside llmenv's upsert.
        assert_eq!(doc["mcpServers"]["user-added"]["command"], "foo");
        assert_eq!(doc["mcpServers"]["icm"]["command"], "icm-bin");
    }

    #[test]
    fn merge_mcp_remote_entry_carries_type() {
        // #244 gap #2: remote servers MUST emit "type" or Claude drops them.
        let tmp = tempfile::tempdir().unwrap();
        merge_mcp_into_claude_json(
            tmp.path(),
            &[remote_mcp(
                "icm",
                "http://still.local:9092/mcp",
                crate::config::McpTransport::Http,
            )],
            None,
        )
        .unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join(CLAUDE_JSON_FILE)).unwrap())
                .unwrap();
        assert_eq!(doc["mcpServers"]["icm"]["type"], "http");
        assert_eq!(
            doc["mcpServers"]["icm"]["url"],
            "http://still.local:9092/mcp"
        );
    }

    #[test]
    fn merge_mcp_sse_remote_emits_sse_type() {
        let tmp = tempfile::tempdir().unwrap();
        merge_mcp_into_claude_json(
            tmp.path(),
            &[remote_mcp(
                "ev",
                "http://h/sse",
                crate::config::McpTransport::Sse,
            )],
            None,
        )
        .unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join(CLAUDE_JSON_FILE)).unwrap())
                .unwrap();
        assert_eq!(doc["mcpServers"]["ev"]["type"], "sse");
    }

    #[test]
    fn merge_mcp_creates_file_when_absent() {
        // No pre-existing .claude.json: a fresh doc with only mcpServers is born.
        let tmp = tempfile::tempdir().unwrap();
        merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("icm", "icm-bin")], None).unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join(CLAUDE_JSON_FILE)).unwrap())
                .unwrap();
        assert_eq!(doc["mcpServers"]["icm"]["command"], "icm-bin");
        assert!(doc.as_object().unwrap().len() == 1, "only mcpServers key");
    }

    #[test]
    fn merge_mcp_refuses_to_clobber_corrupt_file() {
        // .claude.json is overwhelmingly foreign state. A parse failure must
        // abort rather than replace it with a fresh doc (data-loss guard).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_FILE);
        std::fs::write(&path, b"{ not valid json").unwrap();
        let err = merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("icm", "icm-bin")], None)
            .unwrap_err();
        assert!(
            err.to_string().contains("not valid JSON"),
            "expected refusal, got: {err}"
        );
        // Original bytes left intact.
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not valid json");
    }

    #[test]
    fn merge_mcp_no_servers_no_native_leaves_no_file() {
        // Nothing to write → .claude.json is never created.
        let tmp = tempfile::tempdir().unwrap();
        merge_mcp_into_claude_json(tmp.path(), &[], None).unwrap();
        assert!(!tmp.path().join(CLAUDE_JSON_FILE).exists());
    }

    #[test]
    fn merge_mcp_overlays_native_server_fragment() {
        // #97: a native_mcp fragment injects an engine-specific server entry,
        // which merges into mcpServers alongside the resolved set.
        let tmp = tempfile::tempdir().unwrap();
        let native: serde_yaml::Value =
            serde_yaml::from_str("mcpServers:\n  extra:\n    command: native-bin\n").unwrap();
        merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("icm", "icm-bin")], Some(&native))
            .unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join(CLAUDE_JSON_FILE)).unwrap())
                .unwrap();
        assert_eq!(doc["mcpServers"]["icm"]["command"], "icm-bin");
        assert_eq!(doc["mcpServers"]["extra"]["command"], "native-bin");
        // enabledMcpjsonServers is never emitted into .claude.json (#244).
        assert!(doc.get("enabledMcpjsonServers").is_none());
    }

    #[test]
    fn merge_mcp_prunes_stale_owned_servers() {
        // #739: a server llmenv previously owned but no longer resolves must
        // be removed from .claude.json, while foreign servers are preserved.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_FILE);
        write_json(
            &path,
            &serde_json::json!({
                "mcpServers": {
                    "stale-srv": { "command": "stale-bin" },
                    "user-added": { "command": "user-bin" },
                    "current-srv": { "command": "current-bin" }
                }
            }),
        );
        // Pre-populate companion file: llmenv owned stale-srv and current-srv.
        let owned_path = tmp.path().join(CLAUDE_JSON_OWNED_SERVERS_FILE);
        std::fs::write(&owned_path, br#"["stale-srv", "current-srv"]"#).unwrap();

        merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("current-srv", "current-bin")], None)
            .unwrap();

        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // Stale server pruned.
        assert!(
            doc["mcpServers"].get("stale-srv").is_none(),
            "stale server must be pruned"
        );
        // Foreign server preserved.
        assert_eq!(doc["mcpServers"]["user-added"]["command"], "user-bin");
        // Current server upserted.
        assert_eq!(doc["mcpServers"]["current-srv"]["command"], "current-bin");
        // Companion file updated: only current-srv remains.
        let owned: Vec<String> =
            serde_json::from_slice(&std::fs::read(&owned_path).unwrap()).unwrap();
        assert_eq!(owned, vec!["current-srv"]);
    }

    #[test]
    fn merge_mcp_preserves_foreign_when_no_owned() {
        // No companion file → first render; no servers are owned, so no
        // pruning occurs. Foreign servers survive the upsert.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_FILE);
        write_json(
            &path,
            &serde_json::json!({
                "mcpServers": {
                    "user-added": { "command": "user-bin" }
                }
            }),
        );
        merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("icm", "icm-bin")], None).unwrap();

        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["user-added"]["command"], "user-bin");
        assert_eq!(doc["mcpServers"]["icm"]["command"], "icm-bin");
        // Companion file created with the current owned name.
        let owned_path = tmp.path().join(CLAUDE_JSON_OWNED_SERVERS_FILE);
        let owned: Vec<String> =
            serde_json::from_slice(&std::fs::read(&owned_path).unwrap()).unwrap();
        assert_eq!(owned, vec!["icm"]);
    }

    #[test]
    fn merge_mcp_corrupt_companion_file_treated_as_empty() {
        // #739: a corrupt companion file (not valid JSON) is treated as empty,
        // so no pruning occurs — foreign servers survive, and the companion file
        // is overwritten with the current owned set.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_FILE);
        write_json(
            &path,
            &serde_json::json!({
                "mcpServers": {
                    "user-added": { "command": "user-bin" }
                }
            }),
        );
        let owned_path = tmp.path().join(CLAUDE_JSON_OWNED_SERVERS_FILE);
        std::fs::write(&owned_path, b"not valid json").unwrap();

        merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("icm", "icm-bin")], None).unwrap();

        // Foreign server preserved despite corrupt companion file.
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["user-added"]["command"], "user-bin");
        // Companion file overwritten with the current owned servers.
        let owned: Vec<String> =
            serde_json::from_slice(&std::fs::read(&owned_path).unwrap()).unwrap();
        assert_eq!(owned, vec!["icm"]);
    }

    #[test]
    fn merge_mcp_empty_servers_removes_companion_file() {
        // #739: when no llmenv MCP servers are resolved, the companion file
        // should be removed (not written with []).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_FILE);
        write_json(
            &path,
            &serde_json::json!({
                "mcpServers": {
                    "user-added": { "command": "user-bin" },
                    "stale-srv": { "command": "stale-bin" }
                }
            }),
        );
        let owned_path = tmp.path().join(CLAUDE_JSON_OWNED_SERVERS_FILE);
        std::fs::write(&owned_path, br#"["stale-srv"]"#).unwrap();

        // No llmenv servers → stale-srv is pruned from .claude.json and companion
        // file is removed.
        merge_mcp_into_claude_json(tmp.path(), &[], None).unwrap();

        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            doc["mcpServers"].get("stale-srv").is_none(),
            "stale server pruned"
        );
        // Foreign server preserved.
        assert_eq!(doc["mcpServers"]["user-added"]["command"], "user-bin");
        // Companion file removed.
        assert!(
            !owned_path.exists(),
            "companion file removed when no owned servers"
        );
    }

    // #311: hardcoded config-path rejection.

    #[test]
    fn reject_hardcoded_config_path_flags_tilde_claude() {
        let err = reject_hardcoded_config_path("run ~/.claude/skills/x/s.sh", "SKILL.md");
        assert!(err.is_err());
    }

    #[test]
    fn reject_hardcoded_config_path_flags_home_claude() {
        let err = reject_hardcoded_config_path("$HOME/.claude/skills/x", "rules/a.md");
        assert!(err.is_err());
    }

    #[test]
    fn reject_hardcoded_config_path_allows_plugin_root() {
        let ok = reject_hardcoded_config_path("${CLAUDE_PLUGIN_ROOT}/scripts/s.sh", "SKILL.md");
        assert!(ok.is_ok());
    }

    #[test]
    fn reject_hardcoded_config_path_inline_suppress_skips_line() {
        let content = "run ~/.claude/skills/x/s.sh  # llmenv-ignore: hardcoded-path\nclean line";
        assert!(reject_hardcoded_config_path(content, "SKILL.md").is_ok());
    }

    #[test]
    fn reject_hardcoded_config_path_inline_suppress_only_skips_that_line() {
        let content =
            "run ~/.claude/skills/x/s.sh  # llmenv-ignore: hardcoded-path\nrun ~/.claude/other";
        assert!(reject_hardcoded_config_path(content, "SKILL.md").is_err());
    }

    #[test]
    fn reject_hardcoded_config_path_file_suppress_skips_entire_file() {
        let content = "# llmenv-ignore-file: hardcoded-path\nrun ~/.claude/skills/x/s.sh\nmore ~/.claude/stuff";
        assert!(reject_hardcoded_config_path(content, "SKILL.md").is_ok());
    }

    fn write_skill(skills_dir: &std::path::Path, name: &str, files: &[(&str, &str)]) {
        let dir = skills_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, content) in files {
            let dest = dir.join(rel);
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p).unwrap();
            }
            std::fs::write(dest, content).unwrap();
        }
    }

    const VALID_FRONTMATTER: &str = "---\nname: x\ndescription: y\n---\nbody\n";

    #[test]
    fn validate_skills_passes_clean_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        write_skill(&skills, "good", &[("SKILL.md", VALID_FRONTMATTER)]);
        validate_skills(tmp.path()).unwrap();
    }

    #[test]
    fn validate_skills_flags_hardcoded_path_in_helper_script() {
        // The path lives in a bundled script, NOT in SKILL.md — the old check
        // (SKILL.md only) would have missed it.
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        write_skill(
            &skills,
            "leaky",
            &[
                ("SKILL.md", VALID_FRONTMATTER),
                (
                    "scripts/run.sh",
                    "#!/bin/sh\nexec ~/.claude/skills/leaky/x\n",
                ),
            ],
        );
        let err = validate_skills(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("hardcoded"), "got: {err}");
    }

    #[test]
    fn validate_skills_missing_skill_md_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        write_skill(&skills, "empty", &[("notes.md", "hi")]);
        let err = validate_skills(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("missing SKILL.md"), "got: {err}");
    }

    #[test]
    fn classify_claude_path_detects_homebrew() {
        assert_eq!(classify_claude_path("/opt/homebrew/bin/claude"), "homebrew");
        assert_eq!(
            classify_claude_path("/usr/local/Cellar/claude-code/1.0/bin/claude"),
            "homebrew"
        );
        assert_eq!(
            classify_claude_path("/home/linuxbrew/.linuxbrew/bin/claude"),
            "homebrew"
        );
    }

    #[test]
    fn classify_claude_path_detects_npm() {
        assert_eq!(
            classify_claude_path("/usr/local/lib/node_modules/.bin/claude"),
            "npm"
        );
        assert_eq!(
            classify_claude_path("/home/user/.nvm/versions/node/v20/bin/claude"),
            "npm"
        );
        assert_eq!(classify_claude_path("/home/user/.npm/bin/claude"), "npm");
    }

    #[test]
    fn classify_claude_path_falls_back_to_native() {
        assert_eq!(classify_claude_path("/usr/local/bin/claude"), "native");
        assert_eq!(
            classify_claude_path("/home/user/.local/bin/claude"),
            "native"
        );
        assert_eq!(classify_claude_path(""), "native");
    }

    #[test]
    fn classify_claude_path_detects_volta_fnm_pnpm() {
        assert_eq!(classify_claude_path("/home/user/.volta/bin/claude"), "npm");
        assert_eq!(
            classify_claude_path("/home/user/.fnm/node-versions/v20/bin/claude"),
            "npm"
        );
        assert_eq!(
            classify_claude_path("/home/user/.local/share/pnpm/claude"),
            "npm"
        );
        assert_eq!(
            classify_claude_path("/Users/user/Library/pnpm/claude"),
            "npm"
        );
    }

    #[test]
    fn seed_install_method_skips_when_already_present() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings.json");
        let existing = serde_json::json!({
            "installMethod": "homebrew",
            "otherKey": "value"
        });
        std::fs::write(&settings, existing.to_string()).unwrap();

        seed_install_method(tmp.path()).unwrap();

        let content = std::fs::read_to_string(&settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        // installMethod should remain unchanged from existing
        assert_eq!(json["installMethod"], "homebrew");
        assert_eq!(json["otherKey"], "value");
    }

    #[cfg(unix)]
    #[test]
    fn validate_skills_rejects_symlink_escape() {
        // A skill dir that is a symlink pointing outside skills/ must be refused,
        // not followed into a foreign tree (#311 symlink-escape hardening).
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("SKILL.md"), VALID_FRONTMATTER).unwrap();
        std::os::unix::fs::symlink(&outside, skills.join("evil")).unwrap();
        let err = validate_skills(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }

    #[test]
    fn reconcile_preserves_context_mode_self_registered_hook() {
        use serde_json::json;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // Simulate a prior render where context-mode's start.mjs added a cache-heal
        // SessionStart hook into settings.json.
        let on_disk = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [ { "type": "command",
                      "command": "node /cfg/hooks/context-mode-cache-heal.mjs" } ] }
                ]
            },
            "enabledPlugins": { "context-mode@context-mode": true }
        });
        std::fs::write(&path, serde_json::to_vec(&on_disk).unwrap()).unwrap();

        // llmenv re-renders: its own hooks + authoritative enabledPlugins.
        let fresh = json!({
            "hooks": { "SessionStart": [
                { "hooks": [ { "type": "command", "command": "node /cfg/llmenv-own.mjs" } ] }
            ] },
            "enabledPlugins": { "context-mode@context-mode": true },
            "permissions": { "allow": [], "ask": [], "deny": [] }
        });

        let merged = reconcile_settings(&path, fresh).expect("reconcile_settings should succeed");
        let ss = merged["hooks"]["SessionStart"].as_array().unwrap();
        let commands: Vec<&str> = ss
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap())
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert!(
            commands
                .iter()
                .any(|c| c.contains("context-mode-cache-heal")),
            "self-registered cache-heal hook must survive"
        );
        assert!(
            commands.iter().any(|c| c.contains("llmenv-own")),
            "llmenv's own rendered hook must be present"
        );
        assert_eq!(
            merged["enabledPlugins"]["context-mode@context-mode"],
            json!(true)
        );
    }

    // ---- --engine flag baking ----

    #[test]
    fn hook_commands_carry_engine_flag() {
        // #502: every auto-emitted hook command must include `--engine claude_code`
        // so the invoked subcommand knows its caller engine.
        let manifest = crate::merge::MergedManifest::default();
        let settings = render_settings_for_test(&manifest);

        for cmd in hook_commands_for(&settings, "SessionStart") {
            if cmd.starts_with("llmenv ") {
                assert!(
                    cmd.contains("--engine claude_code"),
                    "SessionStart command missing --engine flag: {cmd:?}"
                );
            }
        }
        for cmd in hook_commands_for(&settings, "PreToolUse") {
            if cmd.starts_with("llmenv ") {
                assert!(
                    cmd.contains("--engine claude_code"),
                    "PreToolUse command missing --engine flag: {cmd:?}"
                );
            }
        }
    }

    #[test]
    fn stale_check_command_carries_engine_flag() {
        assert!(
            STALE_CHECK_COMMAND.contains("--engine claude_code"),
            "STALE_CHECK_COMMAND must carry --engine flag: {STALE_CHECK_COMMAND:?}"
        );
    }

    #[test]
    fn config_context_command_carries_engine_flag() {
        assert!(
            CONFIG_CONTEXT_COMMAND.contains("--engine claude_code"),
            "CONFIG_CONTEXT_COMMAND must carry --engine flag: {CONFIG_CONTEXT_COMMAND:?}"
        );
    }

    #[test]
    fn config_guard_command_carries_engine_flag() {
        assert!(
            CONFIG_GUARD_COMMAND.contains("--engine claude_code"),
            "CONFIG_GUARD_COMMAND must carry --engine flag: {CONFIG_GUARD_COMMAND:?}"
        );
    }

    #[test]
    fn hook_run_command_carries_engine_flag() {
        assert!(
            HOOK_RUN_COMMAND.contains("--engine claude_code"),
            "HOOK_RUN_COMMAND must carry --engine flag: {HOOK_RUN_COMMAND:?}"
        );
    }

    // ── First-class skills ────────────────────────────────────────────────────

    /// Scan a plugin directory for a `skills/` subdirectory and project each
    #[test]
    fn write_first_class_skills_copies_files_owner_only() {
        let src_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();

        // Build a minimal skill source dir.
        let skill_src = src_tmp.path().join("my-skill");
        std::fs::create_dir_all(skill_src.join("subdir")).unwrap();
        std::fs::write(skill_src.join("SKILL.md"), VALID_FRONTMATTER).unwrap();
        std::fs::write(skill_src.join("subdir/helper.sh"), "#!/bin/sh\necho hi\n").unwrap();

        let skill = crate::config::SkillSource {
            name: "my-skill".into(),
            path: skill_src.to_str().unwrap().into(),
            when: Vec::new(),
        };
        let owned = crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap();

        // Both files should land in out/skills/my-skill/
        let dest_md = out_tmp.path().join("skills/my-skill/SKILL.md");
        let dest_sh = out_tmp.path().join("skills/my-skill/subdir/helper.sh");
        assert!(dest_md.exists(), "SKILL.md not written");
        assert!(dest_sh.exists(), "subdir/helper.sh not written");
        // Owned paths are relative to out.
        assert!(owned.iter().any(|p| p.ends_with("skills/my-skill")));

        // Permissions should be owner-only (0o600 for files).
        use std::os::unix::fs::PermissionsExt;
        let mode_md = std::fs::metadata(&dest_md).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_md, 0o600, "SKILL.md should be 0o600, got {mode_md:o}");
    }

    #[test]
    fn write_first_class_skills_rejects_traversal_name() {
        let out_tmp = tempfile::tempdir().unwrap();
        let skill = crate::config::SkillSource {
            name: "../evil".into(),
            path: "/some/path".into(),
            when: Vec::new(),
        };
        let err = crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsafe skill name"), "got: {err}");
    }

    #[test]
    fn write_first_class_skills_rejects_control_character_name() {
        // #534: closes the gap a traversal-only check leaves for names that
        // contain no `..`/absolute-path component but are still unsafe as a
        // filesystem/JSON-key identifier.
        let out_tmp = tempfile::tempdir().unwrap();
        let skill = crate::config::SkillSource {
            name: "foo\0bar".into(),
            path: "/some/path".into(),
            when: Vec::new(),
        };
        let err = crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsafe skill name"), "got: {err}");
    }

    #[test]
    fn write_first_class_skills_empty_is_noop() {
        let out_tmp = tempfile::tempdir().unwrap();
        let owned = crate::adapter::skills::write_first_class_skills(out_tmp.path(), &[]).unwrap();
        assert!(owned.is_empty());
        assert!(!out_tmp.path().join("skills").exists());
    }

    // Biased generator: mixes absolute paths and embedded `..` components (both
    // unsafe) with plain relative segments, so enough unsafe cases surface without
    // relying on prop_assume to filter a mostly-safe ".*" generator to death.
    fn arb_unsafe_join_target() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-z0-9]{0,10}".prop_map(|s| format!("/{s}")),
            "[a-z0-9]{0,10}".prop_map(|s| format!("../{s}")),
            "[a-z0-9]{0,10}".prop_map(|s| format!("{s}/../evil")),
        ]
    }

    proptest! {
        #[test]
        fn prop_write_first_class_skills_rejects_unsafe_names(name in arb_unsafe_join_target()) {
            prop_assert!(
                llmenv_paths::is_unsafe_join_target(&name),
                "generator produced a name is_unsafe_join_target disagrees with: {name:?}"
            );
            let out_tmp = tempfile::tempdir().unwrap();
            let skill = crate::config::SkillSource {
                name,
                path: "/some/path".into(),
                when: Vec::new(),
            };
            let result = crate::adapter::skills::write_first_class_skills(
                out_tmp.path(),
                std::slice::from_ref(&skill),
            );
            prop_assert!(
                result.is_err(),
                "unsafe join target name {:?} must be rejected",
                skill.name
            );
        }
    }

    #[test]
    fn project_plugin_skills_copies_skill_from_plugin_dir() {
        let plugin_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();

        // Plugin has a skills/ subdir with one skill.
        let skill_src = plugin_tmp.path().join("skills/my-plugin-skill");
        std::fs::create_dir_all(&skill_src).unwrap();
        std::fs::write(skill_src.join("SKILL.md"), VALID_FRONTMATTER).unwrap();

        let owned =
            crate::adapter::skills::project_plugin_skills(plugin_tmp.path(), out_tmp.path())
                .unwrap();

        assert!(
            out_tmp
                .path()
                .join("skills/my-plugin-skill/SKILL.md")
                .exists(),
            "skill SKILL.md not projected"
        );
        assert!(
            owned.iter().any(|p| p.ends_with("skills/my-plugin-skill")),
            "owned missing skills dir"
        );
    }

    #[test]
    fn project_plugin_skills_no_skills_dir_returns_empty() {
        let plugin_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        // No skills/ subdir in the plugin.
        let owned =
            crate::adapter::skills::project_plugin_skills(plugin_tmp.path(), out_tmp.path())
                .unwrap();
        assert!(owned.is_empty());
    }

    fn external_plugin(marketplace: &str, plugin: &str, install_path: &str) -> ResolvedPlugin {
        ResolvedPlugin {
            marketplace: marketplace.to_string(),
            plugin: plugin.to_string(),
            collection: "test-collection".to_string(),
            install_path: Some(install_path.to_string()),
            git_commit_sha: Some("deadbeef".to_string()),
        }
    }

    #[test]
    fn generate_installed_plugins_json_errors_on_corrupt_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("installed_plugins.json"),
            "{not valid json",
        )
        .unwrap();

        let plugin = external_plugin("mp", "my-plugin", "/tmp/payload");
        let err = generate_installed_plugins_json(tmp.path(), &[&plugin]).unwrap_err();
        assert!(
            err.to_string().contains("not valid JSON"),
            "expected 'not valid JSON' in error, got: {err}"
        );
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "expected 'refusing to overwrite' in error, got: {err}"
        );
    }

    #[test]
    fn generate_installed_plugins_json_succeeds_on_absent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = external_plugin("mp", "my-plugin", "/tmp/payload");
        generate_installed_plugins_json(tmp.path(), &[&plugin]).unwrap();
        assert!(tmp.path().join("plugins/installed_plugins.json").exists());
    }

    proptest! {
        #[test]
        fn prop_generate_installed_plugins_json_merge_is_idempotent(
            names in prop::collection::vec("[a-z][a-z0-9-]{0,10}", 1..5),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let plugins: Vec<ResolvedPlugin> = names
                .iter()
                .map(|n| external_plugin("mp", n, "/tmp/payload"))
                .collect();
            let refs: Vec<&ResolvedPlugin> = plugins.iter().collect();

            generate_installed_plugins_json(tmp.path(), &refs).unwrap();
            let path = tmp.path().join("plugins/installed_plugins.json");
            let first = std::fs::read_to_string(&path).unwrap();

            generate_installed_plugins_json(tmp.path(), &refs).unwrap();
            let second = std::fs::read_to_string(&path).unwrap();

            prop_assert_eq!(
                first, second,
                "calling with the same plugin set twice must not duplicate entries or change output"
            );
        }
    }

    proptest! {
        // #739 roundtrip: writing a set of owned MCP server names then reading
        // back via read_owned_servers must produce an identical set.
        #[test]
        fn prop_read_owned_servers_roundtrip(
            names in prop::collection::btree_set(".{1,40}", 0..10),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(CLAUDE_JSON_OWNED_SERVERS_FILE);

            // Write the set as a JSON array (same serialization pattern used by
            // merge_mcp_into_claude_json).
            let json: Vec<&str> = names.iter().map(String::as_str).collect();
            std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();

            let got = read_owned_servers(&path);
            prop_assert_eq!(got, names, "read_owned_servers must roundtrip the written set");
        }

        // No panic on arbitrary byte content: any input to read_owned_servers
        // must return a BTreeSet (possibly empty) without panicking.
        #[test]
        fn prop_read_owned_servers_no_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..=512),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(CLAUDE_JSON_OWNED_SERVERS_FILE);
            std::fs::write(&path, &bytes).unwrap();
            let _ = read_owned_servers(&path);
            // Any panic would fail the test — the function must handle all inputs.
        }
    }

    #[test]
    fn read_owned_servers_absent_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        let got = read_owned_servers(&path);
        assert!(got.is_empty(), "absent file must return empty set");
    }

    #[test]
    fn read_owned_servers_malformed_json_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_OWNED_SERVERS_FILE);
        std::fs::write(&path, b"not valid json at all").unwrap();
        let got = read_owned_servers(&path);
        assert!(got.is_empty(), "malformed JSON must return empty set");
    }

    #[test]
    fn read_owned_servers_empty_array_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_OWNED_SERVERS_FILE);
        std::fs::write(&path, b"[]").unwrap();
        let got = read_owned_servers(&path);
        assert!(got.is_empty(), "empty JSON array must return empty set");
    }

    #[test]
    fn emit_hook_context_store_only_events_return_empty_string() {
        // Store-only events (SessionStart, SessionEnd) have no model turn to inject
        // context into. Should return empty per Claude Code schema (no additionalContext).
        let adapter = ClaudeCodeAdapter;
        assert_eq!(adapter.emit_hook_context("SessionEnd", "data"), "");
        assert_eq!(adapter.emit_hook_context("SessionStart", "data"), "");
    }

    #[test]
    fn emit_hook_context_injection_events_include_additional_context() {
        // Context-injection events (UserPromptSubmit, PostToolUse) should include
        // additionalContext per Claude Code schema.
        let adapter = ClaudeCodeAdapter;
        for event in ["UserPromptSubmit", "PostToolUse"] {
            let output = adapter.emit_hook_context(event, "context data");
            let parsed: serde_json::Value =
                serde_json::from_str(&output).expect("must be valid JSON");
            assert_eq!(
                parsed["hookSpecificOutput"]["hookEventName"].as_str(),
                Some(event)
            );
            assert!(
                parsed["hookSpecificOutput"]["additionalContext"]
                    .as_str()
                    .expect("must have additionalContext")
                    .contains("context data")
            );
        }
    }

    #[test]
    fn emit_hook_context_empty_text_returns_empty_string() {
        // Empty text should return empty string, not invalid JSON
        let adapter = ClaudeCodeAdapter;
        let output = adapter.emit_hook_context("SessionEnd", "");
        assert_eq!(output, "", "empty text should produce empty output");
    }

    #[test]
    fn model_providers_are_noop_for_claude_code_adapter() {
        // Plan self-review gap: ClaudeCodeAdapter must not emit model provider
        // config into settings.json — it only renders via CrushAdapter.
        let baseline = crate::merge::MergedManifest::default();
        let baseline_json = render_settings_for_test(&baseline);

        let with_providers = crate::merge::MergedManifest {
            capabilities: crate::config::Capabilities {
                model_providers: vec![crate::config::ModelProvider {
                    id: "test".into(),
                    base_url: Some("http://localhost:9999/v1".into()),
                    api_type: Some("openai".into()),
                    ..Default::default()
                }],
                default_models: std::iter::once((
                    "large".into(),
                    crate::config::ModelRef {
                        provider: "test".into(),
                        model: "test-model".into(),
                    },
                ))
                .collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        let with_providers_json = render_settings_for_test(&with_providers);

        assert_eq!(
            baseline_json, with_providers_json,
            "model_providers/default_models must not affect Claude Code settings.json output"
        );
    }
}
