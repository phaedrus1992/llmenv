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

/// Which engine-neutral lifecycle events get a `hook-run` registration for this
/// manifest, and why.
///
/// Exists so `llmenv doctor` reports what `generate_settings_json` actually
/// writes (#741) rather than re-deriving it from scratch.
///
/// `turn_start`'s gate is read straight from here by the generator.
/// `session_start`/`session_end` come from `BASELINE_HOOK_EVENTS`
/// unconditionally, and `stop` is still derived independently in the
/// session-log/task-tracker branch — folding that one in would mean
/// restructuring how the whole session-log event set is emitted.
///
/// `lifecycle_registrations_match_the_generated_settings` is what keeps the
/// independent gates honest: it renders settings for each combination and
/// asserts this function agrees. Session logging is on in a default manifest,
/// so fixtures that leave it that way can't tell the two halves of `stop`'s
/// condition apart — the cases that disable it are the ones that make the
/// assertion capable of failing.
pub(crate) fn lifecycle_hook_registrations(
    manifest: &MergedManifest,
) -> Vec<(&'static str, bool, &'static str)> {
    let icm_active = manifest.mcps.iter().any(|m| m.name == MEMORY_MCP_NAME);
    let session_log = manifest.session_log.any_sink_enabled();
    let task_tracker = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .is_some_and(|t| t.enabled);
    // #317: the self-critique layer runs on Stop, so enabling it has to be a
    // reason to register the event. Without this the layer is a phantom —
    // config accepts it, `doctor` would report it, and nothing ever fires.
    let slippage_critique = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.slippage.as_ref())
        .is_some_and(|s| s.enabled && s.self_critique);
    vec![
        ("session_start", true, "always registered"),
        ("session_end", true, "always registered"),
        (
            "turn_start",
            icm_active,
            "needs a memory backend (features.memory)",
        ),
        (
            "stop",
            session_log || task_tracker || slippage_critique,
            "needs session logging, features.task_tracker, or slippage self_critique",
        ),
    ]
}

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

/// #694/#946: Built-in ICM MCP server tool tiers, rendered by
/// `apply_mcp_tier_permissions` into one coherent allow/ask/deny policy (no
/// wildcard/tier conflict — see #946). Default policy: read-only and
/// mutation tools → allow, destructive → ask; overridable per feature via
/// `features.memory[].mcp_permissions`.
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
    "icm_memoir_export",
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
    "icm_memoir_refine",
    "icm_memoir_link",
];

const ICM_DESTRUCTIVE: &[&str] = &["icm_memory_forget", "icm_memory_forget_topic"];

/// #946: Built-in context-mode MCP plugin tool tiers (without the common
/// `CONTEXT_MODE_MCP_PREFIX`), rendered the same way as the ICM tiers above.
const CTX_READ_ONLY: &[&str] = &["ctx_search", "ctx_stats", "ctx_doctor", "ctx_insight"];

const CTX_MUTATION: &[&str] = &[
    "ctx_index",
    "ctx_execute",
    "ctx_execute_file",
    "ctx_fetch_and_index",
    "ctx_batch_execute",
];

const CTX_DESTRUCTIVE: &[&str] = &["ctx_purge", "ctx_upgrade"];

/// Claude Code's MCP tool-name prefix for the ICM memory server
/// (`mcp__<server>__<tool>`, server name = [`MEMORY_MCP_NAME`]).
const ICM_MCP_PREFIX: &str = "mcp__icm__";

/// #1323: Built-in codebase-memory-mcp server tool tiers, rendered the same
/// way as the ICM/context-mode tiers above. Source-verified against
/// `codebase-memory-mcp`'s own `TOOL_ANNOTATIONS` table (`src/mcp/mcp.c`),
/// which is a deliberately discriminating table, not a blanket MCP-spec
/// default: `index_repository` and `ingest_traces` are explicitly annotated
/// `destructive=false`, while `manage_adr` is explicitly annotated
/// `destructive=true, idempotent=false` — confirmed by its implementation,
/// an unversioned UPSERT (`cbm_store_adr_store`) that replaces the entire
/// ADR document with no history or backup, the one piece of human-authored
/// (not re-derivable-by-reindexing) state in the store. `manage_adr` is
/// therefore tiered `Destructive`, not `Mutation`, despite being a "write"
/// in the same broad sense as the other two.
///
/// `delete_project` is also destructive (irreversibly removes a project's
/// index). The `ask` boundary alone doesn't cover everything destructive
/// here: `index_repository`'s `name` override can replace a *different*
/// project's index, and re-tiering `index_repository` to `ask` would defeat
/// the auto-index-on-`SessionStart` use case this tiering exists for. That
/// case is handled a layer down instead, by the `PreToolUse` deny in
/// [`crate::hook_run::cbm_index_guard`] (#1331).
const CBM_READ_ONLY: &[&str] = &[
    "search_graph",
    "query_graph",
    "trace_path",
    "get_code_snippet",
    "get_graph_schema",
    "get_architecture",
    "search_code",
    "list_projects",
    "index_status",
    "check_index_coverage",
    "detect_changes",
];

const CBM_MUTATION: &[&str] = &["index_repository", "ingest_traces"];

const CBM_DESTRUCTIVE: &[&str] = &["delete_project", "manage_adr"];

/// Claude Code's MCP tool-name prefix for the codebase-memory-mcp server
/// (`mcp__<server>__<tool>`, server name =
/// [`crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME`]).
const CBM_MCP_PREFIX: &str = "mcp__codebase-memory-mcp__";

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

    fn is_active(&self) -> bool {
        std::env::var("CLAUDE_CONFIG_DIR").is_ok()
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

    fn supports_output_styles(&self) -> bool {
        true
    }

    /// Every map this adapter reads — `native_model_providers` is absent
    /// because Claude Code has no provider block to merge into.
    fn native_maps(&self) -> &'static [&'static str] {
        use crate::adapter::native_keys as nk;
        &[
            nk::NATIVE_PERMISSIONS,
            nk::NATIVE_HOOKS,
            nk::NATIVE_PLUGINS,
            nk::NATIVE_MCP,
            nk::NATIVE,
        ]
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

        // Temp dir: CLAUDE_CODE_TMPDIR + the standard POSIX temp vars, so
        // subprocesses scratch inside llmenv's tree rather than the shared
        // /tmp. Claude Code appends /claude-{uid}/ to the value on Unix.
        //
        // Lives in the durable state dir, not the per-hash cache folder (#1379).
        // It used to be `cache_dir.join("tmp")`, which put it inside a directory
        // that both `llmenv prune` and an ordinary config edit are allowed to
        // delete — pruning a stale generation, or minting a new shape hash by
        // editing config.yaml, left every already-running shell with TMPDIR
        // pointing at nothing. The breakage was silent until something needed a
        // temp file, and then surfaced as someone else's error: git, for one,
        // fails an SSH-signed commit with "could not create temporary file:
        // No such file or directory" / "failed to write commit object", which
        // names neither llmenv nor TMPDIR. The state dir is never a prune
        // candidate under any mode (#175), same as `plugins_dir` below, so the
        // path stays valid for the life of the shell. Temp files have no reason
        // to be shape-scoped: `mktemp` already keeps concurrent users apart by
        // generating unique names.
        let tmp_dir = state_dir.join("tmp");
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

        // #1262: skip the file entirely when nothing resolved, rather than
        // leaving a 0-byte CLAUDE.md. Staying out of `owned` also means a copy
        // written by an earlier render is reconciled away as a ghost.
        if !claude_md_content.trim().is_empty() {
            crate::paths::write_owner_only(&out.join("CLAUDE.md"), claude_md_content.as_bytes())?;
            owned.push(PathBuf::from("CLAUDE.md"));
        }

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
        //
        // `write_file_through_dirs`, not a path-based `create_dir_all` +
        // `write_owner_only`/`copy_replacing_symlink`: `out` is the agent's
        // live config dir and persists across renders, so a prior render's
        // output could have been replaced by a symlink anywhere along
        // `rel` — a directory component included, which `create_dir_all`
        // would follow — between calls (#1341-class TOCTOU, #1422/#1427).
        for (rel, abs) in &manifest.files {
            if crate::paths::is_unsafe_join_target(rel.to_string_lossy().as_ref()) {
                anyhow::bail!("path traversal in bundle file: {}", rel.display());
            }
            if is_hook_json(rel) {
                let raw = std::fs::read_to_string(abs)?;
                let rendered = raw.replace("{{ICM_MCP}}", MEMORY_MCP_NAME);
                // 0o600, matching `write_owner_only`'s own contract for this
                // branch: this is llmenv's rendered content, not a pass-through
                // of `abs`'s original mode.
                crate::paths::dirfd::write_file_through_dirs(
                    out,
                    rel,
                    rendered.as_bytes(),
                    rustix::fs::Mode::from(0o600),
                )?;
            } else {
                let bytes =
                    std::fs::read(abs).with_context(|| format!("reading {}", abs.display()))?;
                let mode = crate::materialize::bundle_file_mode(abs)?;
                crate::paths::dirfd::write_file_through_dirs(out, rel, &bytes, mode)?;
            }
            owned.push(rel.clone());
        }

        // Write first-class skills (declared via `capabilities.skills`) before
        // validating, so `validate_skills` covers them along with plugin-sourced ones.
        let skill_owned =
            crate::adapter::skills::write_first_class_skills(out, &manifest.capabilities.skills)?;
        owned.extend(skill_owned);

        // Built-in `llmenv` skill: one reference file per enabled first-party
        // feature (task tracker, memory, context-mode, codebase-memory),
        // replacing the old task-tracker CLAUDE.md fragment. No-op when none
        // are enabled.
        let features = manifest.capabilities.features.clone().unwrap_or_default();
        owned.extend(crate::adapter::llmenv_skill::materialize_llmenv_skill(
            out, &features,
        )?);

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

        // #1130: output styles render natively for Claude Code. Written before
        // validate_skills purely for consistency with the other capability
        // writes above — output-styles/ is a sibling of skills/, not covered
        // by that scan. The `outputStyle` settings key is set independently
        // inside generate_settings_json below, recomputed from the same
        // manifest rather than threaded through as extra state.
        owned.extend(crate::adapter::output_styles::write_native_output_styles(
            out,
            &manifest.capabilities.output_styles,
        )?);

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
        super::emit_hook_context(hook_event_name, text)
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

/// Top-level settings.json keys that a modeled capability renders and that the
/// catch-all still cannot carry. The top-level `native` catch-all (D3) is for
/// keys NO modeled feature owns; a key here would be overlaid last by the blind
/// deep-merge and clobber the rendered output.
///
/// `permissions` used to be in this list. It was removed in #750: it now goes
/// through [`merge_native_permissions`], which appends instead of replacing, so
/// the catch-all can carry Claude-Code-only permission keys llmenv doesn't
/// model without a neutral-schema change. `hooks` stays because it is an array
/// of matcher groups — "additive" has no unambiguous meaning there the way it
/// does for flat allow/ask/deny lists.
///
/// `enabledPlugins`/`extraKnownMarketplaces` (plugins) and the separate
/// `mcp.json` doc use distinct keys and aren't catch-all collisions.
const MODELED_SETTINGS_KEYS: [&str; 1] = ["hooks"];

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
                 `{key}`, which would silently clobber the rendered `{key}`. \
                 Move it to the `native_{key}` sibling instead, which merges in \
                 the safe direction."
            );
        }
    }
    Ok(())
}

/// Layer a `native.claude_code.permissions` fragment over the rendered
/// `permissions` object (#750), and return the fragment with that key removed
/// so the generic catch-all overlay never sees it.
///
/// The catch-all rejected `permissions` outright before this, because
/// [`overlay_native`] is a blind deep-merge: a native `allow`/`ask`/`deny`
/// would *replace* the rendered array, and an omitted or empty `deny` would
/// erase it. That is a security regression — the rendered `deny` is the output
/// of tier policy and neutral rules, and nothing layered on top may weaken it.
///
/// So the arrays merge the way `native_permissions.claude_code` already does:
/// append, dedupe, then re-apply deny > ask > allow authority. A native rule
/// can only ever *add* a restriction or add an allowance that isn't already
/// denied. Every other key — `defaultMode`, `additionalDirectories`,
/// `disableBypassPermissionsMode`, and whatever Claude Code ships next —
/// overwrites, which is the escape-hatch behaviour the issue asks for: those
/// keys carry no rendered security decision to weaken.
fn merge_native_permissions(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    fragment: Option<&serde_yaml::Value>,
) -> anyhow::Result<Option<serde_yaml::Value>> {
    let Some(fragment) = fragment else {
        return Ok(None);
    };
    let Some(map) = fragment.as_mapping() else {
        return Ok(Some(fragment.clone()));
    };
    let perms_key = serde_yaml::Value::String("permissions".into());
    let Some(native_perms) = map.get(&perms_key) else {
        return Ok(Some(fragment.clone()));
    };

    let native_json: serde_json::Value = serde_json::to_value(native_perms)
        .context("converting native.claude_code.permissions to JSON")?;
    let native_obj = native_json.as_object().cloned().unwrap_or_default();

    let mut rendered = settings
        .get("permissions")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();

    // `defaultMode` is modeled (#748 put it in the neutral vocabulary as
    // `capabilities.permissions.default_mode`), and `bypassPermissions` turns
    // the permission system off outright. Letting the catch-all set it would
    // give anything that can author a `native:` block — bundles included — a
    // one-line escalation past every rendered `ask` and `deny`, which is
    // exactly what the old hard error prevented. So it keeps a hard error of
    // its own, pointing at the modeled field. The catch-all stays the escape
    // hatch for keys llmenv genuinely doesn't model.
    if native_obj.contains_key("defaultMode") {
        anyhow::bail!(
            "`native.claude_code.permissions.defaultMode` is not accepted: \
             `defaultMode` is a modeled key, and setting it here would override \
             the rendered permission mode (including to `bypassPermissions`, \
             which disables the permission system). Use \
             `capabilities.permissions.default_mode` instead."
        );
    }

    // Arrays append; everything else overwrites.
    for (key, value) in native_obj {
        if matches!(key.as_str(), "allow" | "ask" | "deny") {
            let mut merged: Vec<String> = rendered
                .get(&key)
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            // #888: normalize here too. The `native_permissions` sibling maps
            // the renamed `Write` tool onto `Edit`; without the same pass a
            // catch-all `Write(...)` rule renders verbatim and matches nothing,
            // which for a `deny` reads as protection that isn't there.
            merged.extend(
                value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(normalize_deprecated_tool)),
            );
            dedup(&mut merged);
            rendered.insert(key, json!(merged));
        } else {
            rendered.insert(key, value);
        }
    }

    apply_permission_authority(&mut rendered);
    settings.insert("permissions".into(), serde_json::Value::Object(rendered));

    // Hand back the fragment without `permissions`, so the caller's generic
    // overlay can't re-apply it as a blind deep-merge afterwards.
    let mut rest = map.clone();
    rest.remove(&perms_key);
    Ok(Some(serde_yaml::Value::Mapping(rest)))
}

/// Enforce deny > ask > allow on a rendered `permissions` object: a rule that
/// appears in a higher-authority bucket is removed from the lower ones.
///
/// Shared by the renderer and [`merge_native_permissions`] so a native fragment
/// can't produce a combination the renderer would never emit — e.g. a rule
/// sitting in both `deny` and `allow`.
fn apply_permission_authority(perms: &mut serde_json::Map<String, serde_json::Value>) {
    let strings = |perms: &serde_json::Map<String, serde_json::Value>, key: &str| {
        perms
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default()
    };
    let deny = strings(perms, "deny");
    let retain = |perms: &mut serde_json::Map<String, serde_json::Value>,
                  key: &str,
                  drop: &std::collections::BTreeSet<String>| {
        if let Some(arr) = perms.get_mut(key).and_then(serde_json::Value::as_array_mut) {
            arr.retain(|v| v.as_str().is_none_or(|s| !drop.contains(s)));
        }
    };
    retain(perms, "ask", &deny);
    retain(perms, "allow", &deny);
    let ask = strings(perms, "ask");
    retain(perms, "allow", &ask);
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
    // #1270 follow-up: record, per server, which top-level keys the native
    // overlay just nulled — the preserve-subkeys loop below needs this,
    // because stripping the null (next line) makes a just-deleted key and a
    // key llmenv simply never rendered look identical (both absent).
    // Without it, a credential-purge null on `env`/`headers` would be
    // "filled back in" from the stale on-disk copy instead of staying gone.
    let nulled_keys: std::collections::HashMap<String, std::collections::HashSet<String>> = doc
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|servers| {
            servers
                .iter()
                .filter_map(|(name, entry)| {
                    let nulls: std::collections::HashSet<String> = entry
                        .as_object()?
                        .iter()
                        .filter(|(_, v)| v.is_null())
                        .map(|(k, _)| k.clone())
                        .collect();
                    (!nulls.is_empty()).then(|| (name.clone(), nulls))
                })
                .collect()
        })
        .unwrap_or_default();
    // #1270: a native null on a key already rendered into a server entry must
    // delete the key rather than persist an explicit JSON null into the real,
    // persistent `.claude.json`. `doc` is a scratch value scoped to llmenv's
    // own server set, so stripping it here doesn't touch any foreign key.
    super::strip_json_nulls(&mut doc);
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

    // Collect current server names before consuming `llmenv_servers` in the
    // loop below — the companion file write needs them afterward.
    let current_names: Vec<String> = llmenv_servers.keys().cloned().collect();

    let servers_val = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    match servers_val.as_object_mut() {
        Some(servers_obj) => {
            // Prune previously-owned servers no longer in the current set.
            for stale_name in &previously_owned {
                if !current_names.contains(stale_name) {
                    servers_obj.remove(stale_name.as_str());
                }
            }
            // Upsert current servers, preserving runtime-added sub-keys.
            for (name, mut entry) in llmenv_servers {
                // Preserve runtime-added sub-keys (e.g., auth tokens) from the
                // previous session's .claude.json so they survive re-materialization
                // in Loose/Normal mode where the same file is reused.
                if let Some(existing) = servers_obj.get(&name).and_then(|v| v.as_object())
                    && let Some(ref mut new_obj) = entry.as_object_mut()
                {
                    let explicitly_nulled = nulled_keys.get(&name);
                    for (k, v) in existing.iter() {
                        let was_explicitly_deleted =
                            explicitly_nulled.is_some_and(|nulled| nulled.contains(k));
                        if !new_obj.contains_key(k) && !was_explicitly_deleted {
                            new_obj.insert(k.clone(), v.clone());
                        }
                    }
                }
                servers_obj.insert(name, entry);
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
    // #1066: the walk descends by file descriptor, not by path. `src` is
    // resolved exactly once, here; every step below opens an entry of a
    // directory this process already holds open, so there is no window in
    // which a directory can be swapped for a symlink between the check and the
    // use. The `src` paths further down are carried for error messages only
    // and are never handed back to the kernel.
    use std::os::fd::AsFd as _;

    let dir = crate::paths::dirfd::open_dir_nofollow(src)
        .with_context(|| format!("opening source directory '{}'", src.display()))?;
    copy_dir_owner_only_inner(dir.as_fd(), src, dest, true)
}

fn copy_dir_owner_only_inner(
    src_dir: std::os::fd::BorrowedFd<'_>,
    src: &Path,
    dest: &Path,
    is_root: bool,
) -> anyhow::Result<Vec<PathBuf>> {
    // #1341: a symlink already at `dest` (planted, or left behind by a prior
    // run) is never legitimate — llmenv owns everything under the
    // materialized output tree. Reject before create_dir_owner_only, which
    // would otherwise follow it (chmod-ing and writing through the target).
    // A stat error other than "not found" fails closed (propagated) rather
    // than being treated as "nothing there, proceed" — the checked path
    // being currently unreadable is not the same as it being absent
    // (security-audit, #1341).
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!(
                "destination path '{}' is a symlink — a path llmenv owns and writes through \
                 must never be a symlink",
                dest.display()
            );
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("checking destination path '{}'", dest.display()));
        }
    }
    use std::os::fd::AsFd as _;

    let mut written: Vec<PathBuf> = Vec::new();
    create_dir_owner_only(dest)?;
    for entry in crate::paths::dirfd::read_dir_entries(src_dir)? {
        let file_name = entry.name.clone();
        let src_path = src.join(&file_name);
        if entry.is_symlink() {
            // #1341: a symlinked SKILL.md at the *skill's own root* is
            // silently dropped by the skip below, producing a skill
            // directory that only fails validation later with a misleading
            // "missing SKILL.md" — the file *is* there in the source, it
            // just never got copied. Fail loud here instead, where the real
            // cause is still in scope. Root-only and case-insensitive
            // (security-audit, #1341): a nested `SKILL.md` (a vendored
            // sub-skill, an example file) isn't the manifest
            // `validate_skills` checks, and macOS's default
            // case-insensitive filesystem would otherwise let `skill.md`
            // take the silent-skip branch and still satisfy
            // `skill_md.exists()`.
            if is_root && file_name.eq_ignore_ascii_case("SKILL.md") {
                anyhow::bail!(
                    "'{}' is a symlink — a skill's SKILL.md must be a real file, not a \
                     symlink",
                    src_path.display()
                );
            }
            // Any other symlinked entry (reference file, helper script) is
            // skipped, not fatal — no TOCTOU-safe way to follow it into a
            // bounded dir. Raised from debug to warn (#1341): silently
            // dropping a referenced file previously left no trace at any
            // default log level, and the skill still validated as if the
            // reference existed.
            tracing::warn!(path = %src_path.display(), "copy_dir_owner_only: skipping symlink");
            continue;
        }
        let dest_path = dest.join(&file_name);
        if entry.is_dir() {
            let child = crate::paths::dirfd::open_dir_at(src_dir, &file_name)
                .with_context(|| format!("opening source directory '{}'", src_path.display()))?;
            let sub_written =
                copy_dir_owner_only_inner(child.as_fd(), &src_path, &dest_path, false)?;
            written.extend(sub_written);
        } else if entry.is_file() {
            // #1341: a symlink already at `dest_path` is never legitimate —
            // `write_owner_only` opens with `create(true).truncate(true)`,
            // which follows a symlink and overwrites its target.
            if let Ok(dest_meta) = std::fs::symlink_metadata(&dest_path)
                && dest_meta.file_type().is_symlink()
            {
                anyhow::bail!(
                    "destination path '{}' is a symlink — a path llmenv owns and writes \
                     through must never be a symlink",
                    dest_path.display()
                );
            }
            let content = crate::paths::dirfd::read_file_at(src_dir, &file_name)
                .with_context(|| format!("reading '{}'", src_path.display()))?;
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
        // state that happens to lack the null key (#720 / the #699 no-null
        // invariant).
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

    // #121/#85: the SessionStart drift check used to be registered here as its
    // own hook. #741 folded it into `hook-run session_start` (registered below
    // via BASELINE_HOOK_EVENTS), so session start spawns one `llmenv` process
    // instead of two that each re-parsed the config. `llmenv check-stale`
    // remains as a command users can run directly.

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

    // #317: the metrics layer counts on PostToolUse, which only session
    // logging registers today. Without this the counter never runs and the
    // session-end summary is always empty.
    if manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.slippage.as_ref())
        .is_some_and(|s| s.enabled && s.metrics)
        && !manifest.session_log.any_sink_enabled()
    {
        hooks_by_event
            .entry("PostToolUse".to_string())
            .or_default()
            .push(json!({
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} post_tool_use") }],
            }));
    }

    // #317 phase 3: the transcript-scan layers judge Bash commands, so Bash
    // has to reach hook-run. Nothing else routes it there.
    if manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.slippage.as_ref())
        .is_some_and(|s| s.enabled && (s.answer_before_act || s.explain_before_act))
    {
        hooks_by_event
            .entry("PreToolUse".to_string())
            .or_default()
            .push(json!({
                "matcher": "^Bash$",
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} pre_tool_use") }],
            }));
    }

    // #317: the write guard needs to see `Write`. `Read` already reaches
    // hook-run via the read-once registration above, but nothing routes
    // `Write` there — the existing `^(Write|Edit|MultiEdit)$` entry runs the
    // config guard, a different command. Without this the layer would record
    // reads and never act on them.
    if manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.slippage.as_ref())
        .is_some_and(|s| s.enabled && s.read_before_edit)
    {
        hooks_by_event
            .entry("PreToolUse".to_string())
            .or_default()
            .push(json!({
                "matcher": "^Write$",
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} pre_tool_use") }],
            }));
    }

    // #1331: block `index_repository`'s project-name override, which can
    // overwrite an unrelated project's index (see `hook_run::cbm_index_guard`).
    // Registered only when codebase-memory-mcp is actually wired — unlike
    // read_once above, whose tool exists in every session regardless of config.
    if manifest
        .mcps
        .iter()
        .any(|m| m.name == crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME)
    {
        hooks_by_event
            .entry("PreToolUse".to_string())
            .or_default()
            .push(json!({
                "matcher": format!("^{}$", crate::hook_run::cbm_index_guard::INDEX_REPOSITORY_TOOL),
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} pre_tool_use") }],
            }));
    }

    // #985: redirect Claude Code's built-in task tools to the `llmenv task`
    // tracker so tasks actually land there instead of Claude's ephemeral state.
    // Only registered when the task tracker is enabled; the `pre_tool_use`
    // handler dispatches on tool_name (TaskCreate/TaskList/TaskUpdate) and denies
    // the native tool with the equivalent `llmenv task` result. (Root-level
    // `features.task_tracker` reaches this gate because build_manifest folds
    // root features into the manifest — see fold_root_features, #987.)
    //
    // #980: `block_engine_task_tools` (default true) is the opt-out — a user
    // who wants the tracker's CLAUDE.md fragment and reminders but still wants
    // Claude's native Task tools available (e.g. for genuine multi-agent
    // teammate coordination) can flip it off without disabling the tracker.
    if manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .is_some_and(|t| t.enabled && t.block_engine_task_tools)
    {
        hooks_by_event
            .entry("PreToolUse".to_string())
            .or_default()
            .push(json!({
                "matcher": "^(TaskCreate|TaskList|TaskUpdate)$",
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} pre_tool_use") }],
            }));
    }

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

    // #317: the rules digest runs on UserPromptSubmit, so enabling the layer
    // has to register the event — the memory-recall gate below and session
    // logging are the only other things that do, and neither is implied by
    // turning slippage on.
    let slippage_reinjection = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.slippage.as_ref())
        .is_some_and(|s| s.enabled && s.rule_reinjection);
    if slippage_reinjection
        && !manifest.session_log.any_sink_enabled()
        && !lifecycle_hook_registrations(manifest)
            .iter()
            .any(|(event, registered, _)| *event == "turn_start" && *registered)
    {
        hooks_by_event
            .entry("UserPromptSubmit".to_string())
            .or_default()
            .push(json!({
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} user_prompt_submit") }],
            }));
    }

    // #499: continuous per-prompt memory recall. Gated on icm_active (unlike the
    // always-on baseline events above) because this runs on every prompt, not
    // just session start/end — an unconditional per-turn network-backed hook
    // would add latency for every scope, including ones with no memory backend
    // configured at all.
    // Gate read from `lifecycle_hook_registrations` so `doctor` reports exactly
    // what gets written here rather than re-deriving the condition (#741).
    if lifecycle_hook_registrations(manifest)
        .iter()
        .any(|(event, registered, _)| *event == "turn_start" && *registered)
    {
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
    } else if lifecycle_hook_registrations(manifest)
        .iter()
        .any(|(event, registered, _)| *event == "stop" && *registered)
    {
        // #231: the task tracker's Stop reminder needs its own hook
        // registration — it must not depend on session logging happening to
        // be on, and #317's self-critique layer needs the same. Only registers
        // `Stop` (not the other session-log events), and only in the `else`
        // branch above so a session-log-enabled setup doesn't get two hook-run
        // invocations on the same event. Reads the shared gate rather than
        // re-deriving the condition, so the two can't drift (#741).
        hooks_by_event
            .entry("Stop".to_string())
            .or_default()
            .push(json!({
                "hooks": [{ "type": "command", "command": format!("{HOOK_RUN_COMMAND} stop") }],
            }));
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
    // #977: dedup the fresh doc itself. reconcile_settings only dedups when a
    // prior settings.json exists (it returns `fresh` verbatim otherwise), so a
    // first/strict render — or a native_hooks overlay that repeats a typed hook
    // — would otherwise write each hook entry twice and run every guard twice
    // per event. Same (strip-nulls, then per-event dedup) invariant reconcile
    // applies, so both paths converge on one entry per (event, matcher, command).
    dedup_hooks_doc(&mut hooks_value);
    settings.insert("hooks".into(), hooks_value);

    // #34: Render neutral permission rules into Claude's string grammar
    // (`Tool(pattern)` / `Tool(path)` / bare `Tool`), then append the per-engine
    // `native.claude_code` rule strings verbatim (aside from the #888 `Write` ->
    // `Edit` rewrite below) into the same allow/ask/deny arrays. Native rules are
    // not a separate object — Claude Code reads one flat array per action (see
    // docs/reference/claude-code/permissions.md). They share the array because
    // both are just permission rule strings; the only difference is neutral
    // rules are generated and native ones are authored.
    let perms = &manifest.capabilities.permissions;
    let native = manifest.capabilities.native_permissions.get("claude_code");

    // #888: normalize native rule strings up front so a user (or bundle)
    // authoring the deprecated `Write(...)` form directly — following stale
    // docs/examples rather than the neutral schema — gets the same rewrite as
    // neutral rules, before suppression comparisons and before landing in
    // settings.json.
    let normalize_native = |select: fn(&crate::config::NativePermissionRules) -> &[String]| {
        native.map_or_else(Vec::new, |n| {
            select(n)
                .iter()
                .map(|s| normalize_deprecated_tool(s))
                .collect()
        })
    };
    let native_allow: Vec<String> = normalize_native(|n| &n.allow);
    let native_ask_rules: Vec<String> = normalize_native(|n| &n.ask);
    let native_deny_rules: Vec<String> = normalize_native(|n| &n.deny);

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
    let native_ask: std::collections::BTreeSet<&str> =
        native_ask_rules.iter().map(String::as_str).collect();
    let native_deny: std::collections::BTreeSet<&str> =
        native_deny_rules.iter().map(String::as_str).collect();

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

    let mut allow = render_action(&perms.allow, &native_allow, PermissionAction::Allow);
    let mut ask = render_action(&perms.ask, &native_ask_rules, PermissionAction::Ask);
    let mut deny = render_action(&perms.deny, &native_deny_rules, PermissionAction::Deny);

    // #946: feature-enabled MCP tool tiers render into exactly one action per
    // tool (allow/ask/deny), replacing #490's wildcard allow. The wildcard used
    // to coexist with these same tier-based ask/deny entries; because Claude
    // Code resolves deny > ask > allow (specific beats wildcard), the wildcard
    // was silently shadowed — mutation tools prompted every call and
    // destructive tools were blocked outright, even though the feature was
    // enabled. Default policy: read-only/mutation -> allow, destructive -> ask;
    // overridable per feature via `mcp_permissions`.
    // Hoisted once and shared by both tier-permission call sites below (NLL
    // ends this borrow at the last `apply_mcp_tier_permissions` call, so the
    // `dedup(&mut allow/ask/deny)` calls after still borrow independently).
    let mut buckets = PermBuckets {
        allow: &mut allow,
        ask: &mut ask,
        deny: &mut deny,
    };
    // #972: same native deny/ask lookups the neutral-rule suppression above
    // uses, reused here so a tiered MCP tool already covered by a more
    // authoritative native rule doesn't also get a competing tier entry.
    let native_cover = NativeCover {
        deny: &native_deny,
        ask: &native_ask,
    };

    if manifest.plugins.iter().any(|p| {
        p.marketplace == crate::config::CONTEXT_MODE_MARKETPLACE
            && p.plugin == crate::config::CONTEXT_MODE_PLUGIN
    }) {
        let overrides = manifest
            .capabilities
            .features
            .as_ref()
            .and_then(|f| f.context_mode.as_ref())
            .and_then(|c| c.mcp_permissions.as_ref());
        apply_mcp_tier_permissions(
            &mut buckets,
            crate::config::CONTEXT_MODE_MCP_PREFIX,
            [
                (CTX_READ_ONLY, McpTier::ReadOnly),
                (CTX_MUTATION, McpTier::Mutation),
                (CTX_DESTRUCTIVE, McpTier::Destructive),
            ],
            overrides,
            &native_cover,
        );
    }

    if let Some(icm) = manifest.mcps.iter().find(|m| m.name == MEMORY_MCP_NAME) {
        apply_mcp_tier_permissions(
            &mut buckets,
            ICM_MCP_PREFIX,
            [
                (ICM_READ_ONLY, McpTier::ReadOnly),
                (ICM_MUTATION, McpTier::Mutation),
                (ICM_DESTRUCTIVE, McpTier::Destructive),
            ],
            icm.mcp_permissions.as_ref(),
            &native_cover,
        );
    }

    if let Some(cbm) = manifest
        .mcps
        .iter()
        .find(|m| m.name == crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME)
    {
        apply_mcp_tier_permissions(
            &mut buckets,
            CBM_MCP_PREFIX,
            [
                (CBM_READ_ONLY, McpTier::ReadOnly),
                (CBM_MUTATION, McpTier::Mutation),
                (CBM_DESTRUCTIVE, McpTier::Destructive),
            ],
            cbm.mcp_permissions.as_ref(),
            &native_cover,
        );
    }

    dedup(&mut allow);
    dedup(&mut ask);
    dedup(&mut deny);

    // #1322: the suppression above only resolves neutral-vs-*native*
    // conflicts (a native deny/ask outranking a *different* neutral rule of
    // the same rendered string). It never cross-checks the neutral buckets
    // against each other, so a `PermissionRule` authored directly in both
    // `permissions.allow` and `permissions.deny` (or `ask`/`deny`) — no
    // native rule involved at all — lands in both. Same deny > ask > allow
    // authority as above, applied as a final pass so every source that can
    // populate these three buckets (neutral rules, native rules, MCP tiers)
    // is covered uniformly, matching what
    // `generate_settings_json_permission_buckets_never_overlap` asserts.
    let deny_set: std::collections::BTreeSet<&str> = deny.iter().map(String::as_str).collect();
    ask.retain(|s| !deny_set.contains(s.as_str()));
    allow.retain(|s| !deny_set.contains(s.as_str()));
    let ask_set: std::collections::BTreeSet<&str> = ask.iter().map(String::as_str).collect();
    allow.retain(|s| !ask_set.contains(s.as_str()));

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

    // #1130: select an output style when exactly one non-`force_for_plugin`
    // entry is tag-active. See `output_styles::write_native_output_styles`
    // for why zero/multiple selectable styles leaves this key unset rather
    // than erroring.
    let selectable_styles: Vec<&str> = manifest
        .capabilities
        .output_styles
        .iter()
        .filter(|o| !o.force_for_plugin)
        .map(|o| o.name.as_str())
        .collect();
    if let [name] = selectable_styles[..] {
        settings.insert("outputStyle".into(), json!(name));
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
    // This overlay runs *before* the catch-all guard below, and `merge_json` is
    // a blind deep-merge, so an unguarded fragment here could clobber the
    // security-rendered output the catch-all is so careful about — a scalar
    // `permissions.deny` would replace the rendered array outright, and a
    // scalar fragment would replace the whole settings object (taking the
    // object-ness the permissions merge depends on with it). Hold it to the
    // same rules.
    if let Some(plugins) = manifest.capabilities.native_plugins.get("claude_code") {
        anyhow::ensure!(
            plugins.is_mapping(),
            "`native_plugins.claude_code` must be a mapping of settings keys, \
             not a scalar or sequence"
        );
        reject_modeled_keys_in_catch_all(plugins)?;
        if plugins
            .as_mapping()
            .is_some_and(|m| m.contains_key(serde_yaml::Value::String("permissions".into())))
        {
            anyhow::bail!(
                "`native_plugins.claude_code` carries a `permissions` key, which \
                 would overwrite the rendered permissions rather than layering \
                 additively. Put it under `native.claude_code.permissions` \
                 (additive) or `native_permissions.claude_code` instead."
            );
        }
    }
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
    // #750: `permissions` is pulled out of the fragment and merged additively
    // before the generic overlay runs, so the blind deep-merge below never sees
    // it. `rest` is the fragment minus that key.
    let rest = if let Some(obj) = settings_value.as_object_mut() {
        merge_native_permissions(obj, manifest.native.get("claude_code"))?
    } else {
        manifest.native.get("claude_code").cloned()
    };
    overlay_native(&mut settings_value, rest.as_ref())?;

    // #1264: a native `null` deletes the key rather than emitting an explicit
    // JSON null — `merge_json`'s shared-key overwrite arm deliberately does not
    // null-strip, so a null on a key the renderer already emitted survived to
    // here. Runs after the last overlay so it catches every layer.
    super::strip_json_nulls(&mut settings_value);

    let settings_path = out.join("settings.json");

    // #991: the hooks llmenv is rendering this round, captured before reconcile
    // consumes `settings_value`. Persisted to a sidecar so the *next* reconcile
    // can tell an llmenv-owned hook that was dropped from config (must be purged)
    // from a genuinely-foreign hook a plugin self-registered (must be kept).
    let rendered_hooks = settings_value.get("hooks").cloned();
    let hooks_sidecar = out.join(HOOKS_SIDECAR_FILE);
    let prev_owned_hooks = read_hooks_sidecar(&hooks_sidecar);

    // #196/#175: in version mode `out` is the agent's live config dir for the
    // whole session, so a plugin may have self-registered hooks (or other keys)
    // into settings.json after llmenv last wrote it. A wholesale overwrite would
    // strand that registration. Reconcile instead: preserve any foreign keys
    // already on disk, while making llmenv authoritative over the keys it owns.
    // In strict mode the file never pre-exists (fresh content-hashed folder), so
    // this is a no-op there.
    let reconciled = reconcile_settings(&settings_path, settings_value, prev_owned_hooks.as_ref())?;
    let json_str = serde_json::to_string_pretty(&reconciled)?;

    crate::paths::write_owner_only_atomic(&settings_path, json_str.as_bytes()).with_context(
        || {
            format!(
                "Failed to write settings.json at {}",
                settings_path.display()
            )
        },
    )?;

    // Record what llmenv rendered this round for the next reconcile's owned-vs-
    // foreign diff. Best-effort: a missing/failed sidecar just degrades reconcile
    // to its prior union-only behavior (foreign preserved, stale llmenv hooks
    // linger) — never fail the render over it.
    if let Some(hooks) = rendered_hooks
        && let Ok(bytes) = serde_json::to_vec(&hooks)
        && let Err(e) = crate::paths::write_owner_only_atomic(&hooks_sidecar, &bytes)
    {
        tracing::debug!(error = %e, path = %hooks_sidecar.display(), "failed to write hooks sidecar");
    }

    Ok(())
}

/// Sidecar recording the `hooks` object llmenv rendered, for the owned-vs-foreign
/// diff in [`reconcile_settings`] (#991). Dotfile so it stays out of the way.
const HOOKS_SIDECAR_FILE: &str = ".llmenv-hooks.json";

/// Read the previously-rendered hooks sidecar. Returns `None` when absent or
/// unparseable — reconcile then falls back to union-only behavior.
fn read_hooks_sidecar(path: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Top-level settings.json keys llmenv renders authoritatively. On a re-render
/// these are **replaced** with llmenv's freshly-computed value — a rule llmenv
/// dropped from config must actually disappear, and `permissions` must never be
/// weakened by a stale union. The one shared key, `hooks`, is handled specially
/// (see [`reconcile_settings`]) so a plugin's self-registered hook survives.
pub(crate) const LLMENV_OWNED_SETTINGS_KEYS: [&str; 10] = [
    "permissions",
    "enabledPlugins",
    "extraKnownMarketplaces",
    "autoMemoryEnabled",
    "effortLevel",
    "advisorSize",
    "outputStyle",
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
    // Resolves PATH in-process rather than running `which` (#1382): the
    // subprocess returned "not found" both when claude was absent and when
    // `which` itself couldn't run, so on an image without `which` — routine
    // for distroless and minimal containers — this reported no binary and the
    // caller silently seeded `installMethod` as if claude were unmanaged.
    crate::paths::resolve_on_path("claude").map(|p| p.display().to_string())
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

/// Seed the default `statusLine` hook into `out/settings.json` if absent
/// (#836), pointing Claude Code's statusline at `llmenv statusline`.
///
/// Deliberately a **seed**, not an owned/authoritative render key (contrast
/// `autoMemoryEnabled`): `statusLine` is not in [`LLMENV_OWNED_SETTINGS_KEYS`],
/// so if this were instead emitted unconditionally into `render_settings`'s
/// `fresh` map, `reconcile_settings`'s passthrough rule would write it
/// through on *every* re-render — permanently clobbering a user's own
/// `/statusline` customization (which writes straight to `settings.json`,
/// outside llmenv) on the very next shell-hook materialize. Seeding once,
/// only when absent, matches [`seed_install_method`]'s contract: once any
/// value exists — llmenv's default, a user customization, or a
/// `native.claude_code.statusLine` override already written by
/// `reconcile_settings` before this runs — it is left alone.
///
/// No-op if `settings.json` does not exist yet (materialize hasn't run) or if
/// `statusLine` is already present.
///
/// # Errors
/// Returns an error if the file exists but cannot be read or written.
pub(crate) fn seed_status_line(out: &std::path::Path) -> anyhow::Result<()> {
    let settings_path = out.join("settings.json");

    match std::fs::read(&settings_path) {
        Ok(bytes) => {
            if let Ok(serde_json::Value::Object(obj)) =
                serde_json::from_slice::<serde_json::Value>(&bytes)
                && obj.contains_key("statusLine")
            {
                return Ok(());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File doesn't exist yet, that's fine.
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading {} for seeding statusLine: {e}",
                settings_path.display()
            ));
        }
    }

    let mut seeded = serde_json::Map::new();
    seeded.insert(
        "statusLine".to_string(),
        // `--color always`: Claude Code invokes this with stdout captured
        // (never a TTY), so the default `auto` color resolution would
        // silently disable every `style:` widget override — the seeded
        // command must opt in explicitly rather than rely on TTY detection.
        serde_json::json!({ "type": "command", "command": "llmenv statusline --color always" }),
    );
    apply_seeded_settings(out, &seeded)
}

/// Collapse duplicate hook entries in a settings.json-shaped hooks doc.
///
/// Null-valued keys (e.g. a null `tool`) differ from absent keys under JSON
/// `PartialEq`, so entries differing only by null-vs-absent don't dedup. Strip
/// nulls first, then dedup each event's entry array so entries from different
/// sources (typed hooks, the `native_hooks` overlay, prior render generations)
/// converge to one entry per event, matcher, and command.
fn dedup_hooks_doc(hooks: &mut serde_json::Value) {
    super::strip_json_nulls(hooks);
    if let Some(obj) = hooks.as_object_mut() {
        for entries in obj.values_mut() {
            if let Some(arr) = entries.as_array_mut() {
                dedup(arr);
            }
        }
    }
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
fn reconcile_settings(
    path: &Path,
    fresh: serde_json::Value,
    prev_owned_hooks: Option<&serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
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
                        // #991: before unioning, drop any on-disk hook that llmenv
                        // rendered last time (prev_owned) but not this time — a
                        // hook removed from config must disappear, not linger via
                        // the union. Foreign hooks (never in prev_owned) are kept.
                        purge_stale_owned_hooks(v, prev_owned_hooks, fresh_val);
                        merge_json(v, fresh_val.clone());
                        // merge_json only dedups byte-identical entries; entries
                        // differing by null-vs-absent keys need the strip-then-
                        // dedup pass to converge across render generations (#977).
                        dedup_hooks_doc(v);
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

/// Normalize a hook entry for equality comparison — a clone with null-valued
/// keys stripped, so entries differing only by null-vs-absent compare equal
/// (same basis [`dedup_hooks_doc`] uses).
fn normalized_hook(entry: &serde_json::Value) -> serde_json::Value {
    let mut clone = entry.clone();
    super::strip_json_nulls(&mut clone);
    clone
}

/// Drop from the on-disk `existing` hooks doc any entry that llmenv rendered in
/// the previous round (`prev_owned`) but is not rendering this round (`fresh`).
///
/// This is the owned-vs-foreign distinction (#991): an entry present in
/// `prev_owned` was llmenv's, so if it's gone from `fresh` the user removed it
/// from config and it must be purged rather than preserved by the union. A hook
/// never in `prev_owned` is foreign (a plugin self-registered it) and is left
/// for the union to keep. No-op when there's no sidecar (`prev_owned` is None).
fn purge_stale_owned_hooks(
    existing: &mut serde_json::Value,
    prev_owned: Option<&serde_json::Value>,
    fresh: &serde_json::Value,
) {
    let (Some(prev), Some(existing_obj)) = (
        prev_owned.and_then(|v| v.as_object()),
        existing.as_object_mut(),
    ) else {
        return;
    };
    for (event, prev_entries) in prev {
        let Some(prev_arr) = prev_entries.as_array() else {
            continue;
        };
        let fresh_norm: Vec<serde_json::Value> = fresh
            .get(event)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(normalized_hook).collect())
            .unwrap_or_default();
        let stale: Vec<serde_json::Value> = prev_arr
            .iter()
            .map(normalized_hook)
            .filter(|e| !fresh_norm.contains(e))
            .collect();
        if stale.is_empty() {
            continue;
        }
        if let Some(arr) = existing_obj.get_mut(event).and_then(|v| v.as_array_mut()) {
            arr.retain(|e| !stale.contains(&normalized_hook(e)));
        }
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
    let tool = normalize_deprecated_tool(&rule.tool);
    if let Some(pattern) = &rule.pattern {
        return vec![format!("{tool}({pattern})")];
    }
    if !rule.paths.is_empty() {
        return rule.paths.iter().map(|p| format!("{tool}({p})")).collect();
    }
    vec![tool]
}

/// Claude Code deprecated the `Write` permission tool name in favor of `Edit`
/// (anthropics/claude-code#78817): a `Write`/`Write(pattern)` rule string only
/// produces a "Fix:" warning now instead of matching anything. Rewrite it
/// before it lands in settings.json so llmenv doesn't hand users the exact
/// warning it exists to spare them from. Only an exact `Write` tool name (bare
/// or immediately followed by `(`) matches — `WriteFile` or a `Write` mention
/// inside another tool's pattern must pass through untouched.
///
/// Applying this to a `deny` rule is safe by construction, not just by luck:
/// a deterministic many-to-one rewrite applied uniformly can only merge
/// distinct strings into new matches, never split an existing match apart, so
/// no prior deny/suppression relationship is lost — the sole precondition is
/// the upstream deprecation claim itself (Write no longer needs its own gate
/// because Edit's rules now cover it).
fn normalize_deprecated_tool(rule: &str) -> String {
    match rule.strip_prefix("Write") {
        Some(rest) if rest.is_empty() || rest.starts_with('(') => format!("Edit{rest}"),
        _ => rule.to_string(),
    }
}

/// Map the neutral `PermissionMode` onto Claude Code's `defaultMode` string.
fn permission_mode_str(mode: crate::config::PermissionMode) -> &'static str {
    use crate::config::PermissionMode;
    match mode {
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Default => "default",
        PermissionMode::BypassPermissions => "bypassPermissions",
        PermissionMode::Auto => "auto",
        PermissionMode::DontAsk => "dontAsk",
        PermissionMode::Manual => "manual",
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

/// Which of the three tool-risk tiers (#946) a feature-enabled MCP's tool
/// belongs to. Read-only/mutation default to `allow`; destructive defaults to
/// `ask` — see [`apply_mcp_tier_permissions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpTier {
    ReadOnly,
    Mutation,
    Destructive,
}

/// Default action for a tier absent an override: read-only and mutation tools
/// are usable without prompting; destructive tools ask first (#946).
fn default_mcp_tier_action(tier: McpTier) -> PermissionAction {
    match tier {
        McpTier::ReadOnly | McpTier::Mutation => PermissionAction::Allow,
        McpTier::Destructive => PermissionAction::Ask,
    }
}

/// The three permission rule vectors rendered into `settings.json`, bundled
/// so functions that touch all three (like [`apply_mcp_tier_permissions`])
/// stay under the project's 5-positional-param limit.
struct PermBuckets<'a> {
    allow: &'a mut Vec<String>,
    ask: &'a mut Vec<String>,
    deny: &'a mut Vec<String>,
}

/// The native-authored deny/ask rule strings from `generate_settings_json`
/// (its `native_deny`/`native_ask` sets), threaded into
/// `apply_mcp_tier_permissions` so a tiered MCP tool already covered by a
/// more authoritative user-authored native rule doesn't also get a competing
/// tier-default entry (#972). Bundled into one struct — same reason as
/// `PermBuckets` — so passing both sets still counts as a single positional
/// param.
struct NativeCover<'a> {
    deny: &'a std::collections::BTreeSet<&'a str>,
    ask: &'a std::collections::BTreeSet<&'a str>,
}

/// Render one feature-enabled MCP's tool tiers into `buckets`, applying the
/// default tier policy or the feature's `mcp_permissions` override. Each tool
/// lands in exactly one action bucket — unlike the #490 wildcard this
/// replaces, there is no broader rule left to conflict with these specific
/// ones.
///
/// A tool's *resolved* action (override else tier default) is suppressed —
/// not pushed into its bucket at all — when a more authoritative native rule
/// already covers that exact `{prefix}{tool}` string: deny > ask > allow,
/// mirroring the `suppressors` closure's authority order above for neutral
/// rules (#972). A resolved deny is never suppressed — nothing outranks it.
fn apply_mcp_tier_permissions(
    buckets: &mut PermBuckets<'_>,
    prefix: &str,
    tiers: [(&[&str], McpTier); 3],
    overrides: Option<&crate::config::McpPermissions>,
    native: &NativeCover<'_>,
) {
    for (tools, tier) in tiers {
        let configured = overrides.and_then(|o| match tier {
            McpTier::ReadOnly => o.read_only,
            McpTier::Mutation => o.mutation,
            McpTier::Destructive => o.destructive,
        });
        let action = match configured {
            Some(crate::config::McpPermissionAction::Allow) => PermissionAction::Allow,
            Some(crate::config::McpPermissionAction::Ask) => PermissionAction::Ask,
            Some(crate::config::McpPermissionAction::Deny) => PermissionAction::Deny,
            None => default_mcp_tier_action(tier),
        };
        let bucket = match action {
            PermissionAction::Allow => &mut *buckets.allow,
            PermissionAction::Ask => &mut *buckets.ask,
            PermissionAction::Deny => &mut *buckets.deny,
        };
        for t in tools {
            let rendered = format!("{prefix}{t}");
            let suppressed = match action {
                PermissionAction::Allow => {
                    native.deny.contains(rendered.as_str())
                        || native.ask.contains(rendered.as_str())
                }
                PermissionAction::Ask => native.deny.contains(rendered.as_str()),
                PermissionAction::Deny => false,
            };
            if !suppressed {
                bucket.push(rendered);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::AgentAdapter;
    use super::{
        CBM_DESTRUCTIVE, CBM_MCP_PREFIX, CBM_MUTATION, CBM_READ_ONLY, CLAUDE_JSON_FILE,
        CLAUDE_JSON_OWNED_SERVERS_FILE, CONFIG_CONTEXT_COMMAND, CONFIG_GUARD_COMMAND,
        CTX_DESTRUCTIVE, CTX_MUTATION, CTX_READ_ONLY, ClaudeCodeAdapter, HOOK_RUN_COMMAND,
        ICM_DESTRUCTIVE, ICM_MUTATION, ICM_READ_ONLY, LLMENV_OWNED_SETTINGS_KEYS,
        MODELED_SETTINGS_KEYS, classify_claude_path, dedup_hooks_doc,
        generate_installed_plugins_json, generate_settings_json, is_hook_json,
        merge_mcp_into_claude_json, normalize_deprecated_tool, overlay_native, permission_mode_str,
        read_owned_servers, reconcile_settings, reject_modeled_keys_in_catch_all,
        render_marketplace_source, render_permission_rule, seed_install_method, seed_status_line,
    };
    use crate::adapter::skills::{
        arb_distinct_resolved_mcps, arb_yaml_value, reject_hardcoded_config_path, validate_skills,
    };
    use crate::config::PermissionRule;
    use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
    use crate::merge::MergedManifest;
    use crate::plugins::resolve::{ResolvedMarketplace, ResolvedPlugin};
    use proptest::prelude::*;
    use std::path::PathBuf;

    /// `materialize`'s bundle-files loop re-renders into `out`, a folder that
    /// persists across calls (the agent's live config dir). If a prior
    /// render's destination entry gets replaced by a symlink, the next
    /// render must replace the symlink rather than write through it —
    /// the same class of TOCTOU bug `src/materialize/inherit.rs` was
    /// hardened against for #1341, extended here per #1422.
    #[cfg(unix)]
    #[test]
    fn materialize_does_not_write_bundle_files_through_a_symlinked_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle_src = tmp.path().join("bundle-file.txt");
        std::fs::write(&bundle_src, b"bundle-content").unwrap();
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, b"must-not-be-touched").unwrap();

        let mut manifest = MergedManifest {
            files: std::collections::BTreeMap::new(),
            ..MergedManifest::default()
        };
        manifest
            .files
            .insert(PathBuf::from("out.txt"), bundle_src.clone());

        ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();
        let dest_file = tmp.path().join("out.txt");
        std::fs::remove_file(&dest_file).unwrap();
        std::os::unix::fs::symlink(&victim, &dest_file).unwrap();

        ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();

        assert!(
            !std::fs::symlink_metadata(&dest_file)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the planted symlink must be replaced, not written through"
        );
        assert_eq!(std::fs::read(&dest_file).unwrap(), b"bundle-content");
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"must-not-be-touched",
            "the symlink's target must be untouched"
        );
    }

    /// The stronger case #1427 closes: a symlinked *directory* component
    /// anywhere in a bundle file's relative path must not be followed
    /// either — `create_dir_all` on a path containing one would resolve
    /// through it, landing the write inside the symlink's target instead of
    /// under the materialized folder.
    #[cfg(unix)]
    #[test]
    fn materialize_does_not_follow_a_symlinked_directory_component() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle_src = tmp.path().join("bundle-file.txt");
        std::fs::write(&bundle_src, b"bundle-content").unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let mut manifest = MergedManifest {
            files: std::collections::BTreeMap::new(),
            ..MergedManifest::default()
        };
        manifest
            .files
            .insert(PathBuf::from("hooks/out.txt"), bundle_src.clone());

        ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();
        let hooks_dir = tmp.path().join("hooks");
        std::fs::remove_dir_all(&hooks_dir).unwrap();
        std::os::unix::fs::symlink(&outside, &hooks_dir).unwrap();

        let err = ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        let _ = err;

        assert!(
            !outside.join("out.txt").exists(),
            "the write must not land inside the symlinked directory's target"
        );
    }

    /// The hook-JSON render branch (`{{ICM_MCP}}` substitution) must get the
    /// same directory-component protection as the raw-copy branch — it isn't
    /// exempt just because the content comes from `write_owner_only` instead
    /// of a file copy (#1427).
    #[cfg(unix)]
    #[test]
    fn materialize_does_not_follow_a_symlinked_directory_component_for_hook_json() {
        let tmp = tempfile::tempdir().unwrap();
        let hook_src = tmp.path().join("hook-src.json");
        std::fs::write(&hook_src, br#"{"command": "{{ICM_MCP}}"}"#).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let mut manifest = MergedManifest {
            files: std::collections::BTreeMap::new(),
            ..MergedManifest::default()
        };
        manifest
            .files
            .insert(PathBuf::from("hooks/hook.json"), hook_src);

        ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();
        let hooks_dir = tmp.path().join("hooks");
        std::fs::remove_dir_all(&hooks_dir).unwrap();
        std::os::unix::fs::symlink(&outside, &hooks_dir).unwrap();

        let err = ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap_err();
        let _ = err;

        assert!(
            !outside.join("hook.json").exists(),
            "the write must not land inside the symlinked directory's target"
        );
    }

    /// #1262: an empty `agents_md` with no applicable fragment must not leave a
    /// 0-byte `CLAUDE.md` on disk.
    #[test]
    fn materialize_omits_claude_md_when_there_is_no_content() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        let owned = ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();

        assert!(
            !tmp.path().join("CLAUDE.md").exists(),
            "no agents_md and no fragment must write no CLAUDE.md at all"
        );
        assert!(
            !owned.contains(&PathBuf::from("CLAUDE.md")),
            "CLAUDE.md must be absent from the owned set so a stale copy from a \
             prior render is reconciled away as a ghost"
        );
    }

    /// #1262: whitespace-only content is as empty as the empty string — writing
    /// it would produce a file whose only content is a newline.
    #[test]
    fn materialize_omits_claude_md_when_content_is_only_whitespace() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest {
            agents_md: "  \n\t\n".into(),
            ..MergedManifest::default()
        };
        ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();

        assert!(
            !tmp.path().join("CLAUDE.md").exists(),
            "whitespace-only agents_md must write no CLAUDE.md"
        );
    }

    /// #1262 non-regression: real content still lands verbatim.
    #[test]
    fn materialize_writes_claude_md_when_there_is_content() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest {
            agents_md: "# Project rules\n".into(),
            ..MergedManifest::default()
        };
        let owned = ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();

        let written = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert_eq!(written, "# Project rules\n");
        assert!(owned.contains(&PathBuf::from("CLAUDE.md")));
    }

    #[test]
    fn materialize_emits_no_schema_sidecar_when_adapter_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = MergedManifest::default();
        ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();
        let has_schema_file = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".schema.json")
            });
        assert!(
            !has_schema_file,
            "ClaudeCodeAdapter has no config_schema() override — must emit no sidecar"
        );
    }

    fn marketplace(name: &str, source: &str, install: Option<&str>) -> ResolvedMarketplace {
        ResolvedMarketplace {
            name: name.into(),
            source: source.into(),
            install_location: install.map(Into::into),
            head: None,
        }
    }

    proptest! {
        // dedup_hooks_doc is idempotent and leaves no per-event duplicates,
        // for any hooks doc built from a small pool of entries (which forces
        // collisions). Pins the #977 normalization primitive.
        #[test]
        fn prop_dedup_hooks_doc_idempotent_and_unique(
            picks in proptest::collection::vec(0usize..3, 0..12)
        ) {
            let pool = [
                serde_json::json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": "a" }] }),
                serde_json::json!({ "matcher": "Bash", "hooks": [{ "type": "command", "command": "a", "tool": null }] }),
                serde_json::json!({ "hooks": [{ "type": "command", "command": "b" }] }),
            ];
            let entries: Vec<serde_json::Value> = picks.iter().map(|i| pool[*i].clone()).collect();
            let mut doc = serde_json::json!({ "PreToolUse": entries });

            dedup_hooks_doc(&mut doc);
            let once = doc.clone();
            dedup_hooks_doc(&mut doc);
            prop_assert_eq!(&doc, &once, "dedup must be idempotent");

            if let Some(arr) = doc["PreToolUse"].as_array() {
                for i in 0..arr.len() {
                    for j in (i + 1)..arr.len() {
                        prop_assert_ne!(&arr[i], &arr[j], "no duplicate entries survive");
                    }
                }
            }
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
            tool in "[A-Za-z]{1,12}".prop_filter("not the deprecated Write tool name", |t| t != "Write"),
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
            tool in "[A-Za-z]{1,12}".prop_filter("not the deprecated Write tool name", |t| t != "Write"),
            paths in proptest::collection::vec("[^()]{1,10}", 1..5),
        ) {
            let rule = PermissionRule { tool: tool.clone(), pattern: None, paths: paths.clone() };
            let out = render_permission_rule(&rule);
            let expected: Vec<String> = paths.iter().map(|p| format!("{tool}({p})")).collect();
            prop_assert_eq!(out, expected);
        }

        // No pattern and no paths → a bare tool-wide rule.
        #[test]
        fn bare_tool_renders_tool_name(
            tool in "[A-Za-z]{1,12}".prop_filter("not the deprecated Write tool name", |t| t != "Write"),
        ) {
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

        // #768 overlay_native completeness: every top-level key of a mapping
        // fragment is present in the destination after the overlay (the
        // fragment's keys are additively layered onto whatever was there).
        #[test]
        fn overlay_native_maps_fragment_keys_into_dst(
            keys in proptest::collection::vec("[a-z]{1,8}", 0..6),
        ) {
            let mut map = serde_yaml::Mapping::new();
            for k in &keys {
                map.insert(
                    serde_yaml::Value::String(k.clone()),
                    serde_yaml::Value::Bool(true),
                );
            }
            let frag = serde_yaml::Value::Mapping(map);
            let mut dst = serde_json::json!({ "preexisting": 1 });
            overlay_native(&mut dst, Some(&frag)).unwrap();
            let obj = dst.as_object().unwrap();
            prop_assert!(obj.contains_key("preexisting"), "existing key dropped");
            for k in &keys {
                prop_assert!(obj.contains_key(k), "fragment key {k:?} missing after overlay");
            }
        }

        // #768 overlay_native deep merge: a nested mapping merges key-by-key into
        // an existing nested object rather than clobbering it — the untouched
        // sibling key survives alongside the fragment's addition.
        #[test]
        fn overlay_native_deep_merges_nested_objects(add_key in "[a-z]{1,8}") {
            let mut dst = serde_json::json!({ "nested": { "keep": "old" } });
            let mut inner = serde_yaml::Mapping::new();
            inner.insert(
                serde_yaml::Value::String(add_key.clone()),
                serde_yaml::Value::String("new".to_owned()),
            );
            let mut outer = serde_yaml::Mapping::new();
            outer.insert(
                serde_yaml::Value::String("nested".to_owned()),
                serde_yaml::Value::Mapping(inner),
            );
            let frag = serde_yaml::Value::Mapping(outer);
            overlay_native(&mut dst, Some(&frag)).unwrap();
            let nested = dst["nested"].as_object().unwrap();
            // The fragment's key is present…
            prop_assert_eq!(nested.get(&add_key).and_then(|v| v.as_str()), Some("new"));
            // …and unless the fragment overwrote it, the sibling `keep` survives.
            if add_key != "keep" {
                prop_assert_eq!(
                    nested.get("keep").and_then(|v| v.as_str()),
                    Some("old"),
                    "deep merge must not clobber the untouched sibling key"
                );
            }
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
        fn merge_mcp_roundtrips_distinct_servers(mcps in arb_distinct_resolved_mcps()) {
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
        fn merge_mcp_empty_overlay_is_deterministic(mcps in arb_distinct_resolved_mcps()) {
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
        fn merge_mcp_writes_owner_only_permissions(mcps in arb_distinct_resolved_mcps()) {
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
        fn merge_mcp_serde_roundtrip(mcps in arb_distinct_resolved_mcps()) {
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

    /// The three structural shapes `merge_json` dispatches on. Two values of
    /// different kinds are a value-type conflict, which must replace wholesale
    /// rather than structurally merge (#852).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ValueKind {
        Scalar,
        Sequence,
        Mapping,
    }

    const VALUE_KINDS: [ValueKind; 3] =
        [ValueKind::Scalar, ValueKind::Sequence, ValueKind::Mapping];

    fn json_kind(value: &serde_json::Value) -> ValueKind {
        match value {
            serde_json::Value::Array(_) => ValueKind::Sequence,
            serde_json::Value::Object(_) => ValueKind::Mapping,
            _ => ValueKind::Scalar,
        }
    }

    /// Arbitrary YAML of exactly `kind`. Containers are non-empty so a
    /// value-type conflict can't be satisfied vacuously, and no leaf is null:
    /// null has its own documented merge semantics (stripped on insert,
    /// replaces on collision) covered by the idempotence property instead.
    fn arb_yaml_of_kind(kind: ValueKind) -> BoxedStrategy<serde_yaml::Value> {
        let scalar = prop_oneof![
            any::<bool>().prop_map(serde_yaml::Value::Bool),
            any::<i64>().prop_map(|n| serde_yaml::Value::Number(n.into())),
            "[a-z]{1,6}".prop_map(serde_yaml::Value::String),
        ];
        match kind {
            ValueKind::Scalar => scalar.boxed(),
            ValueKind::Sequence => proptest::collection::vec(scalar, 1..4)
                .prop_map(serde_yaml::Value::Sequence)
                .boxed(),
            ValueKind::Mapping => proptest::collection::vec(
                ("[a-z]{1,6}".prop_map(serde_yaml::Value::String), scalar),
                1..4,
            )
            .prop_map(|pairs| serde_yaml::Value::Mapping(pairs.into_iter().collect()))
            .boxed(),
        }
    }

    /// A colliding key plus two values of *different* kinds for it: the value
    /// already in the destination, and the one the native fragment carries.
    fn arb_value_type_conflict()
    -> impl Strategy<Value = (String, serde_yaml::Value, serde_yaml::Value)> {
        (0usize..VALUE_KINDS.len(), 0usize..VALUE_KINDS.len())
            .prop_filter("kinds must differ to be a value-type conflict", |(a, b)| {
                a != b
            })
            .prop_flat_map(|(dst_idx, frag_idx)| {
                (
                    "[a-z]{1,8}".prop_map(String::from),
                    arb_yaml_of_kind(VALUE_KINDS[dst_idx]),
                    arb_yaml_of_kind(VALUE_KINDS[frag_idx]),
                )
            })
    }

    proptest! {
        // #852 overlay_native value-type conflict: when the destination holds one
        // structural kind at a key and the fragment holds a different kind, the
        // fragment's value must replace it wholesale. No partial blend, no
        // leaked destination content, no panic. `merge_json` only recurses when
        // both sides are objects or both are arrays; every other pairing hits
        // the overwrite arm, and this pins that down as a property rather than
        // trusting the match arms to stay ordered correctly.
        #[test]
        fn overlay_native_replaces_on_value_type_conflict(
            (key, dst_val, frag_val) in arb_value_type_conflict(),
        ) {
            let dst_json: serde_json::Value = serde_json::to_value(&dst_val).unwrap();
            let mut dst = serde_json::json!({ key.clone(): dst_json.clone() });

            let mut map = serde_yaml::Mapping::new();
            map.insert(serde_yaml::Value::String(key.clone()), frag_val.clone());
            overlay_native(&mut dst, Some(&serde_yaml::Value::Mapping(map))).unwrap();

            let frag_json: serde_json::Value = serde_json::to_value(&frag_val).unwrap();
            let got = dst.get(&key).unwrap();

            // The result takes the fragment's shape, not the destination's.
            prop_assert_eq!(
                json_kind(got),
                json_kind(&frag_json),
                "conflict at {:?} kept the destination's shape: {} vs fragment {}",
                key, got, frag_json
            );

            match &frag_json {
                // A scalar fragment replaces any container exactly.
                serde_json::Value::Object(want) => {
                    let got_obj = got.as_object().unwrap();
                    // Exactly the fragment's keys — no destination key smuggled in.
                    prop_assert!(
                        got_obj.keys().eq(want.keys()),
                        "object conflict must yield exactly the fragment's keys: {got} vs {frag_json}"
                    );
                    for (k, v) in want {
                        prop_assert_eq!(got_obj.get(k), Some(v), "fragment value for {:?} altered", k);
                    }
                }
                serde_json::Value::Array(want) => {
                    let got_arr = got.as_array().unwrap();
                    // Dedup can shrink the array but never introduce an element.
                    for item in got_arr {
                        prop_assert!(
                            want.contains(item),
                            "array conflict leaked a non-fragment element {item}: {got}"
                        );
                    }
                    for item in want {
                        prop_assert!(
                            got_arr.contains(item),
                            "array conflict dropped fragment element {item}: {got}"
                        );
                    }
                }
                scalar => prop_assert_eq!(got, scalar, "scalar fragment must replace exactly"),
            }
        }
    }

    // ---- generate_settings_json: permission render ----

    /// A manifest with one rendered rule per bucket, plus a `native.claude_code`
    /// fragment. #750's whole question is what the fragment may and may not do
    /// to what the renderer produced.
    fn manifest_with_native_permissions(fragment: &str) -> crate::merge::MergedManifest {
        let mut m = crate::merge::MergedManifest::default();
        m.capabilities.permissions = crate::config::Permissions {
            allow: vec![crate::config::PermissionRule {
                tool: "Read".into(),
                pattern: Some("//a".into()),
                paths: Vec::new(),
            }],
            ask: vec![crate::config::PermissionRule {
                tool: "Bash".into(),
                pattern: Some("git push:*".into()),
                paths: Vec::new(),
            }],
            deny: vec![crate::config::PermissionRule {
                tool: "Bash".into(),
                pattern: Some("curl:*".into()),
                paths: Vec::new(),
            }],
            ..Default::default()
        };
        m.native.insert(
            "claude_code".to_string(),
            serde_yaml::from_str(fragment).expect("fragment parses"),
        );
        m
    }

    fn perm_strings(settings: &serde_json::Value, action: &str) -> Vec<String> {
        settings["permissions"][action]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    }

    // #750: the catch-all used to hard-reject `permissions` outright. It now
    // merges, which is only acceptable because the merge is additive.
    #[test]
    fn native_permissions_in_catch_all_append_without_replacing() {
        let settings = render_settings_for_test(&manifest_with_native_permissions(
            "permissions:\n  allow: [\"Read(//b)\"]\n  ask: [\"Bash(git tag:*)\"]\n  deny: [\"Bash(rm:*)\"]\n",
        ));
        for (action, rendered, added) in [
            ("allow", "Read(//a)", "Read(//b)"),
            ("ask", "Bash(git tag:*)", "Bash(git tag:*)"),
            ("deny", "Bash(curl:*)", "Bash(rm:*)"),
        ] {
            let got = perm_strings(&settings, action);
            assert!(
                got.contains(&rendered.to_string()),
                "{action} kept the rendered rule: {got:?}"
            );
            assert!(
                got.contains(&added.to_string()),
                "{action} gained the native rule: {got:?}"
            );
        }
        // The rendered ask survives too — spelled out separately because the
        // loop above reuses one string for both roles on `ask`.
        assert!(perm_strings(&settings, "ask").contains(&"Bash(git push:*)".to_string()));
    }

    // The security property the hard-reject existed to protect. A fragment that
    // omits a rendered deny, or tries to allow it, must not weaken the output.
    #[test]
    fn native_permissions_cannot_drop_or_downgrade_a_rendered_deny() {
        let settings = render_settings_for_test(&manifest_with_native_permissions(
            "permissions:\n  allow: [\"Bash(curl:*)\"]\n  deny: []\n",
        ));
        assert!(
            perm_strings(&settings, "deny").contains(&"Bash(curl:*)".to_string()),
            "an empty native deny must not erase the rendered deny: {:?}",
            perm_strings(&settings, "deny")
        );
        assert!(
            !perm_strings(&settings, "allow").contains(&"Bash(curl:*)".to_string()),
            "deny outranks a native allow of the same rule: {:?}",
            perm_strings(&settings, "allow")
        );
    }

    // The reason #750 exists: keys llmenv doesn't model reach settings.json
    // without waiting for the neutral schema (and a release) to grow a field.
    #[test]
    fn native_permissions_pass_through_keys_llmenv_does_not_model() {
        let settings = render_settings_for_test(&manifest_with_native_permissions(
            "permissions:\n  additionalDirectories: [\"/srv/shared\"]\n  disableBypassPermissionsMode: disable\n",
        ));
        assert_eq!(
            settings["permissions"]["additionalDirectories"][0].as_str(),
            Some("/srv/shared")
        );
        assert_eq!(
            settings["permissions"]["disableBypassPermissionsMode"].as_str(),
            Some("disable")
        );
        // ...and doing so leaves the rendered arrays intact.
        assert!(perm_strings(&settings, "deny").contains(&"Bash(curl:*)".to_string()));
    }

    // A fragment with no `permissions` key must still reach settings.json
    // through the generic catch-all path, unchanged by #750's special-casing.
    #[test]
    fn catch_all_keys_other_than_permissions_still_overlay() {
        let settings = render_settings_for_test(&manifest_with_native_permissions(
            "permissions:\n  allow: [\"Read(//b)\"]\napiKeyHelper: /usr/local/bin/key.sh\n",
        ));
        assert_eq!(
            settings["apiKeyHelper"].as_str(),
            Some("/usr/local/bin/key.sh")
        );
    }

    // P0 from pre-pr-review. `defaultMode` is a *modeled* key (#748 put it in the
    // neutral vocabulary as `capabilities.permissions.default_mode`), and
    // `bypassPermissions` switches the permission system off wholesale. Letting
    // the catch-all set it would hand any bundle that can author `native:` a
    // one-line escalation past every rendered ask and deny — which is precisely
    // what the hard error used to prevent.
    #[test]
    fn native_catch_all_cannot_set_default_mode() {
        let mut m =
            manifest_with_native_permissions("permissions:\n  defaultMode: bypassPermissions\n");
        m.capabilities.permissions.default_mode = Some(crate::config::PermissionMode::Default);
        let tmp = tempfile::tempdir().unwrap();
        let err = generate_settings_json(tmp.path(), &m)
            .unwrap_err()
            .to_string();
        assert!(err.contains("defaultMode"), "names the key: {err}");
        assert!(
            err.contains("permissions.default_mode"),
            "points at the modeled field: {err}"
        );
    }

    // The escape hatch still works for the keys it exists for.
    #[test]
    fn native_catch_all_still_carries_unmodeled_permission_keys() {
        let settings = render_settings_for_test(&manifest_with_native_permissions(
            "permissions:\n  disableBypassPermissionsMode: disable\n",
        ));
        assert_eq!(
            settings["permissions"]["disableBypassPermissionsMode"].as_str(),
            Some("disable")
        );
    }

    // P1 from pre-pr-review. `native_plugins` blind-merges at the settings top
    // level *before* the catch-all guard runs, so a `permissions` key there
    // reached `merge_json` unguarded — a scalar could replace the whole rendered
    // array. The additive merge is only additive if nothing upstream of it can
    // clobber the base first.
    #[test]
    fn native_plugins_cannot_smuggle_a_permissions_key() {
        let mut m = crate::merge::MergedManifest::default();
        m.capabilities.native_plugins.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("permissions:\n  deny: \"clobbered\"\n").unwrap(),
        );
        let tmp = tempfile::tempdir().unwrap();
        let err = generate_settings_json(tmp.path(), &m)
            .unwrap_err()
            .to_string();
        assert!(err.contains("permissions"), "names the key: {err}");
    }

    // P2 from pre-pr-review. A scalar `native_plugins` fragment replaced the
    // entire settings object via merge_json's shape-mismatch arm, which also
    // took the object-ness the permissions merge depends on with it.
    #[test]
    fn native_plugins_must_be_a_mapping() {
        let mut m = crate::merge::MergedManifest::default();
        m.capabilities.native_plugins.insert(
            "claude_code".to_string(),
            serde_yaml::Value::String("not a mapping".into()),
        );
        let tmp = tempfile::tempdir().unwrap();
        let err = generate_settings_json(tmp.path(), &m)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mapping"), "explains the shape: {err}");
    }

    // P2 from pre-pr-review. The doc comment claims parity with the
    // `native_permissions` sibling, which runs #888's deprecated-tool
    // normalization. Without it a catch-all `Write(...)` rule silently matches
    // nothing, because Claude Code renamed the tool to `Edit`.
    #[test]
    fn native_catch_all_permission_strings_are_normalized_like_the_sibling() {
        let settings = render_settings_for_test(&manifest_with_native_permissions(
            "permissions:\n  deny: [\"Write(~/.ssh/**)\"]\n",
        ));
        let deny = perm_strings(&settings, "deny");
        assert!(
            deny.contains(&"Edit(~/.ssh/**)".to_string()),
            "Write should normalize to Edit like native_permissions does: {deny:?}"
        );
    }

    // `hooks` keeps the hard-reject: an array of matcher groups has no
    // unambiguous additive merge, so #750 deliberately stops at permissions.
    #[test]
    fn hooks_in_the_catch_all_are_still_rejected() {
        let mut m = crate::merge::MergedManifest::default();
        m.native.insert(
            "claude_code".to_string(),
            serde_yaml::from_str("hooks:\n  PreToolUse: []\n").unwrap(),
        );
        let tmp = tempfile::tempdir().unwrap();
        let err = generate_settings_json(tmp.path(), &m)
            .unwrap_err()
            .to_string();
        assert!(err.contains("hooks"), "got: {err}");
        assert!(err.contains("native_hooks"), "points at the sibling: {err}");
    }

    fn render_settings_for_test(manifest: &crate::merge::MergedManifest) -> serde_json::Value {
        let tmp = tempfile::tempdir().unwrap();
        // generate_settings_json takes a directory and writes settings.json inside it.
        generate_settings_json(tmp.path(), manifest).unwrap();
        let bytes = std::fs::read(tmp.path().join("settings.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// A manifest whose catch-all `native.claude_code` block sets `key` to
    /// `value`, with every renderer-owned key that `native` may collide with
    /// already emitted so the shared-key overwrite path is the one exercised.
    fn manifest_with_native_override(
        key: &str,
        value: serde_yaml::Value,
    ) -> crate::merge::MergedManifest {
        let mut fragment = serde_yaml::Mapping::new();
        fragment.insert(serde_yaml::Value::String(key.into()), value);
        let mut manifest = crate::merge::MergedManifest::default();
        manifest.capabilities.auto_memory_enabled = Some(true);
        manifest.capabilities.effort_level = Some("high".into());
        manifest.capabilities.advisor_size = Some("large".into());
        manifest.native = std::collections::BTreeMap::from([(
            "claude_code".to_owned(),
            serde_yaml::Value::Mapping(fragment),
        )]);
        manifest
    }

    /// #1264: `native.<engine>.<key>: null` means "delete the key", so the
    /// renderer must emit nothing rather than an explicit JSON `null`. Covers
    /// every key reachable through the catch-all (`permissions` and `hooks` are
    /// refused by `reject_modeled_keys_in_catch_all`, so they can't get here).
    #[test]
    fn native_null_removes_a_rendered_settings_key() {
        for key in ["autoMemoryEnabled", "effortLevel", "advisorSize"] {
            let settings = render_settings_for_test(&manifest_with_native_override(
                key,
                serde_yaml::Value::Null,
            ));
            assert!(
                settings.get(key).is_none(),
                "`native.claude_code.{key}: null` must delete the key, got: {settings}"
            );
        }
    }

    /// #1264: the null-strip must not stop at the top level — a null nested
    /// inside an object value would violate #720's invariant just the same.
    #[test]
    fn native_null_nested_in_an_object_is_stripped_too() {
        let nested: serde_yaml::Value = serde_yaml::from_str("outer:\n  inner: null\n").unwrap();
        let settings = render_settings_for_test(&manifest_with_native_override("someKey", nested));
        assert_eq!(
            settings["someKey"],
            serde_json::json!({ "outer": {} }),
            "a null nested under a native key must be stripped, got: {settings}"
        );
    }

    /// #1264 non-regression: a non-null native value still overrides the
    /// renderer's own emission — that override is the whole point of emitting
    /// these keys before the overlay.
    #[test]
    fn native_non_null_still_overrides_the_rendered_value() {
        let settings = render_settings_for_test(&manifest_with_native_override(
            "autoMemoryEnabled",
            serde_yaml::Value::Bool(false),
        ));
        assert_eq!(
            settings["autoMemoryEnabled"],
            serde_json::json!(false),
            "native must still win on a non-null collision, got: {settings}"
        );
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

    /// Every `matcher` registered for a native hook event, in order.
    fn hook_matchers_for(settings: &serde_json::Value, event: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e["matcher"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn cbm_manifest() -> crate::merge::MergedManifest {
        crate::merge::MergedManifest {
            mcps: vec![crate::mcp::resolve::ResolvedMcp {
                name: crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME.to_string(),
                kind: crate::mcp::resolve::ResolvedKind::Stdio {
                    command: "codebase-memory-mcp".into(),
                    args: vec![],
                    env: Default::default(),
                },
                headers: Default::default(),
                timeout: None,
                disabled_tools: vec![],
                mcp_permissions: None,
                wakeup_max_tokens: None,
            }],
            ..Default::default()
        }
    }

    // #1331: the guard is only reachable if Claude Code actually routes the
    // tool call to it, so the matcher is as load-bearing as the deny itself.
    #[test]
    fn index_repository_guard_is_registered_when_codebase_memory_is_wired() {
        let settings = render_settings_for_test(&cbm_manifest());
        let expected = format!(
            "^{}$",
            crate::hook_run::cbm_index_guard::INDEX_REPOSITORY_TOOL
        );
        assert!(
            hook_matchers_for(&settings, "PreToolUse").contains(&expected),
            "expected {expected} among {:?}",
            hook_matchers_for(&settings, "PreToolUse")
        );
    }

    #[test]
    fn index_repository_guard_is_absent_without_codebase_memory() {
        let settings = render_settings_for_test(&crate::merge::MergedManifest::default());
        assert!(
            !hook_matchers_for(&settings, "PreToolUse")
                .iter()
                .any(|m| m.contains("index_repository")),
            "no codebase-memory MCP is wired, so its tool can never be called"
        );
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
                    retention_days: None,
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
                mcp_permissions: None,
                wakeup_max_tokens: None,
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
                    retention_days: None,
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

    /// String array at `settings.json`'s `permissions.<action>`, empty when absent.
    fn perm_action<'a>(settings: &'a serde_json::Value, action: &str) -> Vec<&'a str> {
        settings
            .get("permissions")
            .and_then(|p| p.get(action))
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default()
    }

    fn context_mode_plugin_manifest() -> crate::merge::MergedManifest {
        crate::merge::MergedManifest {
            plugins: vec![crate::plugins::resolve::ResolvedPlugin {
                marketplace: crate::config::CONTEXT_MODE_MARKETPLACE.into(),
                plugin: crate::config::CONTEXT_MODE_PLUGIN.into(),
                collection: "context_mode (built-in)".into(),
                install_path: None,
                git_commit_sha: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn context_mode_plugin_default_policy_no_wildcard_conflict() {
        // #946: the #490 wildcard allow used to coexist with tier-based
        // ask/deny entries, which Claude Code's deny > ask > allow precedence
        // silently shadowed. Default policy: read-only and mutation tools
        // allow, destructive tools ask — one coherent policy, no wildcard.
        let settings = render_settings_for_test(&context_mode_plugin_manifest());
        let allow = perm_action(&settings, "allow");
        let ask = perm_action(&settings, "ask");
        let deny = perm_action(&settings, "deny");

        for tool in CTX_READ_ONLY.iter().chain(CTX_MUTATION) {
            let rule = format!("{}{tool}", crate::config::CONTEXT_MODE_MCP_PREFIX);
            assert!(
                allow.contains(&rule.as_str()),
                "{rule} missing from allow: {allow:?}"
            );
        }
        for tool in CTX_DESTRUCTIVE {
            let rule = format!("{}{tool}", crate::config::CONTEXT_MODE_MCP_PREFIX);
            assert!(
                ask.contains(&rule.as_str()),
                "{rule} missing from ask: {ask:?}"
            );
        }
        assert!(
            deny.is_empty(),
            "no deny entries expected by default: {deny:?}"
        );

        let wildcard = format!("{}*", crate::config::CONTEXT_MODE_MCP_PREFIX);
        assert!(
            !allow.contains(&wildcard.as_str()),
            "wildcard allow from #490 must no longer be emitted: {allow:?}"
        );
    }

    #[test]
    fn context_mode_absent_no_tiered_permissions() {
        // #694: no context-mode plugin → no ctx_* rules in any permission array.
        let manifest = crate::merge::MergedManifest::default();
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        assert!(
            CTX_READ_ONLY
                .iter()
                .chain(CTX_MUTATION)
                .chain(CTX_DESTRUCTIVE)
                .all(|t| !allow.iter().any(|a| a.contains(t))),
            "context-mode MCP grant must be absent when plugin is absent; got {allow:?}"
        );
    }

    #[test]
    fn icm_active_default_policy_no_wildcard_conflict() {
        // #946: same policy for the ICM MCP as context-mode — read-only and
        // mutation allow, destructive asks.
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
                mcp_permissions: None,
                wakeup_max_tokens: None,
            }],
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let ask = perm_action(&settings, "ask");
        let deny = perm_action(&settings, "deny");

        for tool in ICM_READ_ONLY.iter().chain(ICM_MUTATION) {
            let rule = format!("mcp__icm__{tool}");
            assert!(
                allow.contains(&rule.as_str()),
                "{rule} missing from allow: {allow:?}"
            );
        }
        for tool in ICM_DESTRUCTIVE {
            let rule = format!("mcp__icm__{tool}");
            assert!(
                ask.contains(&rule.as_str()),
                "{rule} missing from ask: {ask:?}"
            );
        }
        assert!(
            deny.is_empty(),
            "no deny entries expected by default: {deny:?}"
        );
    }

    #[test]
    fn codebase_memory_active_default_policy_no_wildcard_conflict() {
        // #1323: same tiered-policy pattern as ICM — read-only tools allow,
        // the one destructive tool (delete_project) asks.
        let manifest = crate::merge::MergedManifest {
            mcps: vec![crate::mcp::resolve::ResolvedMcp {
                name: crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME.to_string(),
                kind: crate::mcp::resolve::ResolvedKind::Stdio {
                    command: "codebase-memory-mcp".into(),
                    args: vec![],
                    env: Default::default(),
                },
                headers: Default::default(),
                timeout: None,
                disabled_tools: vec![],
                mcp_permissions: None,
                wakeup_max_tokens: None,
            }],
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let ask = perm_action(&settings, "ask");
        let deny = perm_action(&settings, "deny");

        for tool in CBM_READ_ONLY.iter().chain(CBM_MUTATION) {
            let rule = format!("{CBM_MCP_PREFIX}{tool}");
            assert!(
                allow.contains(&rule.as_str()),
                "{rule} missing from allow: {allow:?}"
            );
        }
        for tool in CBM_DESTRUCTIVE {
            let rule = format!("{CBM_MCP_PREFIX}{tool}");
            assert!(
                ask.contains(&rule.as_str()),
                "{rule} missing from ask: {ask:?}"
            );
        }
        assert!(
            deny.is_empty(),
            "no deny entries expected by default: {deny:?}"
        );
    }

    /// #1323 (security-audit): `codebase_memory.mcp_permissions` must
    /// actually reach the render — the docs claim parity with ICM's
    /// override mechanism, and until `CodebaseMemory` carried the field
    /// and `resolve_codebase_memory` forwarded it, `cbm.mcp_permissions`
    /// was always `None` and the 14-tool grant was unconditionally
    /// unoverridable.
    #[test]
    fn codebase_memory_mcp_permissions_override_reaches_render() {
        let manifest = crate::merge::MergedManifest {
            mcps: vec![crate::mcp::resolve::ResolvedMcp {
                name: crate::mcp::resolve::CODEBASE_MEMORY_MCP_NAME.to_string(),
                kind: crate::mcp::resolve::ResolvedKind::Stdio {
                    command: "codebase-memory-mcp".into(),
                    args: vec![],
                    env: Default::default(),
                },
                headers: Default::default(),
                timeout: None,
                disabled_tools: vec![],
                mcp_permissions: Some(crate::config::McpPermissions {
                    read_only: None,
                    mutation: Some(crate::config::McpPermissionAction::Ask),
                    destructive: None,
                }),
                wakeup_max_tokens: None,
            }],
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let ask = perm_action(&settings, "ask");

        for tool in CBM_MUTATION {
            let rule = format!("{CBM_MCP_PREFIX}{tool}");
            assert!(
                ask.contains(&rule.as_str()),
                "{rule} should be overridden to ask: {ask:?}"
            );
            assert!(
                !allow.contains(&rule.as_str()),
                "{rule} must not also be in allow: {allow:?}"
            );
        }
    }

    /// Config round-trip: `codebase_memory.mcp_permissions` is a real,
    /// deserializable field, not just a struct member nothing ever sets.
    #[test]
    fn codebase_memory_mcp_permissions_round_trips_through_yaml() {
        let yaml = "when: [proj]\nmcp_permissions:\n  destructive: deny\n";
        let cm: crate::config::CodebaseMemory = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cm.mcp_permissions,
            Some(crate::config::McpPermissions {
                read_only: None,
                mutation: None,
                destructive: Some(crate::config::McpPermissionAction::Deny),
            })
        );
    }

    fn output_style(name: &str) -> crate::config::OutputStyle {
        crate::config::OutputStyle {
            name: name.to_string(),
            description: "A test style".to_string(),
            content: "Be terse.".to_string(),
            when: Vec::new(),
            keep_coding_instructions: false,
            force_for_plugin: false,
        }
    }

    #[test]
    fn output_style_selects_when_exactly_one_active() {
        let manifest = crate::merge::MergedManifest {
            capabilities: crate::config::Capabilities {
                output_styles: vec![output_style("concise")],
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        assert_eq!(settings["outputStyle"], serde_json::json!("concise"));
    }

    #[test]
    fn output_style_no_selection_when_multiple_active() {
        let manifest = crate::merge::MergedManifest {
            capabilities: crate::config::Capabilities {
                output_styles: vec![output_style("a"), output_style("b")],
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        assert!(settings.get("outputStyle").is_none());
    }

    #[test]
    fn output_style_no_selection_when_force_for_plugin() {
        let mut style = output_style("plugin-style");
        style.force_for_plugin = true;
        let manifest = crate::merge::MergedManifest {
            capabilities: crate::config::Capabilities {
                output_styles: vec![style],
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        assert!(settings.get("outputStyle").is_none());
    }

    #[test]
    fn output_style_absent_when_none_declared() {
        let settings = render_settings_for_test(&crate::merge::MergedManifest::default());
        assert!(settings.get("outputStyle").is_none());
    }

    #[test]
    fn materialize_writes_native_output_style_file() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = crate::merge::MergedManifest {
            capabilities: crate::config::Capabilities {
                output_styles: vec![output_style("concise")],
                ..Default::default()
            },
            ..Default::default()
        };
        let owned = ClaudeCodeAdapter
            .materialize(&manifest, tmp.path())
            .unwrap();
        assert!(tmp.path().join("output-styles/concise.md").exists());
        assert!(owned.contains(&PathBuf::from("output-styles/concise.md")));
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["outputStyle"], serde_json::json!("concise"));
    }

    /// #1323 (silent-failure-hunter): a tool omitted from all three CBM tier
    /// lists gets no permission entry at all — `apply_mcp_tier_permissions`
    /// simply skips anything not named in one of the three slices, so a
    /// forgotten or duplicated entry is otherwise silent. Locks the total
    /// count and per-tool uniqueness so a future edit that drops or
    /// duplicates an entry fails loudly here instead. Doesn't catch
    /// codebase-memory-mcp itself adding a new tool upstream — that still
    /// needs a human to notice and tier it — but does catch drift within
    /// this file.
    #[test]
    fn cbm_tiers_cover_every_known_tool_exactly_once() {
        let mut seen = std::collections::HashSet::new();
        for tool in CBM_READ_ONLY
            .iter()
            .chain(CBM_MUTATION)
            .chain(CBM_DESTRUCTIVE)
        {
            assert!(seen.insert(*tool), "{tool} appears in more than one tier");
        }
        assert_eq!(
            seen.len(),
            15,
            "expected all 15 codebase-memory-mcp tools tiered exactly once, got {}: {seen:?}",
            seen.len()
        );
    }

    #[test]
    fn mcp_permissions_override_destructive_to_deny() {
        // #946: `features.context_mode.mcp_permissions` overrides the default
        // policy per tier.
        let mut manifest = context_mode_plugin_manifest();
        manifest.capabilities.features = Some(crate::config::Features {
            context_mode: Some(crate::config::ContextMode {
                enabled: true,
                mcp_permissions: Some(crate::config::McpPermissions {
                    read_only: None,
                    mutation: None,
                    destructive: Some(crate::config::McpPermissionAction::Deny),
                }),
            }),
            ..Default::default()
        });
        let settings = render_settings_for_test(&manifest);
        let ask = perm_action(&settings, "ask");
        let deny = perm_action(&settings, "deny");

        for tool in CTX_DESTRUCTIVE {
            let rule = format!("{}{tool}", crate::config::CONTEXT_MODE_MCP_PREFIX);
            assert!(
                deny.contains(&rule.as_str()),
                "{rule} missing from deny: {deny:?}"
            );
            assert!(
                !ask.contains(&rule.as_str()),
                "{rule} must not also be in ask: {ask:?}"
            );
        }
    }

    #[test]
    fn icm_mcp_permissions_override_destructive_to_deny() {
        // #946: `features.memory[].mcp_permissions` overrides the default
        // policy per tier for ICM — mirrors
        // `mcp_permissions_override_destructive_to_deny` (context-mode path)
        // but resolves the `Memory` entry through the real `resolve_mcps`
        // pipeline (Memory -> resolve_memory -> ResolvedMcp) for a genuine
        // end-to-end check, rather than hand-constructing the resolved struct.
        let host = std::collections::BTreeMap::from([(
            "still".to_string(),
            crate::config::HostEntry {
                addr: "still.local".into(),
            },
        )]);
        let memory = crate::config::Memory {
            server_host: "still".into(),
            port: 7878,
            listen_host: "127.0.0.1".into(),
            when: vec!["home".into()],
            default_topics: vec![],
            default_type: None,
            default_importance: None,
            type_importance: std::collections::BTreeMap::new(),
            retention: None,
            auto_prune: false,
            consolidation: None,
            mcp_permissions: Some(crate::config::McpPermissions {
                read_only: None,
                mutation: None,
                destructive: Some(crate::config::McpPermissionAction::Deny),
            }),
            wakeup_max_tokens: None,
        };
        let active_tags = std::collections::BTreeSet::from(["home".to_string()]);
        let resolved = crate::mcp::resolve::resolve_mcps(&[], &[memory], &host, &active_tags)
            .expect("memory entry with intersecting tags must resolve");
        assert_eq!(
            resolved.len(),
            1,
            "expected exactly the ICM entry to resolve"
        );

        let manifest = crate::merge::MergedManifest {
            mcps: resolved,
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let ask = perm_action(&settings, "ask");
        let deny = perm_action(&settings, "deny");

        for tool in ICM_DESTRUCTIVE {
            let rule = format!("mcp__icm__{tool}");
            assert!(
                deny.contains(&rule.as_str()),
                "{rule} missing from deny: {deny:?}"
            );
            assert!(
                !ask.contains(&rule.as_str()),
                "{rule} must not also be in ask: {ask:?}"
            );
        }
        // Read-only/mutation are untouched by the override — still default allow.
        for tool in ICM_READ_ONLY.iter().chain(ICM_MUTATION) {
            let rule = format!("mcp__icm__{tool}");
            assert!(
                allow.contains(&rule.as_str()),
                "{rule} missing from allow: {allow:?}"
            );
        }
    }

    #[test]
    fn mcp_tier_deny_still_outranks_unrelated_ask_and_allow() {
        // #946: the tier policy must not break the pre-existing native-wins
        // deny > ask > allow precedence (a native deny suppresses a neutral
        // allow of the same rule string) for unrelated (non-tier) rules.
        let mut manifest = context_mode_plugin_manifest();
        manifest.capabilities.permissions = crate::config::Permissions {
            allow: vec![PermissionRule {
                tool: "Bash".into(),
                pattern: Some("rm *".into()),
                paths: Vec::new(),
            }],
            ..Default::default()
        };
        let mut native_permissions = std::collections::BTreeMap::new();
        native_permissions.insert(
            "claude_code".to_string(),
            crate::config::NativePermissionRules {
                allow: Vec::new(),
                ask: Vec::new(),
                deny: vec!["Bash(rm *)".into()],
            },
        );
        manifest.capabilities.native_permissions = native_permissions;
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let deny = perm_action(&settings, "deny");
        assert!(deny.contains(&"Bash(rm *)"), "deny missing: {deny:?}");
        assert!(
            !allow.contains(&"Bash(rm *)"),
            "native deny must suppress the conflicting neutral allow: {allow:?}"
        );
        // Unrelated tier-based allow entries are unaffected.
        let ctx_read = format!(
            "{}{}",
            crate::config::CONTEXT_MODE_MCP_PREFIX,
            CTX_READ_ONLY[0]
        );
        assert!(allow.contains(&ctx_read.as_str()));
    }

    #[test]
    fn neutral_deny_suppresses_neutral_allow_for_same_rule() {
        // #1322: a tool authored directly in both `permissions.allow` and
        // `permissions.deny` (no native involvement at all) must resolve to
        // exactly one bucket, same deny > ask > allow authority as the
        // native-vs-neutral suppression above — found by
        // generate_settings_json_permission_buckets_never_overlap's proptest.
        let mut manifest = crate::merge::MergedManifest::default();
        manifest.capabilities.permissions = crate::config::Permissions {
            allow: vec![PermissionRule {
                tool: "E".into(),
                pattern: None,
                paths: Vec::new(),
            }],
            deny: vec![PermissionRule {
                tool: "E".into(),
                pattern: None,
                paths: Vec::new(),
            }],
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let deny = perm_action(&settings, "deny");
        assert!(deny.contains(&"E"), "deny missing: {deny:?}");
        assert!(
            !allow.contains(&"E"),
            "a rule denied and allowed at once must not also appear in allow: {allow:?}"
        );
    }

    // ---- #972: native-wins suppression for tiered MCP tools ----
    // Ports main's inline native-wins behavior onto release/3.x's helper
    // structure: a tiered tool's *resolved* action (override else tier
    // default) is suppressed by a more authoritative native rule on that
    // exact tool string, same deny > ask > allow authority as the unrelated
    // neutral-rule precedence already covered above.

    #[test]
    fn native_deny_suppresses_tier_allow_for_read_only_tool() {
        let mut manifest = context_mode_plugin_manifest();
        let rule = format!(
            "{}{}",
            crate::config::CONTEXT_MODE_MCP_PREFIX,
            CTX_READ_ONLY[0]
        );
        let mut native_permissions = std::collections::BTreeMap::new();
        native_permissions.insert(
            "claude_code".to_string(),
            crate::config::NativePermissionRules {
                allow: Vec::new(),
                ask: Vec::new(),
                deny: vec![rule.clone()],
            },
        );
        manifest.capabilities.native_permissions = native_permissions;
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let deny = perm_action(&settings, "deny");
        assert!(
            deny.contains(&rule.as_str()),
            "expected {rule} in deny: {deny:?}"
        );
        assert!(
            !allow.contains(&rule.as_str()),
            "native deny must suppress the tier's resolved allow: {allow:?}"
        );
        // A sibling read-only tool with no native rule is unaffected.
        let sibling = format!(
            "{}{}",
            crate::config::CONTEXT_MODE_MCP_PREFIX,
            CTX_READ_ONLY[1]
        );
        assert!(
            allow.contains(&sibling.as_str()),
            "sibling tool must still get its tier default: {allow:?}"
        );
    }

    #[test]
    fn native_ask_suppresses_tier_allow_for_mutation_tool() {
        let mut manifest = context_mode_plugin_manifest();
        let rule = format!(
            "{}{}",
            crate::config::CONTEXT_MODE_MCP_PREFIX,
            CTX_MUTATION[0]
        );
        let mut native_permissions = std::collections::BTreeMap::new();
        native_permissions.insert(
            "claude_code".to_string(),
            crate::config::NativePermissionRules {
                allow: Vec::new(),
                ask: vec![rule.clone()],
                deny: Vec::new(),
            },
        );
        manifest.capabilities.native_permissions = native_permissions;
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let ask = perm_action(&settings, "ask");
        assert!(
            ask.contains(&rule.as_str()),
            "expected {rule} in ask: {ask:?}"
        );
        assert!(
            !allow.contains(&rule.as_str()),
            "native ask must suppress the tier's resolved allow: {allow:?}"
        );
    }

    #[test]
    fn native_deny_on_destructive_tool_wins_and_is_not_double_listed() {
        let mut manifest = context_mode_plugin_manifest();
        let rule = format!(
            "{}{}",
            crate::config::CONTEXT_MODE_MCP_PREFIX,
            CTX_DESTRUCTIVE[0]
        );
        let mut native_permissions = std::collections::BTreeMap::new();
        native_permissions.insert(
            "claude_code".to_string(),
            crate::config::NativePermissionRules {
                allow: Vec::new(),
                ask: Vec::new(),
                deny: vec![rule.clone()],
            },
        );
        manifest.capabilities.native_permissions = native_permissions;
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let ask = perm_action(&settings, "ask");
        let deny = perm_action(&settings, "deny");
        assert_eq!(
            deny.iter().filter(|d| **d == rule).count(),
            1,
            "expected exactly one deny entry, not double-listed: {deny:?}"
        );
        assert!(
            !ask.contains(&rule.as_str()),
            "destructive tool's default ask must not survive alongside deny: {ask:?}"
        );
        assert!(
            !allow.contains(&rule.as_str()),
            "must not also appear in allow: {allow:?}"
        );
    }

    #[test]
    fn native_deny_wins_over_config_override_for_one_mutation_tool() {
        // `mcp_permissions` overrides mutation -> ask; a native deny on one
        // specific mutation tool must still outrank that resolved ask, while
        // its siblings (no native rule) follow the override.
        let mut manifest = context_mode_plugin_manifest();
        manifest.capabilities.features = Some(crate::config::Features {
            context_mode: Some(crate::config::ContextMode {
                enabled: true,
                mcp_permissions: Some(crate::config::McpPermissions {
                    read_only: None,
                    mutation: Some(crate::config::McpPermissionAction::Ask),
                    destructive: None,
                }),
            }),
            ..Default::default()
        });
        let denied_rule = format!(
            "{}{}",
            crate::config::CONTEXT_MODE_MCP_PREFIX,
            CTX_MUTATION[0]
        );
        let mut native_permissions = std::collections::BTreeMap::new();
        native_permissions.insert(
            "claude_code".to_string(),
            crate::config::NativePermissionRules {
                allow: Vec::new(),
                ask: Vec::new(),
                deny: vec![denied_rule.clone()],
            },
        );
        manifest.capabilities.native_permissions = native_permissions;
        let settings = render_settings_for_test(&manifest);
        let ask = perm_action(&settings, "ask");
        let deny = perm_action(&settings, "deny");
        assert!(
            deny.contains(&denied_rule.as_str()),
            "expected {denied_rule} in deny: {deny:?}"
        );
        assert!(
            !ask.contains(&denied_rule.as_str()),
            "deny-covered tool must not also be in the overridden ask: {ask:?}"
        );
        for tool in CTX_MUTATION.iter().skip(1) {
            let rule = format!("{}{tool}", crate::config::CONTEXT_MODE_MCP_PREFIX);
            assert!(
                ask.contains(&rule.as_str()),
                "{rule} missing from overridden ask: {ask:?}"
            );
        }
    }

    #[test]
    fn tool_without_native_rule_still_gets_tier_default() {
        // A native rule naming an unrelated tool must not affect tiered tools
        // it doesn't name — the common case, and a guard against an overeager
        // suppression check matching more than the exact rendered string.
        let mut manifest = context_mode_plugin_manifest();
        let mut native_permissions = std::collections::BTreeMap::new();
        native_permissions.insert(
            "claude_code".to_string(),
            crate::config::NativePermissionRules {
                allow: Vec::new(),
                ask: Vec::new(),
                deny: vec!["Bash(rm *)".into()],
            },
        );
        manifest.capabilities.native_permissions = native_permissions;
        let settings = render_settings_for_test(&manifest);
        let allow = perm_action(&settings, "allow");
        let ask = perm_action(&settings, "ask");
        for tool in CTX_READ_ONLY.iter().chain(CTX_MUTATION) {
            let rule = format!("{}{tool}", crate::config::CONTEXT_MODE_MCP_PREFIX);
            assert!(
                allow.contains(&rule.as_str()),
                "{rule} missing from allow: {allow:?}"
            );
        }
        for tool in CTX_DESTRUCTIVE {
            let rule = format!("{}{tool}", crate::config::CONTEXT_MODE_MCP_PREFIX);
            assert!(
                ask.contains(&rule.as_str()),
                "{rule} missing from ask: {ask:?}"
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

    // ---- #888: deprecated `Write` permission tool -> `Edit` ----
    // Claude Code deprecated the `Write(<path>)` rule string in favor of
    // `Edit(<path>)` (anthropics/claude-code#78817); a stale `Write` entry only
    // warns instead of matching. llmenv must rewrite it before it lands in
    // settings.json rather than handing the user that exact warning.

    #[test]
    fn neutral_write_rule_renders_as_edit() {
        let manifest = crate::merge::MergedManifest {
            capabilities: crate::config::Capabilities {
                permissions: crate::config::Permissions {
                    allow: vec![PermissionRule {
                        tool: "Write".into(),
                        pattern: Some("/foo/*".into()),
                        paths: Vec::new(),
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert!(
            allow.iter().any(|v| v == "Edit(/foo/*)"),
            "expected Edit(/foo/*) in allow, got {allow:?}"
        );
        assert!(
            !allow
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.starts_with("Write("))),
            "deprecated Write(...) rule must not reach settings.json; got {allow:?}"
        );
    }

    #[test]
    fn native_write_rule_string_normalizes_to_edit() {
        // Native rules are authored verbatim strings (not neutral PermissionRule),
        // so a user following stale docs/examples could still type "Write(...)".
        let mut native_permissions = std::collections::BTreeMap::new();
        native_permissions.insert(
            "claude_code".to_string(),
            crate::config::NativePermissionRules {
                allow: vec!["Write(/bar)".into()],
                ask: Vec::new(),
                deny: Vec::new(),
            },
        );
        let manifest = crate::merge::MergedManifest {
            capabilities: crate::config::Capabilities {
                native_permissions,
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert!(
            allow.iter().any(|v| v == "Edit(/bar)"),
            "expected Edit(/bar) in allow, got {allow:?}"
        );
        assert!(
            !allow
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.starts_with("Write("))),
            "deprecated native Write(...) rule must not reach settings.json; got {allow:?}"
        );
    }

    #[test]
    fn normalized_write_rule_dedupes_against_existing_edit_rule() {
        // A bundle could plausibly carry both the old Write(...) form and the
        // already-migrated Edit(...) form for the same path; after normalization
        // they must collapse to one entry, not appear twice.
        let mut native_permissions = std::collections::BTreeMap::new();
        native_permissions.insert(
            "claude_code".to_string(),
            crate::config::NativePermissionRules {
                allow: vec!["Write(/baz)".into(), "Edit(/baz)".into()],
                ask: Vec::new(),
                deny: Vec::new(),
            },
        );
        let manifest = crate::merge::MergedManifest {
            capabilities: crate::config::Capabilities {
                native_permissions,
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = render_settings_for_test(&manifest);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        let edit_count = allow.iter().filter(|v| *v == "Edit(/baz)").count();
        assert_eq!(
            edit_count, 1,
            "expected exactly one deduped Edit(/baz); got {allow:?}"
        );
    }

    #[test]
    fn normalize_deprecated_tool_only_matches_write_tool_name() {
        // Bare tool name and pattern form both migrate.
        assert_eq!(normalize_deprecated_tool("Write"), "Edit");
        assert_eq!(normalize_deprecated_tool("Write(/x)"), "Edit(/x)");
        // Already-current and unrelated tools/strings pass through unchanged.
        assert_eq!(normalize_deprecated_tool("Edit(/x)"), "Edit(/x)");
        assert_eq!(
            normalize_deprecated_tool("Bash(Write foo)"),
            "Bash(Write foo)"
        );
        // A tool name that merely starts with "Write" is not the deprecated tool.
        assert_eq!(normalize_deprecated_tool("WriteFile(/x)"), "WriteFile(/x)");
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
        let out = reconcile_settings(&path, fresh.clone(), None).unwrap();
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
        let out = reconcile_settings(&path, fresh, None).unwrap();
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
        let out = reconcile_settings(&path, fresh, None).unwrap();
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
        let out = reconcile_settings(&path, llmenv_hook.clone(), None).unwrap();
        let entries = out["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "identical hook deduped, not doubled");
    }

    #[test]
    fn reconcile_purges_owned_hook_removed_from_config() {
        // #991: a hook llmenv rendered last round (in prev_owned) but not this
        // round must be purged from disk, not preserved by the union.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let old = serde_json::json!({
            "hooks": { "PreToolUse": [
                { "matcher": "X", "hooks": [{ "type": "command", "command": "old-owned" }] }
            ] }
        });
        write_json(&path, &old);
        let prev_owned = old["hooks"].clone();
        let fresh = serde_json::json!({ "hooks": {} });
        let out = reconcile_settings(&path, fresh, Some(&prev_owned)).unwrap();
        let pre = out["hooks"].get("PreToolUse").and_then(|v| v.as_array());
        assert!(
            pre.is_none_or(|a| a.is_empty()),
            "owned hook removed from config must be purged: {:?}",
            out["hooks"]
        );
    }

    #[test]
    fn reconcile_preserves_foreign_hook_never_owned() {
        // #991: a hook llmenv never rendered (absent from prev_owned) is a plugin
        // self-registration and must survive even though it's absent from fresh.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        write_json(
            &path,
            &serde_json::json!({
                "hooks": { "PreToolUse": [
                    { "matcher": "X", "hooks": [{ "type": "command", "command": "foreign-plugin" }] }
                ] }
            }),
        );
        // prev_owned covered a different event/command — the foreign one was never ours.
        let prev_owned = serde_json::json!({
            "SessionStart": [{ "hooks": [{ "type": "command", "command": "llmenv-x" }] }]
        });
        let fresh = serde_json::json!({ "hooks": {} });
        let out = reconcile_settings(&path, fresh, Some(&prev_owned)).unwrap();
        let cmds: Vec<&str> = out["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["hooks"][0]["command"].as_str())
            .collect();
        assert!(
            cmds.contains(&"foreign-plugin"),
            "foreign (never-owned) hook must be preserved: {:?}",
            out["hooks"]
        );
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
        let out = reconcile_settings(&path, fresh, None).unwrap();
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
        let out = reconcile_settings(&path, fresh, None).unwrap();
        let entries = out["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "nulls at any depth stripped before dedup");
    }

    use llmenv_util::testkit::arb_json;

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
        let out = reconcile_settings(&path, fresh, None).unwrap();
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
        let out = reconcile_settings(&path, fresh.clone(), None).unwrap();
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
        let out = reconcile_settings(&path, fresh, None).unwrap();
        assert_eq!(
            out["statusLine"]["command"], "my-status-script",
            "native passthrough key must survive re-render"
        );
        assert_eq!(out["cleanupPeriodDays"], 365);
    }

    // ---- reconcile_settings (#719): property-based invariants ----

    // Reuses `llmenv_util::testkit::arb_json()` (bounded, null-bearing
    // recursive JSON, #1281) for the on-disk `existing` side.

    // The `fresh` side of reconcile is always a genuine llmenv render, not
    // arbitrary JSON. Deriving it from a real manifest keeps the properties
    // honest: reconcile's hooks-merge/dedup and null-stripping are designed for
    // the exact `{ Event: [{matcher, hooks:[...]}] }` shape a render produces,
    // and arbitrary "hooks" values (e.g. `{"a":[true]}`) exercise paths that
    // the function never actually sees.
    fn arb_fresh_render() -> impl Strategy<Value = serde_json::Value> {
        arb_merged_manifest().prop_map(|m| render_settings_for_test(&m))
    }

    proptest! {
        // Determinism: reconcile is a pure function of (on-disk bytes, fresh) —
        // the same inputs produce the same output every time.
        #[test]
        fn reconcile_is_deterministic(existing in arb_json(), fresh in arb_fresh_render()) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("settings.json");
            write_json(&path, &existing);
            let a = reconcile_settings(&path, fresh.clone(), None).unwrap();
            let b = reconcile_settings(&path, fresh.clone(), None).unwrap();
            prop_assert_eq!(a, b);
        }

        // Idempotency: reconciling against the result of a previous reconcile
        // (same fresh) yields that same result — re-renders converge.
        #[test]
        fn reconcile_is_idempotent(existing in arb_json(), fresh in arb_fresh_render()) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("settings.json");
            write_json(&path, &existing);
            let once = reconcile_settings(&path, fresh.clone(), None).unwrap();
            write_json(&path, &once);
            let twice = reconcile_settings(&path, fresh, None).unwrap();
            prop_assert_eq!(once, twice);
        }

        // Fresh wins: every owned key (except `hooks`, which is unioned) present
        // in the fresh render appears verbatim in the output — a stale on-disk
        // value never survives for an authoritative owned key.
        #[test]
        fn reconcile_fresh_wins_for_owned_non_hook_keys(
            existing in arb_json(),
            fresh in arb_fresh_render(),
        ) {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("settings.json");
            write_json(&path, &existing);
            let out = reconcile_settings(&path, fresh.clone(), None).unwrap();
            let fresh_obj = fresh.as_object().unwrap();
            for key in LLMENV_OWNED_SETTINGS_KEYS {
                if key == "hooks" {
                    continue; // unioned, not replaced
                }
                if let Some(fresh_val) = fresh_obj.get(key) {
                    prop_assert_eq!(
                        out.get(key),
                        Some(fresh_val),
                        "owned key {:?} did not take the fresh value",
                        key
                    );
                }
            }
        }

        // Hooks are unioned (not replaced): a foreign hook entry present on disk
        // survives a re-render, and llmenv's own re-rendered entries are deduped
        // rather than accumulating. Every render emits SessionStart entries, so
        // that event is always present to union against.
        #[test]
        fn reconcile_unions_hooks_preserving_foreign_and_deduping_own(
            manifest in arb_merged_manifest(),
            foreign_cmd in "[a-z]{3,12}",
        ) {
            let fresh = render_settings_for_test(&manifest);
            let foreign = serde_json::json!({
                "hooks": [{ "type": "command", "command": foreign_cmd }]
            });
            // Simulate the on-disk file: llmenv's own last render plus a
            // plugin-registered foreign SessionStart entry.
            let mut existing = fresh.clone();
            existing["hooks"]["SessionStart"]
                .as_array_mut()
                .unwrap()
                .push(foreign.clone());
            let expected_len = existing["hooks"]["SessionStart"].as_array().unwrap().len();

            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("settings.json");
            write_json(&path, &existing);
            let out = reconcile_settings(&path, fresh, None).unwrap();

            let session_start = out["hooks"]["SessionStart"].as_array().unwrap();
            prop_assert!(
                session_start.contains(&foreign),
                "foreign hook entry was dropped by the union"
            );
            prop_assert_eq!(
                session_start.len(),
                expected_len,
                "re-rendered llmenv entries must dedup, not accumulate"
            );
        }
    }

    // ---- generate_settings_json (#720): property-based invariants ----

    use crate::adapter::skills::{arb_hook, arb_permission_rule};

    /// Top-level `settings.json` keys `generate_settings_json` renders that the
    /// catch-all `native` block is allowed to collide with. `permissions` and
    /// `hooks` are excluded on purpose — `reject_modeled_keys_in_catch_all`
    /// refuses those, so generating one would make the render error out instead
    /// of exercising the overlay.
    const OVERRIDABLE_SETTINGS_KEYS: [&str; 4] = [
        "autoMemoryEnabled",
        "effortLevel",
        "advisorSize",
        "outputStyle",
    ];

    /// Arbitrary `native.claude_code` catch-all fragment. Keys are drawn from
    /// both fresh names (the insert path) and the keys the renderer actually
    /// emits (the shared-key overwrite path) — the renderer emits those before
    /// the overlay specifically so `native` can override them, so the overwrite
    /// path is reachable in production and must be covered.
    ///
    /// Null values are generated freely, including on colliding keys: since
    /// #1264 a native `null` deletes the key, so the render upholds #720's
    /// no-null-valued-keys invariant either way.
    ///
    /// The null-on-a-colliding-key pair gets its own generator arm rather than
    /// relying on `arb_yaml_value` happening to produce a top-level null. That
    /// is the exact shape #1264 regressed on, and `prop_recursive` favours
    /// container recursion over leaf termination hard enough that it otherwise
    /// lands in roughly 1% of fragments — a handful of hits per default
    /// 256-case run, with enough variance to miss the regression outright.
    fn arb_native_fragment()
    -> impl Strategy<Value = std::collections::BTreeMap<String, serde_yaml::Value>> {
        let overridable = || {
            proptest::sample::select(OVERRIDABLE_SETTINGS_KEYS.as_slice()).prop_map(String::from)
        };
        let pair = prop_oneof![
            2 => ("[a-z]{1,8}".prop_map(String::from), arb_yaml_value(2)),
            2 => (overridable(), arb_yaml_value(2)),
            1 => (overridable(), Just(serde_yaml::Value::Null)),
        ];
        proptest::collection::vec(pair, 0..4).prop_map(|pairs| {
            let mut fragment = serde_yaml::Mapping::new();
            for (k, v) in pairs {
                if MODELED_SETTINGS_KEYS.contains(&k.as_str()) {
                    continue;
                }
                fragment.insert(serde_yaml::Value::String(k), v);
            }
            std::collections::BTreeMap::from([(
                "claude_code".to_owned(),
                serde_yaml::Value::Mapping(fragment),
            )])
        })
    }

    fn arb_merged_manifest() -> impl Strategy<Value = crate::merge::MergedManifest> {
        (
            proptest::collection::vec(arb_permission_rule(), 0..4),
            proptest::collection::vec(arb_permission_rule(), 0..4),
            proptest::collection::vec(arb_hook(), 0..4),
            any::<bool>(),
            arb_native_fragment(),
            proptest::option::of(any::<bool>()),
            proptest::option::of("[a-z]{1,8}"),
            proptest::option::of("[a-z]{1,8}"),
        )
            .prop_map(
                |(
                    allow,
                    deny,
                    hooks,
                    transcript_on,
                    native,
                    auto_memory_enabled,
                    effort_level,
                    advisor_size,
                )| crate::merge::MergedManifest {
                    capabilities: crate::config::Capabilities {
                        permissions: crate::config::Permissions {
                            default_mode: None,
                            preset: None,
                            allow,
                            ask: Vec::new(),
                            deny,
                        },
                        hooks,
                        auto_memory_enabled,
                        effort_level,
                        advisor_size,
                        ..Default::default()
                    },
                    session_log: crate::config::SessionLog {
                        transcript: Some(crate::config::TranscriptSinkConfig {
                            enabled: transcript_on,
                            level: crate::config::LogLevel::Info,
                            retention_days: None,
                        }),
                        ..Default::default()
                    },
                    native,
                    ..Default::default()
                },
            )
    }

    // Recursively assert no object anywhere in `v` carries a null-valued key.
    fn assert_no_null_keys(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(map) => map
                .values()
                .all(|child| !child.is_null() && assert_no_null_keys(child)),
            serde_json::Value::Array(items) => items.iter().all(assert_no_null_keys),
            _ => true,
        }
    }

    fn write_settings_bytes(manifest: &crate::merge::MergedManifest) -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        generate_settings_json(tmp.path(), manifest).unwrap();
        std::fs::read(tmp.path().join("settings.json")).unwrap()
    }

    proptest! {
        // The written settings.json is always valid, re-parseable JSON.
        #[test]
        fn generate_settings_json_is_valid_json(manifest in arb_merged_manifest()) {
            let bytes = write_settings_bytes(&manifest);
            let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let reserialized = serde_json::to_vec(&parsed).unwrap();
            let reparsed: serde_json::Value = serde_json::from_slice(&reserialized).unwrap();
            prop_assert_eq!(parsed, reparsed);
        }

        // #699 core invariant: no hook handler (indeed no object anywhere in the
        // rendered settings) contains a null-valued key.
        #[test]
        fn generate_settings_json_has_no_null_valued_keys(manifest in arb_merged_manifest()) {
            let settings = render_settings_for_test(&manifest);
            prop_assert!(
                assert_no_null_keys(&settings),
                "settings.json contains a null-valued key: {settings}"
            );
        }

        // Determinism: the same manifest renders byte-identical settings.json.
        #[test]
        fn generate_settings_json_is_deterministic(manifest in arb_merged_manifest()) {
            prop_assert_eq!(write_settings_bytes(&manifest), write_settings_bytes(&manifest));
        }

        // #947 regression guard: no rendered permission string appears in more
        // than one of allow/ask/deny at once. Claude Code resolves deny > ask >
        // allow, so a string present in two buckets is always a silent
        // self-shadow — one of the two entries can never fire. `render_action`'s
        // suppression logic (#946/#972) exists specifically to prevent this;
        // this test locks the invariant in so a future change to that logic
        // can't quietly reintroduce it.
        #[test]
        fn generate_settings_json_permission_buckets_never_overlap(manifest in arb_merged_manifest()) {
            let settings = render_settings_for_test(&manifest);
            let bucket = |name: &str| -> std::collections::BTreeSet<String> {
                settings["permissions"][name]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .map(str::to_owned)
                    .collect()
            };
            let (allow, ask, deny) = (bucket("allow"), bucket("ask"), bucket("deny"));
            prop_assert!(
                allow.is_disjoint(&ask),
                "allow/ask overlap: {:?}", allow.intersection(&ask).collect::<Vec<_>>()
            );
            prop_assert!(
                allow.is_disjoint(&deny),
                "allow/deny overlap: {:?}", allow.intersection(&deny).collect::<Vec<_>>()
            );
            prop_assert!(
                ask.is_disjoint(&deny),
                "ask/deny overlap: {:?}", ask.intersection(&deny).collect::<Vec<_>>()
            );
        }
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
            mcp_permissions: None,
            wakeup_max_tokens: None,
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
            mcp_permissions: None,
            wakeup_max_tokens: None,
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

    /// #1270: a native `null` on a key llmenv already rendered into
    /// `.claude.json` must delete the key rather than persist an explicit
    /// JSON null, mirroring #1264's fix for `settings.json`. This is the
    /// highest-priority write path from #1270 — `.claude.json` is real,
    /// persistent user state, not a rebuildable cache folder, so a stray
    /// null here survives across renders instead of being rebuilt away.
    #[test]
    fn merge_mcp_native_null_removes_a_rendered_server_key() {
        let tmp = tempfile::tempdir().unwrap();
        let native: serde_yaml::Value =
            serde_yaml::from_str("mcpServers:\n  icm:\n    command: null\n").unwrap();
        merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("icm", "icm-bin")], Some(&native))
            .unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tmp.path().join(CLAUDE_JSON_FILE)).unwrap())
                .unwrap();
        assert!(
            doc["mcpServers"]["icm"].get("command").is_none(),
            "`native_mcp.claude_code.mcpServers.icm.command: null` must delete \
             the key, got: {doc}"
        );
    }

    /// #1270 follow-up: the preserve-runtime-subkeys loop below the strip must
    /// not resurrect a key the native fragment explicitly nulled. Before this
    /// fix, stripping the null off the scratch `doc` also erased the "this key
    /// was explicitly deleted" signal the loop checks via `contains_key`, so a
    /// credential-purge null on `env`/`headers` silently restored the stale
    /// on-disk value on every re-render — the exact case the #1270 changelog
    /// entry claims is fixed. Uses a non-empty `env` so llmenv's own render
    /// emits the key (an empty env renders no key at all either way).
    #[test]
    fn merge_mcp_native_null_does_not_resurrect_preserved_subkey() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_FILE);
        write_json(
            &path,
            &serde_json::json!({
                "mcpServers": { "icm": { "command": "icm-bin", "env": { "TOKEN": "leaked-old-value" } } }
            }),
        );
        let mcp = ResolvedMcp {
            name: "icm".into(),
            kind: ResolvedKind::Stdio {
                command: "icm-bin".into(),
                args: vec![],
                env: std::collections::BTreeMap::from([("TOKEN".into(), "resolved-value".into())]),
            },
            headers: std::collections::BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            wakeup_max_tokens: None,
        };
        let native: serde_yaml::Value =
            serde_yaml::from_str("mcpServers:\n  icm:\n    env: null\n").unwrap();
        merge_mcp_into_claude_json(tmp.path(), &[mcp], Some(&native)).unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            doc["mcpServers"]["icm"].get("env").is_none(),
            "`native_mcp.claude_code.mcpServers.icm.env: null` must delete the \
             key, not resurrect the on-disk value, got: {doc}"
        );
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

    #[test]
    fn merge_mcp_preserves_existing_server_sub_keys() {
        // #814: when an llmenv-managed server entry already exists in
        // .claude.json with runtime-added sub-keys (auth tokens, etc.), a
        // re-materialization must preserve those keys alongside the fresh
        // config-driven keys.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CLAUDE_JSON_FILE);
        // Pre-existing entry for "icm" with an auth block Claude Code added.
        write_json(
            &path,
            &serde_json::json!({
                "mcpServers": {
                    "icm": {
                        "command": "icm-bin-old",
                        "auth": { "token": "abc123" }
                    }
                }
            }),
        );
        // Re-materialize with a fresh entry that only has the command.
        merge_mcp_into_claude_json(tmp.path(), &[stdio_mcp("icm", "icm-bin")], None).unwrap();

        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        // Command is the fresh value.
        assert_eq!(doc["mcpServers"]["icm"]["command"], "icm-bin");
        // Auth is preserved from the existing entry.
        assert_eq!(doc["mcpServers"]["icm"]["auth"]["token"], "abc123");
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

    #[test]
    fn seed_status_line_seeds_default_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings.json");
        std::fs::write(
            &settings,
            serde_json::json!({ "otherKey": "value" }).to_string(),
        )
        .unwrap();

        seed_status_line(tmp.path()).unwrap();

        let content = std::fs::read_to_string(&settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["statusLine"]["type"], "command");
        assert_eq!(
            json["statusLine"]["command"],
            "llmenv statusline --color always"
        );
        assert_eq!(json["otherKey"], "value");
    }

    #[test]
    fn seed_status_line_skips_when_already_present() {
        // Proves the no-stomp property: whatever is already on disk — whether a
        // user's own `/statusline` customization, a native override llmenv wrote
        // on a prior render, or any other tool's value — must survive untouched.
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("settings.json");
        let existing = serde_json::json!({
            "statusLine": { "type": "command", "command": "my-custom-script" },
            "otherKey": "value"
        });
        std::fs::write(&settings, existing.to_string()).unwrap();

        seed_status_line(tmp.path()).unwrap();

        let content = std::fs::read_to_string(&settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["statusLine"]["command"], "my-custom-script");
        assert_eq!(json["otherKey"], "value");
    }

    #[test]
    fn seed_status_line_noop_when_settings_file_absent() {
        // materialize hasn't run yet / adapter isn't Claude Code — must not error
        // or create a seeded-only file (mirrors seed_install_method's contract).
        let tmp = tempfile::tempdir().unwrap();
        seed_status_line(tmp.path()).unwrap();
        assert!(!tmp.path().join("settings.json").exists());
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

        let merged =
            reconcile_settings(&path, fresh, None).expect("reconcile_settings should succeed");
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

    /// #741: doctor and the settings generator must agree on which lifecycle
    /// hooks are wired, so the gate lives in one function both consult.
    // #317: same phantom-layer risk as self_critique, one event over. The
    // digest runs on UserPromptSubmit, which nothing else registers in this
    // fixture, so without the registration the layer would be config-only.
    #[test]
    fn slippage_rule_reinjection_alone_registers_user_prompt_submit() {
        let mut m = crate::merge::MergedManifest::default();
        m.session_log.file = None;
        m.session_log.transcript = None;
        m.capabilities.features = Some(llmenv_config::Features {
            slippage: Some(llmenv_config::SlippageControl {
                enabled: true,
                rule_reinjection: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let settings = render_settings_for_test(&m);
        assert!(
            hook_commands_for(&settings, "UserPromptSubmit")
                .iter()
                .any(|c| c.ends_with(" user_prompt_submit")),
            "got {:?}",
            hook_commands_for(&settings, "UserPromptSubmit")
        );

        // With the layer off nothing else pulls the event in, so the
        // assertion above can't be passing for an unrelated reason.
        let mut off = m.clone();
        off.capabilities.features = Some(llmenv_config::Features {
            slippage: Some(llmenv_config::SlippageControl::default()),
            ..Default::default()
        });
        assert!(
            hook_commands_for(&render_settings_for_test(&off), "UserPromptSubmit").is_empty(),
            "nothing but the layer should register UserPromptSubmit here"
        );
    }

    // #317: the consistency test above compares the gate to what's generated,
    // so it passes whenever the two agree — including when they agree that
    // nothing is registered. This asserts the behaviour directly: enabling
    // self_critique, with neither session logging nor the task tracker, must
    // put a Stop hook in settings.json, or the layer never runs.
    #[test]
    fn slippage_self_critique_alone_registers_the_stop_hook() {
        let mut m = crate::merge::MergedManifest::default();
        m.session_log.file = None;
        m.session_log.transcript = None;
        m.capabilities.features = Some(llmenv_config::Features {
            slippage: Some(llmenv_config::SlippageControl {
                enabled: true,
                self_critique: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let settings = render_settings_for_test(&m);
        assert!(
            hook_commands_for(&settings, "Stop")
                .iter()
                .any(|c| c.ends_with(" stop")),
            "self_critique runs on Stop, so Stop must be registered: {:?}",
            hook_commands_for(&settings, "Stop")
        );

        // And with the layer off, nothing else pulls Stop in — otherwise the
        // assertion above would pass for the wrong reason.
        let mut off = m.clone();
        off.capabilities.features = Some(llmenv_config::Features {
            slippage: Some(llmenv_config::SlippageControl::default()),
            ..Default::default()
        });
        assert!(
            hook_commands_for(&render_settings_for_test(&off), "Stop").is_empty(),
            "nothing but the layer should be registering Stop in this fixture"
        );
    }

    #[test]
    fn lifecycle_registrations_match_the_generated_settings() {
        for (label, manifest) in [
            ("bare", crate::merge::MergedManifest::default()),
            (
                "with memory",
                crate::merge::MergedManifest {
                    mcps: vec![crate::mcp::resolve::ResolvedMcp {
                        name: crate::mcp::resolve::MEMORY_MCP_NAME.to_string(),
                        kind: crate::mcp::resolve::ResolvedKind::Remote {
                            url: "http://localhost:9999".into(),
                            transport: crate::config::McpTransport::Http,
                        },
                        headers: Default::default(),
                        timeout: None,
                        disabled_tools: vec![],
                        mcp_permissions: None,
                        wakeup_max_tokens: None,
                    }],
                    ..Default::default()
                },
            ),
            // `stop`'s gate is the one still derived independently by the
            // generator, so both of its enabling paths need covering.
            // Session logging is on in a default manifest, so `stop` is
            // registered either way there — these two isolate each half of its
            // condition, which is what makes the assertion able to fail.
            ("task tracker, no session log", {
                let mut m = crate::merge::MergedManifest::default();
                m.session_log.file = None;
                m.session_log.transcript = None;
                m.capabilities.features = Some(llmenv_config::Features {
                    task_tracker: Some(llmenv_config::TaskTracker {
                        enabled: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                m
            }),
            // #317: self_critique is a third path to a registered `stop`. A
            // fixture with neither of the other two proves the layer isn't a
            // phantom — config accepted, hook never registered, nothing fires.
            ("slippage self_critique only", {
                let mut m = crate::merge::MergedManifest::default();
                m.session_log.file = None;
                m.session_log.transcript = None;
                m.capabilities.features = Some(llmenv_config::Features {
                    slippage: Some(llmenv_config::SlippageControl {
                        enabled: true,
                        self_critique: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                m
            }),
            ("neither session log nor task tracker", {
                let mut m = crate::merge::MergedManifest::default();
                m.session_log.file = None;
                m.session_log.transcript = None;
                m
            }),
        ] {
            let registrations = super::lifecycle_hook_registrations(&manifest);
            let settings: serde_json::Value =
                serde_json::from_slice(&write_settings_bytes(&manifest)).unwrap();
            let commands: Vec<String> = settings["hooks"]
                .as_object()
                .into_iter()
                .flat_map(|events| events.values())
                .flat_map(|entries| entries.as_array().cloned().unwrap_or_default())
                .flat_map(|entry| entry["hooks"].as_array().cloned().unwrap_or_default())
                .filter_map(|h| h["command"].as_str().map(str::to_owned))
                .collect();

            for (event, registered, why) in registrations {
                // Space-delimited: `ends_with("stop")` also matches
                // `subagent_stop`, which silently made this assertion pass on
                // the wrong hook.
                let suffix = format!(" {event}");
                let present = commands
                    .iter()
                    .any(|c| c.contains("hook-run") && c.ends_with(&suffix));
                assert_eq!(
                    present,
                    registered,
                    "{label}: doctor says {event} registered={registered} ({why}) but \
                     settings.json {}: {commands:?}",
                    if present { "has it" } else { "does not" }
                );
            }
        }
    }

    /// #741: the drift check now runs inside `hook-run session_start` rather
    /// than from its own hook, so session start spawns one `llmenv` process
    /// instead of two that each re-parse the config.
    #[test]
    fn session_start_registers_one_llmenv_command_not_a_separate_stale_check() {
        let manifest = crate::merge::MergedManifest::default();
        let bytes = write_settings_bytes(&manifest);
        let settings: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let commands: Vec<String> = settings["hooks"]["SessionStart"]
            .as_array()
            .expect("SessionStart hooks are always registered")
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|h| h["command"].as_str().map(str::to_owned))
            .collect();

        assert!(
            !commands.iter().any(|c| c.contains("check-stale")),
            "check-stale must not be registered separately any more: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.contains("hook-run") && c.ends_with("session_start")),
            "hook-run session_start carries the drift check now: {commands:?}"
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

    // #1341: a symlink already present at the destination is never
    // legitimate — llmenv owns everything under the materialized output.
    #[cfg(unix)]
    // #1066: the source root is now opened once, with O_NOFOLLOW, and the
    // walk descends from that fd. A symlink standing where the source
    // directory should be therefore fails instead of being copied through —
    // the path-based version resolved it happily.
    #[test]
    fn copy_dir_owner_only_refuses_a_symlinked_source_root() {
        let src_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        let real = src_tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("SKILL.md"), b"x").unwrap();
        let link = src_tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = super::copy_dir_owner_only(&link, &out_tmp.path().join("dest")).unwrap_err();
        assert!(
            format!("{err:#}").contains("opening source directory"),
            "expected the source open to fail, got {err:#}"
        );
    }

    #[test]
    fn copy_dir_owner_only_rejects_symlinked_destination() {
        let src_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src_tmp.path().join("my-skill")).unwrap();
        std::fs::write(src_tmp.path().join("my-skill/SKILL.md"), VALID_FRONTMATTER).unwrap();

        let elsewhere = tempfile::tempdir().unwrap();
        let dest = out_tmp.path().join("skills").join("my-skill");
        std::fs::create_dir_all(out_tmp.path().join("skills")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &dest).unwrap();

        let skill = crate::config::SkillSource {
            name: "my-skill".into(),
            path: src_tmp.path().join("my-skill").to_str().unwrap().into(),
            when: Vec::new(),
        };
        let err = crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap_err();
        assert!(err.to_string().contains("symlink"), "got: {err}");
        assert!(!elsewhere.path().join("SKILL.md").exists());
    }

    // #1341: a symlinked SKILL.md is otherwise silently dropped by the
    // inside-tree skip, producing a skill dir that only fails validation
    // later with a misleading "missing SKILL.md" error.
    #[cfg(unix)]
    #[test]
    fn copy_dir_owner_only_rejects_symlinked_skill_md() {
        let src_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        let skill_src = src_tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_src).unwrap();
        let real_md = src_tmp.path().join("real-SKILL.md");
        std::fs::write(&real_md, VALID_FRONTMATTER).unwrap();
        std::os::unix::fs::symlink(&real_md, skill_src.join("SKILL.md")).unwrap();

        let skill = crate::config::SkillSource {
            name: "my-skill".into(),
            path: skill_src.to_str().unwrap().into(),
            when: Vec::new(),
        };
        let err = crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap_err();
        assert!(err.to_string().contains("SKILL.md"), "got: {err}");
        assert!(err.to_string().contains("symlink"), "got: {err}");
    }

    // A symlinked entry that is *not* SKILL.md is skipped, not fatal — the
    // rest of the skill (including SKILL.md itself) still materializes.
    #[cfg(unix)]
    #[test]
    fn copy_dir_owner_only_skips_non_skill_md_symlink() {
        let src_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        let skill_src = src_tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_src).unwrap();
        std::fs::write(skill_src.join("SKILL.md"), VALID_FRONTMATTER).unwrap();
        let real_ref = src_tmp.path().join("real-reference.md");
        std::fs::write(&real_ref, "reference content").unwrap();
        std::os::unix::fs::symlink(&real_ref, skill_src.join("reference.md")).unwrap();

        let skill = crate::config::SkillSource {
            name: "my-skill".into(),
            path: skill_src.to_str().unwrap().into(),
            when: Vec::new(),
        };
        crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap();
        assert!(out_tmp.path().join("skills/my-skill/SKILL.md").exists());
        assert!(!out_tmp.path().join("skills/my-skill/reference.md").exists());
    }

    // #1341 security-audit: a nested SKILL.md (not the skill's own root
    // manifest — a vendored sub-skill, an example file) is skipped like any
    // other symlink, not fatal for the whole render.
    #[cfg(unix)]
    #[test]
    fn copy_dir_owner_only_nested_symlinked_skill_md_is_not_fatal() {
        let src_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        let skill_src = src_tmp.path().join("my-skill");
        std::fs::create_dir_all(skill_src.join("examples")).unwrap();
        std::fs::write(skill_src.join("SKILL.md"), VALID_FRONTMATTER).unwrap();
        let real_md = src_tmp.path().join("real-example-SKILL.md");
        std::fs::write(&real_md, "example").unwrap();
        std::os::unix::fs::symlink(&real_md, skill_src.join("examples/SKILL.md")).unwrap();

        let skill = crate::config::SkillSource {
            name: "my-skill".into(),
            path: skill_src.to_str().unwrap().into(),
            when: Vec::new(),
        };
        crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap();
        assert!(out_tmp.path().join("skills/my-skill/SKILL.md").exists());
        assert!(
            !out_tmp
                .path()
                .join("skills/my-skill/examples/SKILL.md")
                .exists()
        );
    }

    // #1341 security-audit: case-insensitive match closes the gap a
    // case-insensitive filesystem (macOS default) would otherwise leave —
    // `skill.md` must be caught the same as `SKILL.md`.
    #[cfg(unix)]
    #[test]
    fn copy_dir_owner_only_rejects_symlinked_skill_md_case_insensitive() {
        let src_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        let skill_src = src_tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_src).unwrap();
        let real_md = src_tmp.path().join("real-skill.md");
        std::fs::write(&real_md, VALID_FRONTMATTER).unwrap();
        std::os::unix::fs::symlink(&real_md, skill_src.join("skill.md")).unwrap();

        let skill = crate::config::SkillSource {
            name: "my-skill".into(),
            path: skill_src.to_str().unwrap().into(),
            when: Vec::new(),
        };
        let err = crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap_err();
        assert!(err.to_string().contains("symlink"), "got: {err}");
    }

    // #1341 security-audit: write_owner_only follows a symlink at its
    // destination and overwrites the target; a symlink planted at a
    // destination *file* path (not just a directory) must be rejected too.
    #[cfg(unix)]
    #[test]
    fn copy_dir_owner_only_rejects_symlinked_destination_file() {
        let src_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        let skill_src = src_tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_src).unwrap();
        std::fs::write(skill_src.join("SKILL.md"), VALID_FRONTMATTER).unwrap();

        let victim = tempfile::tempdir().unwrap();
        let victim_file = victim.path().join("victim.txt");
        std::fs::write(&victim_file, "do not overwrite me").unwrap();
        let dest_skill_dir = out_tmp.path().join("skills/my-skill");
        std::fs::create_dir_all(&dest_skill_dir).unwrap();
        std::os::unix::fs::symlink(&victim_file, dest_skill_dir.join("SKILL.md")).unwrap();

        let skill = crate::config::SkillSource {
            name: "my-skill".into(),
            path: skill_src.to_str().unwrap().into(),
            when: Vec::new(),
        };
        let err = crate::adapter::skills::write_first_class_skills(
            out_tmp.path(),
            std::slice::from_ref(&skill),
        )
        .unwrap_err();
        assert!(err.to_string().contains("symlink"), "got: {err}");
        assert_eq!(
            std::fs::read_to_string(&victim_file).unwrap(),
            "do not overwrite me",
            "victim file must not be overwritten through the symlink"
        );
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

        let (owned, names) =
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
        assert_eq!(names, vec!["my-plugin-skill".to_string()]);
    }

    #[test]
    fn project_plugin_skills_no_skills_dir_returns_empty() {
        let plugin_tmp = tempfile::tempdir().unwrap();
        let out_tmp = tempfile::tempdir().unwrap();
        // No skills/ subdir in the plugin.
        let (owned, names) =
            crate::adapter::skills::project_plugin_skills(plugin_tmp.path(), out_tmp.path())
                .unwrap();
        assert!(owned.is_empty());
        assert!(names.is_empty());
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

    // ---- PermissionMode -> string mapping ----

    #[test]
    fn permission_mode_str_maps_all_variants() {
        use crate::config::PermissionMode;
        for (mode, expected) in [
            (PermissionMode::AcceptEdits, "acceptEdits"),
            (PermissionMode::Plan, "plan"),
            (PermissionMode::Default, "default"),
            (PermissionMode::BypassPermissions, "bypassPermissions"),
            (PermissionMode::Auto, "auto"),
            (PermissionMode::DontAsk, "dontAsk"),
            (PermissionMode::Manual, "manual"),
        ] {
            assert_eq!(
                permission_mode_str(mode),
                expected,
                "permission_mode_str({mode:?})"
            );
        }
    }

    // ------------------------------------------------------------------
    // #801: Coverage detection — ensure every known ICM/ctx tool has a
    // permission-tier entry. When a new tool is added to the MCP server
    // or plugin, update the snapshot below AND add it to the matching
    // `*_READ_ONLY` / `*_MUTATION` / `*_DESTRUCTIVE` const array above.
    // ------------------------------------------------------------------

    /// Snapshot of every tool exported by the ICM MCP server (server-side
    /// name, without the `mcp__icm__` prefix that Claude Code applies).
    const ALL_KNOWN_ICM_TOOLS: &[&str] = &[
        // READ_ONLY (16)
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
        "icm_memoir_export",
        "icm_memoir_list",
        // MUTATION (13)
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
        "icm_memoir_refine",
        "icm_memoir_link",
        // DESTRUCTIVE (2)
        "icm_memory_forget",
        "icm_memory_forget_topic",
    ];

    /// Snapshot of every tool exported by the context-mode plugin (without
    /// any common prefix — names match `CTX_*` arrays directly).
    const ALL_KNOWN_CTX_TOOLS: &[&str] = &[
        // READ_ONLY (4)
        "ctx_search",
        "ctx_stats",
        "ctx_doctor",
        "ctx_insight",
        // MUTATION (5)
        "ctx_index",
        "ctx_execute",
        "ctx_execute_file",
        "ctx_fetch_and_index",
        "ctx_batch_execute",
        // DESTRUCTIVE (2)
        "ctx_purge",
        "ctx_upgrade",
    ];

    #[test]
    fn icm_tool_tiers_cover_all_known_tools() {
        let ro: std::collections::BTreeSet<&str> = ICM_READ_ONLY.iter().copied().collect();
        let mutation: std::collections::BTreeSet<&str> = ICM_MUTATION.iter().copied().collect();
        let dest: std::collections::BTreeSet<&str> = ICM_DESTRUCTIVE.iter().copied().collect();

        // No tool appears in more than one tier.
        for &tier in &[&ro, &mutation, &dest] {
            let dupes: Vec<_> = {
                let others: [&std::collections::BTreeSet<&str>; 2] = if std::ptr::eq(tier, &ro) {
                    [&mutation, &dest]
                } else if std::ptr::eq(tier, &mutation) {
                    [&ro, &dest]
                } else {
                    [&ro, &mutation]
                };
                tier.iter()
                    .filter(|t| others[0].contains(*t) || others[1].contains(*t))
                    .copied()
                    .collect()
            };
            assert!(dupes.is_empty(), "ICM tool(s) in multiple tiers: {dupes:?}");
        }

        let mut covered: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        covered.extend(&ro);
        covered.extend(&mutation);
        covered.extend(&dest);
        let all: std::collections::BTreeSet<&str> = ALL_KNOWN_ICM_TOOLS.iter().copied().collect();

        let uncovered: Vec<_> = all.difference(&covered).copied().collect();
        assert!(
            uncovered.is_empty(),
            "ICM tool(s) in ALL_KNOWN_ICM_TOOLS but not in any tier array: {uncovered:?}\n\
             Add each tool to the correct ICM_* array above."
        );

        let extras: Vec<_> = covered.difference(&all).copied().collect();
        assert!(
            extras.is_empty(),
            "ICM tool(s) in tier arrays but not in ALL_KNOWN_ICM_TOOLS: {extras:?}\n\
             Either remove the stale entry or add the tool to ALL_KNOWN_ICM_TOOLS."
        );
    }

    #[test]
    fn ctx_tool_tiers_cover_all_known_tools() {
        let ro: std::collections::BTreeSet<&str> = CTX_READ_ONLY.iter().copied().collect();
        let mutation: std::collections::BTreeSet<&str> = CTX_MUTATION.iter().copied().collect();
        let dest: std::collections::BTreeSet<&str> = CTX_DESTRUCTIVE.iter().copied().collect();

        // No tool appears in more than one tier.
        for &tier in &[&ro, &mutation, &dest] {
            let dupes: Vec<_> = {
                let others: [&std::collections::BTreeSet<&str>; 2] = if std::ptr::eq(tier, &ro) {
                    [&mutation, &dest]
                } else if std::ptr::eq(tier, &mutation) {
                    [&ro, &dest]
                } else {
                    [&ro, &mutation]
                };
                tier.iter()
                    .filter(|t| others[0].contains(*t) || others[1].contains(*t))
                    .copied()
                    .collect()
            };
            assert!(dupes.is_empty(), "CTX tool(s) in multiple tiers: {dupes:?}");
        }

        let mut covered: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        covered.extend(&ro);
        covered.extend(&mutation);
        covered.extend(&dest);
        let all: std::collections::BTreeSet<&str> = ALL_KNOWN_CTX_TOOLS.iter().copied().collect();

        let uncovered: Vec<_> = all.difference(&covered).copied().collect();
        assert!(
            uncovered.is_empty(),
            "CTX tool(s) in ALL_KNOWN_CTX_TOOLS but not in any tier array: {uncovered:?}\n\
             Add each tool to the correct CTX_* array above."
        );

        let extras: Vec<_> = covered.difference(&all).copied().collect();
        assert!(
            extras.is_empty(),
            "CTX tool(s) in tier arrays but not in ALL_KNOWN_CTX_TOOLS: {extras:?}\n\
             Either remove the stale entry or add the tool to ALL_KNOWN_CTX_TOOLS."
        );
    }
}
