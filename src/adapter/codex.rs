//! Adapter for OpenAI Codex (#233).
//!
//! Writes `config.toml` into the cache dir and exports `CODEX_HOME` so Codex
//! discovers it — the direct analogue of `CLAUDE_CONFIG_DIR`.
//!
//! # Scope
//!
//! This is the first materialization slice: MCP servers and the merged
//! `AGENTS.md`. The remaining parity work has its own issues, and each is a
//! deliberate gap rather than an oversight:
//!
//! - permissions (#1102) — landed, filesystem-only; see
//!   [`classify_permission_profile`]. Codex's `[permissions.<name>]` profiles
//!   also cover `network.domains`, but rendering that meaningfully requires
//!   also modeling `network.enabled`/`network.mode` (Codex's network proxy is
//!   off by default under `workspace-write`, so a domain entry alone can be
//!   dead config) — a bigger, separate sandbox/network-vocabulary gap
//!   (`docs/reference/codex/sandbox-and-approvals.md`) that this slice does
//!   not take on. `approval_policy`/`sandbox_mode` remain unmodeled too;
//!   Codex's permission profiles intersect with (never replace) those.
//! - lifecycle hooks (#1108) — landed; see [`render_hooks`].
//! - seeded settings (#1107) — landed; see [`apply_seeded_settings`]. Codex has
//!   no install-method seed to write (it self-detects in-process from its own
//!   exe path) and no external-command status-line hook to seed (#1104):
//!   `tui.status_line` is a fixed list of built-in item identifiers, not a
//!   "run a command" surface like Claude Code's `statusLine` — both are
//!   documented gaps, not missed work.
//! - session/history/auth inheritance across hash changes (#1105) — landed;
//!   see [`crate::materialize::inherit::link_codex_sessions_dir`] and sibling
//!   functions. The six SQLite state databases Codex also writes into
//!   `$CODEX_HOME` (state/logs/goals/memories/queue/thread-history) are **not**
//!   covered: naively symlinking or copying a live SQLite file risks
//!   corruption via its WAL/shm sidecars, and deserves its own design pass
//!   rather than reusing the single-file-copy contract that only fits
//!   `auth.json`/`history.jsonl`.
//! - rules beyond the merged AGENTS.md (#1103) — landed; Codex has no
//!   `rules/*.md`-with-glob-frontmatter convention
//!   (`docs/reference/codex/agents-md.md`), so `manifest.rules` folds into
//!   the same instructions file via
//!   [`crate::merge::agents_md::append_rules`] rather than being written out
//!   separately the way Claude Code and opencode do — lossy (path-scoped,
//!   conditional rules become unconditional prose), but the only target
//!   Codex has. `project_doc_max_bytes` (Codex's cap on its own AGENTS.md
//!   directory-walk discovery) does not apply here: this adapter always
//!   points `model_instructions_file` at an explicit path, a different,
//!   uncapped read path in Codex's own config loader.
//! - doctor diagnostics (#1100) — landed; see
//!   [`crate::cli::doctor`]'s Codex-specific section.
//! - plugins/skills/LSP (#1106) — landed for skills; see
//!   [`render_skills_config`]. Plugin-installation metadata and LSP are
//!   verified-absent from Codex (`supports_plugins`/`supports_lsp` doc
//!   comments), not deferred work.
//!
//! Field names come from Codex's own deserialization structs
//! (`codex-rs/config/src/config_toml.rs`, `mcp_types.rs`) rather than its docs,
//! which now redirect to developers.openai.com.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::AgentAdapter;
use crate::merge::MergedManifest;

/// Adapter for Codex: writes `config.toml` into the cache dir and exports
/// `CODEX_HOME` so Codex discovers it.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAdapter;

/// Codex reads `$CODEX_HOME/config.toml`.
pub(crate) const CODEX_CONFIG_FILE: &str = "config.toml";

/// Lifecycle events Codex accepts, from `HookEventsToml`'s serde renames
/// (`codex-rs/config/src/hook_config.rs`).
///
/// Claude Code's `Notification` has no entry there, so a bundle declaring it
/// gets a warning rather than a hook Codex would ignore.
const SUPPORTED_HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];

/// `hook-run` invocation baked into the hooks llmenv emits for Codex. The
/// `--engine` value is validated at the CLI boundary (#1386), so a rename here
/// fails loudly rather than sniffing a different engine's config.
const HOOK_RUN_COMMAND: &str = "llmenv hook-run --engine codex";

/// Injects the source config paths at session start so the agent edits llmenv's
/// config rather than the managed cache (#289).
const CONFIG_CONTEXT_COMMAND: &str = "llmenv config-context --engine codex";

/// Warns when the agent writes inside the managed cache dir (#289).
const CONFIG_GUARD_COMMAND: &str = "llmenv config-guard --engine codex";

/// Polls the usage backend and sleeps an adaptive delay.
const THROTTLE_COMMAND: &str = "llmenv throttle";

/// Engine-neutral lifecycle events that always get a `hook-run` registration,
/// paired with Codex's native event name. Mirrors the Claude Code adapter's
/// baseline: ICM memory wake-up/store plus the session-log lifecycle events.
const BASELINE_HOOK_EVENTS: &[(&str, &str)] = &[
    ("session_start", "SessionStart"),
    ("session_end", "SessionEnd"),
];

/// `(engine-neutral event, native Codex event)` pairs registered when any
/// session-log sink is enabled — per-hook prompt/tool-use capture (#382).
///
/// Claude Code's set minus `Notification`, which `HookEventsToml` has no field
/// for: emitting it would write a key Codex silently ignores, so the capture
/// would look wired and never fire.
const SESSION_LOG_HOOK_EVENTS: &[(&str, &str)] = &[
    ("user_prompt_submit", "UserPromptSubmit"),
    ("pre_tool_use", "PreToolUse"),
    ("post_tool_use", "PostToolUse"),
    ("stop", "Stop"),
    ("subagent_stop", "SubagentStop"),
    ("pre_compact", "PreCompact"),
];

/// Keys this adapter renders itself, and which a `native.codex` catch-all
/// fragment must therefore not overwrite.
///
/// `model_instructions_file` is here for the same reason opencode guards its
/// `instructions`: it points at the bundle-merged rules pipeline, which is where
/// org and security policy lives. Letting the catch-all replace that scalar
/// would redirect every rule llmenv merged to a file of the fragment author's
/// choosing, silently — `overlay_native_json` runs after the render, so the
/// override simply wins. The framework already classifies this concept as
/// modeled-with-no-escape-hatch; use `capabilities.rules`.
const CODEX_MODELED_KEYS: &[&str] = &[
    "mcp_servers",
    "model_instructions_file",
    "hooks",
    "permissions",
    "default_permissions",
    "skills",
];

/// Name of the `[permissions.<name>]` profile llmenv writes and, when
/// rendered, activates via `default_permissions` (#1102).
const PERMISSION_PROFILE_NAME: &str = "llmenv";

/// Keys `apply_seeded_settings` refuses to seed even though llmenv doesn't
/// render them itself.
///
/// #1102 models `capabilities.permissions` as far as filesystem access goes,
/// but `approval_policy`/`sandbox_mode`/etc. remain unmodeled and out of
/// [`CODEX_MODELED_KEYS`] — leaving them seedable would let
/// `init.seeded_settings` silently weaken the posture a user's
/// `capabilities.permissions` establishes on every other engine, the same
/// class of gap `warn_about_unrenderable_capabilities` already warns about
/// for a rule the rendered profile can't represent (security-audit, #1421).
const CODEX_SECURITY_SENSITIVE_KEYS: &[&str] = &[
    "approval_policy",
    "sandbox_mode",
    "sandbox_workspace_write",
    "trusted_projects",
    "shell_environment_policy",
];

impl AgentAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn is_active(&self) -> bool {
        std::env::var("CODEX_HOME").is_ok()
    }

    fn binary_name(&self) -> &'static str {
        "codex"
    }

    fn supports_plugins(&self) -> bool {
        // #1106: verified against `openai/codex`'s own source — there is no
        // plugin-installation-metadata concept at all (no analogue of
        // `installed_plugins.json`), so there is nothing for this adapter to
        // model. Skills (a separate llmenv concept from plugins) are handled
        // unconditionally in `materialize`/`render_skills_config` regardless
        // of this flag.
        false
    }

    fn supports_lsp(&self) -> bool {
        // #1106: verified against `openai/codex`'s own source — no `Lsp`
        // config struct, no `[lsp]` table anywhere in `config_toml.rs`.
        false
    }

    fn supports_model_providers(&self) -> bool {
        // Codex has `model_providers`, but rendering it is not in this slice.
        false
    }

    fn supports_output_styles(&self) -> bool {
        false
    }

    /// Only the maps this slice actually reads. `native_permissions` and
    /// `native_hooks` are deliberately absent: listing a map the adapter never
    /// renders would make `llmenv doctor`'s dead-native-key warning miss a key
    /// that genuinely does nothing for Codex.
    fn native_maps(&self) -> &'static [&'static str] {
        use crate::adapter::native_keys as nk;
        &[nk::NATIVE_MCP, nk::NATIVE_HOOKS, nk::NATIVE]
    }

    fn supported_hook_events(&self) -> &'static [&'static str] {
        SUPPORTED_HOOK_EVENTS
    }

    fn env_vars(
        &self,
        cache_dir: &Path,
        _state_dir: &Path,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let config_dir = cache_dir.to_str().ok_or_else(|| {
            anyhow::anyhow!("cache_dir is not valid UTF-8: {}", cache_dir.display())
        })?;
        // `CODEX_HOME` is the directory holding `config.toml`, not the file.
        Ok(vec![("CODEX_HOME".into(), config_dir.to_string())])
    }

    fn materialize(&self, manifest: &MergedManifest, out: &Path) -> anyhow::Result<Vec<PathBuf>> {
        super::skills::create_dir_owner_only(out)?;
        let mut owned: Vec<PathBuf> = Vec::new();

        let permission_decision = classify_permission_profile(&manifest.capabilities.permissions);
        warn_about_unrenderable_capabilities(&permission_decision);

        let mut doc = serde_json::Map::new();

        if let PermissionProfileDecision::Rendered(entries) = &permission_decision {
            let filesystem: serde_json::Map<String, serde_json::Value> = entries
                .iter()
                .map(|(path, access)| (path.clone(), json!(access.as_str())))
                .collect();
            let mut profile = serde_json::Map::new();
            profile.insert("description".into(), json!(PERMISSION_PROFILE_DESCRIPTION));
            profile.insert("filesystem".into(), serde_json::Value::Object(filesystem));
            let mut profiles = serde_json::Map::new();
            profiles.insert(
                PERMISSION_PROFILE_NAME.into(),
                serde_json::Value::Object(profile),
            );
            doc.insert("permissions".into(), serde_json::Value::Object(profiles));
            doc.insert("default_permissions".into(), json!(PERMISSION_PROFILE_NAME));
        }

        // AGENTS.md, pointed at explicitly. Codex discovers a project's
        // AGENTS.md on its own, but this one is llmenv's merged output living in
        // the cache dir, which is not a project root — `model_instructions_file`
        // is the field that takes an absolute path to it.
        //
        // #1103: Codex has no `rules/*.md`-with-glob-frontmatter convention
        // (`docs/reference/codex/agents-md.md`), so `manifest.rules` folds into
        // this same file instead of being written out as separate files the
        // way Claude Code and opencode do — a lossy transform (path-scoped,
        // conditional rules become unconditional prose), but the only target
        // Codex has.
        super::skills::reject_hardcoded_config_path(&manifest.agents_md, "AGENTS.md")?;
        for r in &manifest.rules {
            super::skills::reject_hardcoded_config_path(&r.raw, &r.rel.to_string_lossy())?;
        }
        let instructions_content =
            crate::merge::agents_md::append_rules(&manifest.agents_md, &manifest.rules);
        if !instructions_content.trim().is_empty() {
            let agents_path = out.join("AGENTS.md");
            crate::paths::write_owner_only(&agents_path, instructions_content.as_bytes())?;
            let as_str = agents_path.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "materialized AGENTS.md path is not valid UTF-8: {}",
                    agents_path.display()
                )
            })?;
            doc.insert("model_instructions_file".into(), json!(as_str));
            // Relative, per the trait contract: `CacheManifest::new` filters the
            // owned set through `is_unsafe_join_target`, which rejects absolute
            // paths (#196). An absolute entry here is silently dropped, and a
            // file llmenv doesn't own is never ghost-reconciled — a stale
            // AGENTS.md would sit in CODEX_HOME feeding revoked instructions to
            // the model, since that is also Codex's own user-instructions path.
            owned.push(PathBuf::from("AGENTS.md"));
        }

        // #1106: first-class skills + the built-in `llmenv` skill, reusing the
        // same SKILL.md convention Claude Code uses
        // (`docs/reference/codex/skills.md`). Unlike Claude Code's
        // auto-discovery, Codex requires each skill folder to be explicitly
        // registered via `[[skills.config]]` — `render_skills_config` scans
        // `out/skills/` afterward rather than tracking each skill by name
        // through the several code paths below that can write one, so a
        // skill can never go unregistered just because a future writer
        // forgot to also update a registration list.
        owned.extend(super::skills::write_first_class_skills(
            out,
            &manifest.capabilities.skills,
        )?);
        let features = manifest.capabilities.features.clone().unwrap_or_default();
        owned.extend(super::llmenv_skill::materialize_llmenv_skill(
            out, &features,
        )?);
        super::skills::validate_skills(out)?;

        let skills_config = render_skills_config(out)?;
        if !skills_config.is_empty() {
            doc.insert("skills".into(), json!({ "config": skills_config }));
        }

        let mcp_servers = render_mcp_servers(manifest);
        if !mcp_servers.is_empty() || manifest.capabilities.native_mcp.contains_key("codex") {
            let mut value = serde_json::Value::Object(mcp_servers);
            super::overlay_native_json(
                &mut value,
                manifest.capabilities.native_mcp.get("codex"),
                "native_mcp.codex",
            )?;
            if !value.as_object().is_none_or(serde_json::Map::is_empty) {
                doc.insert("mcp_servers".into(), value);
            }
        }

        // Always non-empty now: `render_hooks` appends the baseline registrations
        // llmenv wires itself, not just what a bundle declared.
        let hooks = render_hooks(manifest);
        if !hooks.is_empty() || manifest.capabilities.native_hooks.contains_key("codex") {
            let mut value = serde_json::json!({ "events": hooks });
            super::overlay_native_json(
                &mut value,
                manifest.capabilities.native_hooks.get("codex"),
                "native_hooks.codex",
            )?;
            doc.insert("hooks".into(), value);
        }

        if let Some(native) = manifest.native.get("codex") {
            super::reject_modeled_native_keys(native, CODEX_MODELED_KEYS, "codex")?;
        }
        let mut doc_value = serde_json::Value::Object(doc);
        super::overlay_native_json(&mut doc_value, manifest.native.get("codex"), "native.codex")?;
        // TOML has no null, so a native null would fail serialization outright
        // rather than deleting the key it targets. Stripping matches every other
        // adapter's "native null deletes the rendered key" contract (#1270).
        super::strip_json_nulls(&mut doc_value);

        let rendered = toml::to_string_pretty(&doc_value)
            .map_err(|e| anyhow::anyhow!("failed to render Codex config.toml: {e}"))?;
        let out_path = out.join(CODEX_CONFIG_FILE);
        crate::paths::write_owner_only(&out_path, rendered.as_bytes())?;
        owned.push(PathBuf::from(CODEX_CONFIG_FILE));

        Ok(owned)
    }

    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String {
        super::emit_hook_context(hook_event_name, text)
    }
}

/// Warn about capabilities this slice can't render, rather than failing the
/// whole render.
///
/// A bundle shared across engines commonly declares capabilities for Claude
/// Code. That is a cross-engine gap, not an authoring mistake, and failing here
/// would also drop the MCP servers and AGENTS.md that Codex *can* use — the same
/// reasoning the Crush adapter applies to its own unsupported events.
fn warn_about_unrenderable_capabilities(decision: &PermissionProfileDecision) {
    if let PermissionProfileDecision::Refused {
        unmappable_rule_count,
        mappable_rule_count,
    } = decision
    {
        eprintln!(
            "warning: the Codex adapter cannot render capabilities.permissions as a Codex \
             permission profile — {unmappable_rule_count} rule(s) have no Codex equivalent \
             (Bash/other command rules, WebFetch/domain rules, or `ask`-tier rules — Codex's \
             permission profiles model filesystem access only, and `ask` has no per-rule \
             posture at all). Per #1102's all-or-nothing rule, this also drops {mappable_rule_count} \
             otherwise-renderable Read/Edit/Write/MultiEdit path rule(s) rather than emit a \
             profile that looks more complete than it is. Codex runs under its own default \
             approval policy and sandbox mode instead. Tracking issue: \
             https://github.com/phaedrus1992/llmenv/issues/1102"
        );
    }
}

/// The Codex filesystem access mode a rendered path entry gets (#1102).
///
/// Mirrors Codex's own `FileSystemAccessMode` (`codex-rs/protocol/src/permissions.rs`)
/// so the variants serialize as its documented lowercase strings, and the
/// declaration order matches its stated conflict precedence — deny beats
/// write, write beats read — so `Ord::max` picks the right value when the
/// same path appears under more than one neutral rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CodexFsAccess {
    Read,
    Write,
    Deny,
}

impl CodexFsAccess {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Deny => "deny",
        }
    }
}

/// Why the Codex `[permissions.<name>]` profile was or wasn't rendered
/// (#1102).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionProfileDecision {
    /// No neutral permission rules at all — nothing to render, nothing to warn
    /// about.
    Empty,
    /// Every rule maps cleanly onto Codex's filesystem permission entries.
    Rendered(std::collections::BTreeMap<String, CodexFsAccess>),
    /// At least one rule has no Codex equivalent (or is `ask`-tier, which
    /// Codex's permission profile has no per-rule concept of at all), so
    /// nothing renders — a partial profile would look like enforcement for
    /// rules it can't actually represent.
    Refused {
        unmappable_rule_count: usize,
        mappable_rule_count: usize,
    },
}

/// Explains what a rendered `[permissions.llmenv]` profile covers, so a human
/// reading `config.toml` (or a bundle author debugging a dropped rule) knows
/// its scope without cross-referencing the issue tracker. Since the profile
/// only ever renders when every declared rule mapped cleanly (#1102's
/// all-or-nothing rule), this never has to say what was dropped — check the
/// stderr warning for that.
const PERMISSION_PROFILE_DESCRIPTION: &str = "Generated by llmenv from capabilities.permissions. \
     Covers Read/Edit/Write/MultiEdit path rules only — llmenv renders nothing here at all if \
     the config also has a rule with no Codex equivalent (Bash, WebFetch, `ask`-tier, etc.); see \
     https://github.com/phaedrus1992/llmenv/issues/1102.";

/// Normalize a rule's path string so equivalent spellings collide at the same
/// `classify_permission_profile` entry (security-audit finding, #1102): a
/// deny rule written as `./src/` and an allow rule on the same path written
/// as `src` must reconcile to one entry, or the documented deny-wins
/// precedence silently fails to apply just because the two rules spelled the
/// path differently. Strips a leading `./` and a trailing `/` (never the bare
/// root `/`) — the two unambiguous cases where two spellings are certainly
/// the same path. Deliberately does **not** attempt to reconcile a relative
/// path against an absolute one: that requires knowing the workspace root
/// Codex resolves against, which this adapter has no reliable way to know,
/// and guessing would risk merging two paths that are not actually the same.
fn normalize_permission_path(path: &str) -> String {
    let trimmed = path.strip_prefix("./").unwrap_or(path);
    match trimmed.strip_suffix('/') {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => trimmed.to_string(),
    }
}

/// The Codex filesystem access a neutral tool implies, or `None` for a tool
/// with no Codex permission-profile equivalent (#1102).
fn mappable_fs_access(tool: &str) -> Option<CodexFsAccess> {
    match tool {
        "Read" => Some(CodexFsAccess::Read),
        "Edit" | "Write" | "MultiEdit" => Some(CodexFsAccess::Write),
        _ => None,
    }
}

/// Classify `perms` into what a Codex `[permissions.<name>]` profile can
/// faithfully represent (#1102).
///
/// All-or-nothing per config, deliberately: Codex's permission profiles model
/// filesystem (and, unmodeled here, network) access only — there is no
/// per-command allowlist and no per-rule `ask` posture, only the global
/// `approval_policy`/`sandbox_mode`. Rendering the mappable subset while
/// silently dropping the rest would produce a profile that *looks* like it
/// enforces a user's full rule set when it enforces only part of it — worse
/// than the status quo of rendering nothing. So a single unmappable rule
/// anywhere in `allow`/`ask`/`deny` refuses the whole profile, not just that
/// rule.
///
/// `ask`-tier rules are unconditionally unmappable, regardless of tool: Codex
/// has no way to say "prompt about this one path" — only a global posture —
/// so an `ask` rule can never be represented at the per-rule granularity the
/// user wrote it at.
pub(crate) fn classify_permission_profile(
    perms: &crate::config::Permissions,
) -> PermissionProfileDecision {
    use std::collections::BTreeMap;

    let total = perms.allow.len() + perms.ask.len() + perms.deny.len();
    if total == 0 {
        return PermissionProfileDecision::Empty;
    }

    let mut entries: BTreeMap<String, CodexFsAccess> = BTreeMap::new();
    // `ask` has no per-rule Codex equivalent at all (see doc comment above),
    // so every `ask` rule is unmappable regardless of its tool.
    let mut unmappable = perms.ask.len();
    let mut mappable = 0usize;

    let allow_rules = perms.allow.iter().map(|rule| (rule, None));
    let deny_rules = perms
        .deny
        .iter()
        .map(|rule| (rule, Some(CodexFsAccess::Deny)));
    for (rule, forced_access) in allow_rules.chain(deny_rules) {
        let clean_paths_rule = !rule.paths.is_empty() && rule.pattern.is_none();
        let Some(fs_kind) = clean_paths_rule
            .then(|| mappable_fs_access(&rule.tool))
            .flatten()
        else {
            unmappable += 1;
            continue;
        };
        mappable += 1;
        // `forced_access` (deny) always wins over the tool-implied access —
        // a `deny` rule blocks the path outright regardless of whether it was
        // declared against `Read` or `Write`/`Edit`/`MultiEdit`.
        let access = forced_access.unwrap_or(fs_kind);
        for path in &rule.paths {
            entries
                .entry(normalize_permission_path(path))
                .and_modify(|existing| *existing = (*existing).max(access))
                .or_insert(access);
        }
    }

    if unmappable > 0 {
        PermissionProfileDecision::Refused {
            unmappable_rule_count: unmappable,
            mappable_rule_count: mappable,
        }
    } else {
        PermissionProfileDecision::Rendered(entries)
    }
}

/// Render `manifest.capabilities.hooks` into Codex's `hooks.events` table.
///
/// Codex uses the same nested shape as Claude Code — a list of matcher groups
/// per event, each carrying `hooks: [{ type = "command", command = … }]` — and
/// the event names match too (`hook_config.rs`'s serde renames). So llmenv's
/// engine-neutral hooks map across without a translation layer.
///
/// Two kinds of hook are skipped with a warning rather than rendered:
///
/// - an event Codex has no field for. `Notification` is the live case: Claude
///   Code has it, `HookEventsToml` doesn't, and an unknown key would be ignored
///   silently (Codex tolerates unknown fields), so the hook would look wired
///   while never firing.
/// - an `mcp_tool` handler. Codex's handler enum is tagged `type` with a
///   `command` variant only.
fn render_hooks(manifest: &MergedManifest) -> serde_json::Map<String, serde_json::Value> {
    let mut by_event: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    for hook in &manifest.capabilities.hooks {
        if !SUPPORTED_HOOK_EVENTS.contains(&hook.event.as_str()) {
            eprintln!(
                "warning: Codex has no '{}' lifecycle event — skipping this hook. \
                 Supported events: {}. Move it to a claude_code-only bundle to \
                 silence this warning.",
                hook.event,
                SUPPORTED_HOOK_EVENTS.join(", ")
            );
            continue;
        }
        if matches!(hook.handler.kind, crate::config::HookHandlerKind::McpTool) {
            eprintln!(
                "warning: Codex hooks run commands only, so the mcp_tool hook on '{}' \
                 (tool '{}') is skipped. Use a command hook instead.",
                hook.event,
                hook.handler.tool.as_deref().unwrap_or("<unnamed>")
            );
            continue;
        }
        let Some(command) = hook.handler.command.as_ref() else {
            eprintln!(
                "warning: the command hook on '{}' has no command — skipping it.",
                hook.event
            );
            continue;
        };
        // Bundle-relative script paths are authored once and materialized per
        // engine, so they have to be resolved against the bundle they came from.
        let resolved = match &hook.bundle_origin {
            Some(bundle_dir) => super::resolve_bundle_relative_paths(command, bundle_dir)
                .unwrap_or_else(|| {
                    tracing::warn!(
                        "failed to resolve bundle-relative path for command in {bundle_dir:?}: {command:?}"
                    );
                    command.clone()
                }),
            None => command.clone(),
        };

        let mut group = serde_json::Map::new();
        if let Some(matcher) = &hook.matcher {
            group.insert("matcher".into(), json!(matcher));
        }
        group.insert(
            "hooks".into(),
            json!([{ "type": "command", "command": resolved }]),
        );
        by_event
            .entry(hook.event.clone())
            .or_default()
            .push(serde_json::Value::Object(group));
    }

    emit_baseline_hooks(manifest, &mut by_event);

    by_event
        .into_iter()
        .map(|(event, groups)| (event, json!(groups)))
        .collect()
}

/// Register the hooks llmenv wires itself, rather than ones a bundle declared —
/// the Codex half of the set the Claude Code adapter emits (#1108).
///
/// Safe to point at `hook-run` because Codex reads the same hook-output shape
/// Claude Code does: `{"hookSpecificOutput": {"hookEventName", "additionalContext"}}`,
/// which is exactly what `emit_hook_context` produces. Verified against
/// `codex-rs/hooks/src/engine/output_parser.rs`.
fn emit_baseline_hooks(
    manifest: &MergedManifest,
    by_event: &mut std::collections::BTreeMap<String, Vec<serde_json::Value>>,
) {
    let command_group =
        |command: String| json!({ "hooks": [{ "type": "command", "command": command }] });
    let matched_group = |matcher: &str, command: String| json!({ "matcher": matcher, "hooks": [{ "type": "command", "command": command }] });

    // Where to edit llmenv's config, injected at session start (#289).
    by_event
        .entry("SessionStart".into())
        .or_default()
        .push(command_group(CONFIG_CONTEXT_COMMAND.to_string()));

    // Anchored so only exact tool names match, not substrings like BatchEdit.
    // Exits 0 — it warns, it never blocks the write.
    by_event
        .entry("PreToolUse".into())
        .or_default()
        .push(matched_group(
            "^(Write|Edit|MultiEdit)$",
            CONFIG_GUARD_COMMAND.to_string(),
        ));

    // Read-once dedup (#318). The `hook-run` handler checks
    // `features.read_once.enabled` and passes through when off, so the matcher
    // is the only cost when the feature is disabled.
    //
    // #1442: skipped when session logging registers an unmatched `pre_tool_use`
    // below — that group already runs for every tool, including `Read`, so
    // emitting both made a `Read` reach the hook twice, halving
    // `repeat_detect`'s effective threshold and making `read_once` see its own
    // first entry.
    let session_log_includes_pre_tool_use = SESSION_LOG_HOOK_EVENTS
        .iter()
        .any(|(neutral, _)| *neutral == "pre_tool_use");
    if !super::unmatched_pre_tool_use_registered(manifest, session_log_includes_pre_tool_use) {
        by_event
            .entry("PreToolUse".into())
            .or_default()
            .push(matched_group(
                "^Read$",
                format!("{HOOK_RUN_COMMAND} pre_tool_use"),
            ));
    }

    if manifest.throttle.is_some() {
        by_event
            .entry("PreToolUse".into())
            .or_default()
            .push(command_group(format!("{THROTTLE_COMMAND} pre-tool")));
        by_event
            .entry("UserPromptSubmit".into())
            .or_default()
            .push(command_group(format!("{THROTTLE_COMMAND} prompt")));
    }

    for (neutral_event, native_event) in BASELINE_HOOK_EVENTS {
        by_event
            .entry((*native_event).to_string())
            .or_default()
            .push(command_group(format!("{HOOK_RUN_COMMAND} {neutral_event}")));
    }

    // #317: the rules digest runs on UserPromptSubmit, so enabling the layer has
    // to register the event — the memory-recall gate below and session logging
    // are the only other things that do, and neither is implied by turning
    // slippage on.
    if super::slippage_rule_reinjection_enabled(manifest)
        && !manifest.session_log.any_sink_enabled()
        && !super::lifecycle_event_registered(manifest, "turn_start")
    {
        by_event
            .entry("UserPromptSubmit".into())
            .or_default()
            .push(command_group(format!(
                "{HOOK_RUN_COMMAND} user_prompt_submit"
            )));
    }

    // #499: continuous per-prompt memory recall. Gated (unlike the baseline
    // events above) because it runs on every prompt — an unconditional
    // network-backed per-turn hook would add latency for scopes with no memory
    // backend configured at all.
    if super::lifecycle_event_registered(manifest, "turn_start") {
        by_event
            .entry("UserPromptSubmit".into())
            .or_default()
            .push(command_group(format!("{HOOK_RUN_COMMAND} turn_start")));
    }

    if manifest.session_log.any_sink_enabled() {
        for (neutral_event, native_event) in SESSION_LOG_HOOK_EVENTS {
            by_event
                .entry((*native_event).to_string())
                .or_default()
                .push(command_group(format!("{HOOK_RUN_COMMAND} {neutral_event}")));
        }
    } else if super::lifecycle_event_registered(manifest, "stop") {
        // #231/#317: the task tracker's Stop reminder and the self-critique
        // layer each need their own registration rather than depending on
        // session logging happening to be on. Only in the `else` branch, so a
        // session-log setup doesn't get two `hook-run` calls on the same event.
        by_event
            .entry("Stop".into())
            .or_default()
            .push(command_group(format!("{HOOK_RUN_COMMAND} stop")));
    }
}

/// Render `manifest.mcps` into Codex's `mcp_servers` table.
///
/// The shape is Codex's `RawMcpServerConfig`, which is what `config.toml` is
/// actually deserialized into: the transport is `#[serde(flatten)]`ed and
/// `untagged`, so there is **no** `type` key — a `command` means stdio and a
/// `url` means streamable HTTP. Emitting a `type` field would be rejected
/// outright, since the transport enum is `deny_unknown_fields`.
/// Build Codex's `[[skills.config]]` entries by scanning the skills llmenv
/// just wrote under `out/skills/` (#1106).
///
/// Codex has no auto-discovery like Claude Code's `skills/` convention — each
/// skill folder needs an explicit registration entry naming its absolute
/// `path` (`docs/reference/codex/skills.md`). Scanning the materialized
/// directory rather than threading a name list through every code path that
/// can write a skill (first-class skills, the built-in `llmenv` skill, and
/// any future source) means a skill can never go unregistered just because a
/// future writer forgot to also update a registration list.
fn render_skills_config(out: &Path) -> anyhow::Result<Vec<serde_json::Value>> {
    let skills_dir = out.join("skills");
    let Some(entries) = crate::paths::read_dir_optional(&skills_dir)? else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        // `entry.file_type()` (not `entry.path().is_dir()`, silent-failure-hunter
        // finding, #1106): `is_dir()` swallows any stat error — permission
        // denied, a TOCTOU race — into `false`, which would silently drop a
        // skill llmenv just wrote a few lines above from the registration
        // list, exactly the failure mode this function's own doc comment
        // says scanning the directory is supposed to prevent.
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name());
        }
    }
    names.sort();

    names
        .into_iter()
        .map(|name| {
            let path = skills_dir.join(&name);
            let as_str = path.to_str().ok_or_else(|| {
                anyhow::anyhow!("skill path is not valid UTF-8: {}", path.display())
            })?;
            Ok(json!({ "path": as_str, "enabled": true }))
        })
        .collect()
}

fn render_mcp_servers(manifest: &MergedManifest) -> serde_json::Map<String, serde_json::Value> {
    use crate::mcp::resolve::ResolvedKind;

    let mut servers = serde_json::Map::new();
    for mcp in &manifest.mcps {
        let mut entry = match &mcp.kind {
            ResolvedKind::Stdio { command, args, env } => {
                let mut e = serde_json::Map::new();
                e.insert("command".into(), json!(command));
                if !args.is_empty() {
                    e.insert("args".into(), json!(args));
                }
                if !env.is_empty() {
                    e.insert("env".into(), json!(env));
                }
                e
            }
            ResolvedKind::Remote { url, transport } => {
                // Codex speaks stdio and streamable HTTP only — its raw config
                // has no SSE field at all. Rendering an SSE server as `url`
                // would produce a config Codex reads as streamable HTTP and
                // then fails to talk to, so skip it and say why.
                if matches!(*transport, crate::config::McpTransport::Sse) {
                    eprintln!(
                        "warning: MCP server '{}' uses the SSE transport, which Codex does not \
                         support (it speaks stdio and streamable HTTP) — skipping it for Codex. \
                         Other engines still get it.",
                        mcp.name
                    );
                    continue;
                }
                let mut e = serde_json::Map::new();
                e.insert("url".into(), json!(url));
                if !mcp.headers.is_empty() {
                    e.insert("http_headers".into(), json!(mcp.headers));
                }
                e
            }
        };

        // `tool_timeout_sec`, not `startup_timeout_sec`: llmenv's `timeout` is
        // documented as a per-server *request* timeout in seconds, and Codex's
        // startup timeout covers initialization instead.
        if let Some(secs) = mcp.timeout {
            entry.insert("tool_timeout_sec".into(), json!(secs));
        }
        if !mcp.disabled_tools.is_empty() {
            entry.insert("disabled_tools".into(), json!(mcp.disabled_tools));
        }

        servers.insert(mcp.name.clone(), serde_json::Value::Object(entry));
    }
    servers
}

/// Merge user-elected seeded keys into `out/config.toml` after the adapter has
/// already written the file (#1107) — the Codex analogue of
/// `claude_code::apply_seeded_settings`.
///
/// Unlike Claude Code's `settings.json`, this adapter re-renders `config.toml`
/// from scratch every call rather than reconciling against the prior file, so
/// this always re-reads whatever `materialize` just wrote. A no-op once every
/// seeded key is already present. Never touches [`CODEX_MODELED_KEYS`] —
/// those are llmenv's own render surface, not a user default.
///
/// # Errors
/// Returns an error if the file cannot be read, parsed, re-serialized, or
/// written.
pub(crate) fn apply_seeded_settings(
    out: &Path,
    seeded: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if seeded.is_empty() {
        return Ok(());
    }
    let path = out.join(CODEX_CONFIG_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "reading {} for seeding: {e}",
                path.display()
            ));
        }
    };
    // `toml::Table`, not `toml::Value` — parsing straight to `Value` chokes on
    // array-of-tables headers like `[[hooks.events.PreToolUse]]` (the same
    // type this module's own `materialize_to_toml` test helper parses into).
    let toml_val: toml::Table = raw
        .parse()
        .map_err(|e| anyhow::anyhow!("parsing {} for seeding: {e}", path.display()))?;
    // Round-trip through `serde_json::Value` (guaranteed via `Serialize`,
    // rather than relying on the less-certain toml-Deserializer-into-Value
    // direction) so the merge logic matches `claude_code::apply_seeded_settings`
    // exactly.
    let mut doc = serde_json::to_value(&toml_val)
        .map_err(|e| anyhow::anyhow!("converting {} for seeding: {e}", path.display()))?;
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    let mut changed = false;
    for (k, v) in seeded {
        if CODEX_SECURITY_SENSITIVE_KEYS.contains(&k.as_str()) {
            eprintln!(
                "warning: '{k}' in init.seeded_settings is a security-sensitive Codex key and \
                 will NOT be seeded — llmenv only models Codex permissions as far as filesystem \
                 access (#1102), so a seeded approval_policy/sandbox_mode/etc. could silently \
                 run Codex less restrictively than the posture capabilities.permissions \
                 establishes on every other engine."
            );
            continue;
        }
        if !CODEX_MODELED_KEYS.contains(&k.as_str()) && !obj.contains_key(k) {
            obj.insert(k.clone(), v.clone());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    // TOML has no null, matching the adapter's own render path (`materialize`
    // strips nulls for the same reason): an explicit null in seeded_settings
    // must delete nothing here (there's nothing to delete — a null seeded
    // value can only ever be newly inserted, never override an existing key)
    // rather than reach the serializer and fail the whole write.
    super::strip_json_nulls(&mut doc);
    let rendered = toml::to_string_pretty(&doc)
        .map_err(|e| anyhow::anyhow!("rendering seeded {}: {e}", path.display()))?;
    crate::paths::write_owner_only_atomic(&path, rendered.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing seeded {}: {e}", path.display()))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test scaffolding"
)]
mod tests {
    use super::{
        CodexAdapter, HOOK_RUN_COMMAND, SESSION_LOG_HOOK_EVENTS, SUPPORTED_HOOK_EVENTS,
        render_mcp_servers,
    };
    use crate::adapter::AgentAdapter;
    use crate::config::McpTransport;
    use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
    use crate::merge::MergedManifest;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn stdio_mcp(name: &str) -> ResolvedMcp {
        ResolvedMcp {
            name: name.into(),
            kind: ResolvedKind::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "some-mcp".into()],
                env: BTreeMap::new(),
            },
            headers: BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            wakeup_max_tokens: None,
        }
    }

    fn remote_mcp(name: &str, transport: McpTransport) -> ResolvedMcp {
        ResolvedMcp {
            name: name.into(),
            kind: ResolvedKind::Remote {
                url: "https://example.test/mcp".into(),
                transport,
            },
            headers: BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            wakeup_max_tokens: None,
        }
    }

    /// Parse the materialized `config.toml` back, so assertions are about what
    /// Codex would actually read rather than about the text llmenv emitted.
    fn materialize_to_toml(manifest: &MergedManifest) -> (tempfile::TempDir, toml::Table) {
        let dir = tempfile::tempdir().unwrap();
        CodexAdapter.materialize(manifest, dir.path()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        let parsed: toml::Table = raw.parse().expect("emitted config.toml must be valid TOML");
        (dir, parsed)
    }

    #[test]
    fn env_vars_point_codex_home_at_the_config_directory() {
        let cache = std::path::Path::new("/tmp/llmenv-cache/codex");
        let vars = CodexAdapter
            .env_vars(cache, std::path::Path::new("/tmp/llmenv-state"))
            .unwrap();
        assert_eq!(
            vars,
            vec![(
                "CODEX_HOME".to_string(),
                "/tmp/llmenv-cache/codex".to_string()
            )],
            "CODEX_HOME must be the directory holding config.toml, not the file"
        );
    }

    /// Codex's transport enum is `untagged` + `deny_unknown_fields`: a `command`
    /// key means stdio and a `url` means streamable HTTP. Emitting a `type` key
    /// — which every other adapter in this tree does — would make Codex reject
    /// the whole server entry.
    #[test]
    fn stdio_servers_render_without_a_type_key() {
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(stdio_mcp("icm"));

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let server = parsed["mcp_servers"]["icm"].as_table().unwrap();

        assert_eq!(server["command"].as_str(), Some("npx"));
        assert_eq!(
            server["args"].as_array().unwrap().len(),
            2,
            "args must survive as an array"
        );
        assert!(
            !server.contains_key("type"),
            "Codex's transport is untagged and deny_unknown_fields — a `type` key \
             would make it reject this server: {server:?}"
        );
    }

    #[test]
    fn http_servers_render_as_a_url() {
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(remote_mcp("remote", McpTransport::Http));

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let server = parsed["mcp_servers"]["remote"].as_table().unwrap();

        assert_eq!(server["url"].as_str(), Some("https://example.test/mcp"));
        assert!(!server.contains_key("type"));
        assert!(!server.contains_key("command"));
    }

    /// Codex has no SSE transport — its raw config accepts `command` or `url`
    /// only. Rendering an SSE server as a plain `url` would produce a config
    /// Codex reads as streamable HTTP and then can't talk to, so it's skipped.
    #[test]
    fn sse_servers_are_skipped_rather_than_mistranslated() {
        let mut manifest = MergedManifest::default();
        manifest
            .mcps
            .push(remote_mcp("sse-server", McpTransport::Sse));
        manifest.mcps.push(stdio_mcp("kept"));

        let servers = render_mcp_servers(&manifest);
        assert!(
            !servers.contains_key("sse-server"),
            "an SSE server must not be rendered as streamable HTTP"
        );
        assert!(
            servers.contains_key("kept"),
            "skipping one server must not drop the others"
        );
    }

    /// llmenv's `timeout` is a per-server *request* timeout in seconds, which is
    /// Codex's `tool_timeout_sec` — not `startup_timeout_sec`, which covers
    /// initialization.
    #[test]
    fn timeout_maps_to_the_tool_timeout_not_the_startup_timeout() {
        let mut manifest = MergedManifest::default();
        let mut mcp = stdio_mcp("slow");
        mcp.timeout = Some(30);
        mcp.disabled_tools = vec!["dangerous".into()];
        manifest.mcps.push(mcp);

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let server = parsed["mcp_servers"]["slow"].as_table().unwrap();

        assert_eq!(server["tool_timeout_sec"].as_integer(), Some(30));
        assert!(!server.contains_key("startup_timeout_sec"));
        assert_eq!(
            server["disabled_tools"].as_array().unwrap()[0].as_str(),
            Some("dangerous")
        );
    }

    /// The trait contract says the owned set is relative to `out`, and
    /// `CacheManifest::new` enforces it by dropping anything absolute (#196).
    /// An absolute entry here doesn't error — it silently means llmenv owns
    /// nothing, so a file it stops writing is never cleaned up.
    #[test]
    fn materialize_returns_paths_relative_to_out() {
        let mut manifest = MergedManifest {
            agents_md: "# Rules\n".into(),
            ..MergedManifest::default()
        };
        manifest.mcps.push(stdio_mcp("icm"));

        let dir = tempfile::tempdir().unwrap();
        let owned = CodexAdapter.materialize(&manifest, dir.path()).unwrap();

        assert!(
            owned.iter().all(|p| p.is_relative()),
            "owned paths must be relative to `out` or CacheManifest discards them: {owned:?}"
        );
        for name in ["AGENTS.md", "config.toml"] {
            assert!(
                owned.iter().any(|p| p == std::path::Path::new(name)),
                "{name} must be reported as owned so it can be ghost-reconciled: {owned:?}"
            );
            assert!(dir.path().join(name).is_file(), "{name} should exist");
        }
    }

    #[test]
    fn agents_md_is_written_and_pointed_at_by_absolute_path() {
        let manifest = MergedManifest {
            agents_md: "# Rules\nBe terse.\n".into(),
            ..MergedManifest::default()
        };

        let (dir, parsed) = materialize_to_toml(&manifest);
        let pointer = parsed["model_instructions_file"].as_str().unwrap();

        assert_eq!(pointer, dir.path().join("AGENTS.md").to_str().unwrap());
        assert!(
            std::path::Path::new(pointer).is_file(),
            "model_instructions_file must point at a file that exists"
        );
        assert_eq!(
            std::fs::read_to_string(pointer).unwrap(),
            "# Rules\nBe terse.\n"
        );
    }

    /// #1269's rule, applied here: empty content must leave no file behind and
    /// no pointer to one, or Codex loads an empty instructions file.
    #[test]
    fn empty_agents_md_writes_no_file_and_no_pointer() {
        let manifest = MergedManifest::default();
        let (dir, parsed) = materialize_to_toml(&manifest);

        assert!(!dir.path().join("AGENTS.md").exists());
        assert!(!parsed.contains_key("model_instructions_file"));
    }

    // ---- rules folded into AGENTS.md (#1103) ----

    fn rule_file(
        bundle: &str,
        rel: &str,
        body: &str,
        frontmatter: Option<&str>,
    ) -> crate::merge::rules::RuleFile {
        let raw = match frontmatter {
            Some(fm) => format!("---\n{fm}\n---\n{body}"),
            None => body.to_string(),
        };
        crate::merge::rules::RuleFile {
            bundle: bundle.into(),
            rel: std::path::PathBuf::from(rel),
            frontmatter: frontmatter.map(String::from),
            body: body.into(),
            raw,
        }
    }

    /// Codex has no `rules/*.md` convention (#1103) — a declared rule must
    /// fold into `AGENTS.md`/`model_instructions_file` rather than being
    /// silently dropped, and no `rules/` directory is ever written.
    #[test]
    fn rules_fold_into_agents_md_with_frontmatter_stripped() {
        let mut manifest = MergedManifest {
            agents_md: "# Base rules\n".into(),
            ..MergedManifest::default()
        };
        manifest.rules.push(rule_file(
            "base",
            "rules/rust.md",
            "# Rust rules\nUse `?` not `unwrap`.\n",
            Some("scope: rust"),
        ));

        let dir = tempfile::tempdir().unwrap();
        let owned = CodexAdapter.materialize(&manifest, dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();

        assert!(content.contains("# Base rules"));
        assert!(content.contains("# Rust rules"));
        assert!(content.contains("Use `?` not `unwrap`."));
        assert!(
            !content.contains("scope: rust"),
            "frontmatter must not leak into AGENTS.md: {content}"
        );
        assert!(
            !dir.path().join("rules").exists(),
            "Codex has no rules/ convention — nothing should be written there"
        );
        assert!(
            owned.iter().all(|p| p != std::path::Path::new("rules")),
            "no rules/ path should be reported as owned: {owned:?}"
        );
    }

    /// A rule with no `agents_md` content at all must still render — folding
    /// must not depend on there being a non-empty base.
    #[test]
    fn a_rule_alone_with_empty_agents_md_still_renders() {
        let mut manifest = MergedManifest::default();
        manifest
            .rules
            .push(rule_file("base", "rules/only.md", "# Only a rule\n", None));

        let (dir, parsed) = materialize_to_toml(&manifest);
        let content = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(content.contains("# Only a rule"));
        assert_eq!(
            parsed["model_instructions_file"].as_str(),
            Some(dir.path().join("AGENTS.md").to_str().unwrap())
        );
    }

    /// A hardcoded cache-dir path in a rule body must be rejected the same
    /// way one in `agents_md` itself already is (#289).
    #[test]
    fn a_hardcoded_config_path_in_a_rule_body_is_rejected() {
        let mut manifest = MergedManifest::default();
        manifest.rules.push(rule_file(
            "base",
            "rules/bad.md",
            "Edit ~/.claude/settings.json directly.\n",
            None,
        ));

        let dir = tempfile::tempdir().unwrap();
        let err = CodexAdapter.materialize(&manifest, dir.path());
        assert!(
            err.is_err(),
            "a hardcoded config path in a rule body must be rejected: {err:?}"
        );
    }

    /// A server name with a dot would silently become nested tables if the
    /// emitter didn't quote keys — the reason this adapter serializes with the
    /// `toml` crate rather than formatting strings.
    #[test]
    fn server_names_needing_quotes_round_trip() {
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(stdio_mcp("weird.name with space"));

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let servers = parsed["mcp_servers"].as_table().unwrap();

        assert!(
            servers.contains_key("weird.name with space"),
            "the dotted name must stay one key, not become nested tables: {servers:?}"
        );
    }

    /// TOML has no null. A `native` null must delete the key it targets — the
    /// same contract every other adapter honours (#1270) — rather than reaching
    /// the serializer and failing the whole render.
    ///
    /// Uses a key the catch-all is allowed to set: `reject_modeled_native_keys`
    /// checks for the key's *presence*, not its value, so a null on a modeled
    /// key is a hard error rather than a delete. Crush's equivalent test makes
    /// the same choice for the same reason.
    #[test]
    fn a_native_null_deletes_the_key_instead_of_failing_serialization() {
        let mut manifest = MergedManifest::default();
        manifest.native.insert(
            "codex".into(),
            serde_yaml::from_str("model: o3\nreview_model: null\n").unwrap(),
        );

        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert_eq!(parsed["model"].as_str(), Some("o3"));
        assert!(
            !parsed.contains_key("review_model"),
            "an explicit null must remove the key rather than emit a TOML value"
        );
    }

    fn command_hook(event: &str, matcher: Option<&str>, command: &str) -> crate::config::Hook {
        crate::config::Hook {
            event: event.into(),
            matcher: matcher.map(Into::into),
            handler: crate::config::HookHandler {
                kind: crate::config::HookHandlerKind::Command,
                command: Some(command.into()),
                tool: None,
            },
            bundle_origin: None,
        }
    }

    /// A hook declared with a bundle-relative script path must resolve
    /// against that bundle's directory (#1101) — `resolve_bundle_relative_paths`
    /// itself is covered by its own unit test in `adapter::mod`, but the glue
    /// wiring `hook.bundle_origin` through `render_hooks` is Codex-specific
    /// and needs its own proof it's actually called.
    #[test]
    fn hook_command_with_bundle_origin_resolves_relative_paths() {
        let mut manifest = MergedManifest::default();
        let mut hook = command_hook("PreToolUse", Some("Bash"), "bash hooks/guard.sh");
        hook.bundle_origin = Some(std::path::PathBuf::from("/bundles/foo"));
        manifest.capabilities.hooks.push(hook);

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let rendered = format!("{:?}", parsed["hooks"]["events"]["PreToolUse"]);
        assert!(
            rendered.contains("/bundles/foo/hooks/guard.sh"),
            "bundle-relative hook command must resolve against bundle_origin: {rendered}"
        );
    }

    /// Codex takes the same nested matcher-group shape as Claude Code, under
    /// `hooks.events.<Event>`, and its event names match — so the neutral hooks
    /// map across without translation.
    #[test]
    fn hooks_render_into_codex_matcher_groups() {
        let mut manifest = MergedManifest::default();
        manifest
            .capabilities
            .hooks
            .push(command_hook("PreToolUse", Some("Bash"), "echo guard"));

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let groups = parsed["hooks"]["events"]["PreToolUse"].as_array().unwrap();

        // Other groups on this event are the baseline hooks llmenv wires itself.
        let declared = groups
            .iter()
            .find(|g| g.get("matcher").and_then(toml::Value::as_str) == Some("Bash"))
            .expect("the declared hook must be present among the baseline ones");
        let handlers = declared["hooks"].as_array().unwrap();
        assert_eq!(handlers[0]["type"].as_str(), Some("command"));
        assert_eq!(handlers[0]["command"].as_str(), Some("echo guard"));
    }

    /// llmenv wires its own lifecycle hooks for Codex the way it does for Claude
    /// Code (#1108). They point at `hook-run --engine codex`, which is safe
    /// because Codex reads the same `hookSpecificOutput`/`additionalContext`
    /// shape `emit_hook_context` produces.
    #[test]
    fn baseline_hooks_are_registered_without_any_declared_hooks() {
        let manifest = MergedManifest::default();
        let (_dir, parsed) = materialize_to_toml(&manifest);
        let events = parsed["hooks"]["events"].as_table().unwrap();
        let rendered = format!("{events:?}");

        for expected in [
            "llmenv config-context --engine codex",
            "llmenv config-guard --engine codex",
            "llmenv hook-run --engine codex pre_tool_use",
            "llmenv hook-run --engine codex session_start",
            "llmenv hook-run --engine codex session_end",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
        assert!(events.contains_key("SessionStart"));
        assert!(events.contains_key("SessionEnd"));
    }

    /// Every `command` string registered under one native Codex event.
    fn hook_commands_for(parsed: &toml::Table, event: &str) -> Vec<String> {
        parsed["hooks"]["events"]
            .get(event)
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|group| group.get("hooks").and_then(toml::Value::as_array))
            .flatten()
            .filter_map(|h| h.get("command").and_then(toml::Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    fn manifest_with_memory_mcp() -> MergedManifest {
        MergedManifest {
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
        }
    }

    /// A manifest with session logging off, so gates that session logging would
    /// otherwise satisfy on its own can actually be told apart.
    fn manifest_without_session_log() -> MergedManifest {
        let mut manifest = MergedManifest::default();
        manifest.session_log.file = None;
        manifest.session_log.transcript = None;
        manifest
    }

    /// #1435: Codex materialized the memory MCP but never the per-turn recall
    /// hook, so `features.memory` was configured and nothing ever fired.
    #[test]
    fn turn_start_wired_when_memory_backend_active() {
        let (_dir, parsed) = materialize_to_toml(&manifest_with_memory_mcp());
        assert!(
            hook_commands_for(&parsed, "UserPromptSubmit")
                .contains(&format!("{HOOK_RUN_COMMAND} turn_start")),
            "{parsed:?}"
        );
    }

    /// Per-prompt and network-backed, so it stays off for scopes with no memory
    /// backend — same gate the Claude Code adapter applies.
    #[test]
    fn turn_start_not_wired_without_memory_backend() {
        let (_dir, parsed) = materialize_to_toml(&MergedManifest::default());
        assert!(
            !hook_commands_for(&parsed, "UserPromptSubmit")
                .contains(&format!("{HOOK_RUN_COMMAND} turn_start")),
            "{parsed:?}"
        );
    }

    /// #231: the task tracker's Stop reminder must not depend on session
    /// logging happening to be on.
    #[test]
    fn stop_wired_for_task_tracker_without_session_log() {
        let mut manifest = manifest_without_session_log();
        manifest.capabilities.features = Some(llmenv_config::Features {
            task_tracker: Some(llmenv_config::TaskTracker {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(
            hook_commands_for(&parsed, "Stop").contains(&format!("{HOOK_RUN_COMMAND} stop")),
            "{parsed:?}"
        );
    }

    /// #317: the self-critique layer runs on Stop, so enabling it has to
    /// register the event — otherwise the layer is a phantom.
    #[test]
    fn stop_wired_for_slippage_self_critique_alone() {
        let mut manifest = manifest_without_session_log();
        manifest.capabilities.features = Some(llmenv_config::Features {
            slippage: Some(llmenv_config::SlippageControl {
                enabled: true,
                self_critique: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(
            hook_commands_for(&parsed, "Stop").contains(&format!("{HOOK_RUN_COMMAND} stop")),
            "{parsed:?}"
        );
    }

    #[test]
    fn stop_not_wired_without_session_log_task_tracker_or_self_critique() {
        let (_dir, parsed) = materialize_to_toml(&manifest_without_session_log());
        assert!(
            !hook_commands_for(&parsed, "Stop").contains(&format!("{HOOK_RUN_COMMAND} stop")),
            "{parsed:?}"
        );
    }

    /// #317: the rules digest runs on UserPromptSubmit, so enabling
    /// reinjection has to register the event on its own.
    #[test]
    fn slippage_rule_reinjection_alone_registers_user_prompt_submit() {
        let mut manifest = manifest_without_session_log();
        manifest.capabilities.features = Some(llmenv_config::Features {
            slippage: Some(llmenv_config::SlippageControl {
                enabled: true,
                rule_reinjection: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(
            hook_commands_for(&parsed, "UserPromptSubmit")
                .contains(&format!("{HOOK_RUN_COMMAND} user_prompt_submit")),
            "{parsed:?}"
        );
    }

    /// #382: per-turn capture, registered when any sink is enabled. Codex has
    /// no `Notification` event, so that one member of Claude's set is dropped
    /// rather than emitted as a key Codex would ignore.
    #[test]
    fn session_log_turn_hooks_registered_when_a_sink_is_enabled() {
        let (_dir, parsed) = materialize_to_toml(&MergedManifest::default());
        for (neutral, native) in SESSION_LOG_HOOK_EVENTS {
            assert!(
                hook_commands_for(&parsed, native)
                    .contains(&format!("{HOOK_RUN_COMMAND} {neutral}")),
                "{native} missing {neutral}: {parsed:?}"
            );
        }
        assert!(
            !parsed["hooks"]["events"]
                .as_table()
                .unwrap()
                .contains_key("Notification"),
            "Codex has no Notification event: {parsed:?}"
        );
    }

    #[test]
    fn session_log_turn_hooks_absent_when_every_sink_is_off() {
        let (_dir, parsed) = materialize_to_toml(&manifest_without_session_log());
        assert!(
            !hook_commands_for(&parsed, "PostToolUse")
                .contains(&format!("{HOOK_RUN_COMMAND} post_tool_use")),
            "{parsed:?}"
        );
    }

    /// Every session-log event Codex registers has to be one Codex actually
    /// accepts, or the hook looks wired and never fires.
    #[test]
    fn session_log_events_are_all_supported_by_codex() {
        for (_, native) in SESSION_LOG_HOOK_EVENTS {
            assert!(
                SUPPORTED_HOOK_EVENTS.contains(native),
                "{native} is not a Codex event"
            );
        }
    }

    /// Tool names an anchored-alternation matcher (`^Read$`, `^(A|B)$`)
    /// accepts. Every `pre_tool_use` matcher llmenv registers has that shape;
    /// one that doesn't fails here rather than being silently mis-parsed into
    /// an empty set that trivially satisfies the disjointness assertion.
    fn tools_accepted_by(matcher: &str) -> Vec<String> {
        let body = matcher
            .strip_prefix('^')
            .and_then(|m| m.strip_suffix('$'))
            .unwrap_or_else(|| panic!("matcher {matcher} is not anchored"));
        let body = body
            .strip_prefix('(')
            .and_then(|b| b.strip_suffix(')'))
            .unwrap_or(body);
        assert!(
            body.chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '|'),
            "matcher {matcher} is not a literal alternation this helper can read"
        );
        body.split('|').map(str::to_owned).collect()
    }

    /// #1442: a single tool call must reach `hook-run pre_tool_use` exactly
    /// once. Mirrors `claude_code`'s test of the same name — both adapters
    /// merge the same two sources of `pre_tool_use` registrations (the
    /// matcher-scoped read-once hook and session logging's unmatched capture
    /// hook), and the bug reached Codex precisely because the gate was
    /// per-adapter.
    #[test]
    fn pre_tool_use_reaches_hook_run_once_per_tool() {
        for bits in 0..4u8 {
            let (session_log, task_tracker) = (bits & 1 != 0, bits & 2 != 0);
            let label = format!("session_log={session_log} task_tracker={task_tracker}");
            let mut manifest = MergedManifest::default();
            if !session_log {
                manifest.session_log.file = None;
                manifest.session_log.transcript = None;
            }
            manifest.capabilities.features = Some(llmenv_config::Features {
                task_tracker: Some(llmenv_config::TaskTracker {
                    enabled: task_tracker,
                    ..Default::default()
                }),
                ..Default::default()
            });

            let (_dir, parsed) = materialize_to_toml(&manifest);
            let mut unmatched = 0usize;
            let mut matched: Vec<Vec<String>> = Vec::new();
            for group in parsed["hooks"]["events"]
                .get("PreToolUse")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
            {
                let registers = group
                    .get("hooks")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|h| h.get("command").and_then(toml::Value::as_str))
                    .any(|c| c.ends_with(" pre_tool_use"));
                if !registers {
                    continue;
                }
                match group.get("matcher").and_then(toml::Value::as_str) {
                    None => unmatched += 1,
                    Some(m) => matched.push(tools_accepted_by(m)),
                }
            }

            assert!(
                unmatched <= 1,
                "{label}: {unmatched} every-tool pre_tool_use registrations: {parsed:?}"
            );
            assert!(
                unmatched == 0 || matched.is_empty(),
                "{label}: an every-tool registration already covers {matched:?}, \
                 so those tools fire pre_tool_use twice: {parsed:?}"
            );
            // Lower bound (#1442 P2): an upper bound alone passes at zero
            // registrations too. The session-log-off case is covered by
            // `read_once_matcher_survives_when_session_logging_is_off`.
            if session_log {
                assert_eq!(
                    unmatched, 1,
                    "{label}: session logging is on but no every-tool \
                     pre_tool_use registration exists: {parsed:?}"
                );
            }
            for (i, a) in matched.iter().enumerate() {
                for b in &matched[i + 1..] {
                    let overlap: Vec<&String> = a.iter().filter(|t| b.contains(t)).collect();
                    assert!(
                        overlap.is_empty(),
                        "{label}: {overlap:?} matches two pre_tool_use registrations"
                    );
                }
            }
        }
    }

    /// #1442: with session logging off nothing registers the unmatched capture
    /// hook, so the matcher-scoped read-once registration is what has to keep
    /// `Read` reaching `hook-run` — the dedup gate must not drop it outright.
    #[test]
    fn read_once_matcher_survives_when_session_logging_is_off() {
        let (_dir, parsed) = materialize_to_toml(&manifest_without_session_log());
        assert!(
            hook_commands_for(&parsed, "PreToolUse")
                .contains(&format!("{HOOK_RUN_COMMAND} pre_tool_use")),
            "{parsed:?}"
        );
    }

    /// The gates `doctor` reports must match what this adapter actually writes,
    /// the same way `claude_code`'s equivalent test keeps its two halves honest.
    ///
    /// Enumerates every combination of the four inputs the gates read rather
    /// than a handful of fixtures: with hand-picked cases a gate that quietly
    /// starts requiring two enablers instead of one still passes, because no
    /// fixture isolates the second.
    #[test]
    fn lifecycle_registrations_match_the_rendered_hooks() {
        for bits in 0..16u8 {
            let (session_log, task_tracker, self_critique, memory) =
                (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            let label = format!(
                "session_log={session_log} task_tracker={task_tracker} \
                 self_critique={self_critique} memory={memory}"
            );
            let mut manifest = if memory {
                manifest_with_memory_mcp()
            } else {
                MergedManifest::default()
            };
            if !session_log {
                manifest.session_log.file = None;
                manifest.session_log.transcript = None;
            }
            manifest.capabilities.features = Some(llmenv_config::Features {
                task_tracker: Some(llmenv_config::TaskTracker {
                    enabled: task_tracker,
                    ..Default::default()
                }),
                slippage: Some(llmenv_config::SlippageControl {
                    enabled: true,
                    self_critique,
                    ..Default::default()
                }),
                ..Default::default()
            });

            let (_dir, parsed) = materialize_to_toml(&manifest);
            let commands: Vec<String> = parsed["hooks"]["events"]
                .as_table()
                .unwrap()
                .keys()
                .flat_map(|event| hook_commands_for(&parsed, event))
                .collect();
            for (event, registered, why) in crate::adapter::lifecycle_hook_registrations(&manifest)
            {
                // Space-delimited so `stop` doesn't also match `subagent_stop`.
                let suffix = format!(" {event}");
                let present = commands
                    .iter()
                    .any(|c| c.contains("hook-run") && c.ends_with(&suffix));
                assert_eq!(
                    present, registered,
                    "{label}: doctor says {event} registered={registered} ({why}), \
                     config.toml disagrees: {commands:?}"
                );
            }
        }
    }

    /// The `--engine` value is validated at the CLI boundary (#1386), so it has
    /// to name a registered adapter or every hook fails at runtime.
    #[test]
    fn baseline_hook_commands_name_a_registered_engine() {
        assert!(
            crate::adapter::require_known_engine("codex").is_ok(),
            "the engine id baked into the hook commands must resolve"
        );
    }

    /// Throttle hooks only appear when the manifest asks for them.
    #[test]
    fn throttle_hooks_are_gated_on_the_manifest() {
        let manifest = MergedManifest::default();
        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(
            !format!("{:?}", parsed["hooks"]).contains("llmenv throttle"),
            "throttle must not be wired when the manifest has none"
        );
    }

    /// `Notification` is Claude-only — `HookEventsToml` has no field for it. An
    /// unknown key would be ignored silently, so the hook would look wired and
    /// never fire; skipping it with a warning is the honest outcome.
    #[test]
    fn an_event_codex_lacks_is_skipped_not_emitted() {
        let mut manifest = MergedManifest::default();
        manifest
            .capabilities
            .hooks
            .push(command_hook("Notification", None, "echo hi"));
        manifest
            .capabilities
            .hooks
            .push(command_hook("SessionStart", None, "echo start"));

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let events = parsed["hooks"]["events"].as_table().unwrap();

        assert!(
            !events.contains_key("Notification"),
            "Codex has no Notification event: {events:?}"
        );
        assert!(events.contains_key("SessionStart"), "{events:?}");
    }

    /// Codex's handler enum is tagged `type` with a `command` variant only.
    #[test]
    fn mcp_tool_hooks_are_skipped() {
        let mut manifest = MergedManifest::default();
        let mut hook = command_hook("PreToolUse", None, "unused");
        hook.handler.kind = crate::config::HookHandlerKind::McpTool;
        hook.handler.command = None;
        hook.handler.tool = Some("some_tool".into());
        manifest.capabilities.hooks.push(hook);

        let (_dir, parsed) = materialize_to_toml(&manifest);
        // The baseline hooks still render; the mcp_tool one must not appear.
        let rendered = format!("{:?}", parsed["hooks"]);
        assert!(
            !rendered.contains("some_tool"),
            "the mcp_tool hook must be skipped: {rendered}"
        );
    }

    #[test]
    fn native_codex_cannot_clobber_the_rendered_hooks() {
        let mut manifest = MergedManifest::default();
        manifest
            .capabilities
            .hooks
            .push(command_hook("SessionStart", None, "echo start"));
        manifest.native.insert(
            "codex".into(),
            serde_yaml::from_str("hooks:\n  events: {}\n").unwrap(),
        );

        let dir = tempfile::tempdir().unwrap();
        let err = CodexAdapter.materialize(&manifest, dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("hooks"), "{err:#}");
    }

    /// A `deny Bash` rule has no Codex permission-profile equivalent at all
    /// (#1102's `mappable_fs_access` returns `None` for it), so it must not
    /// silently evaporate into an empty render — it's warned about instead.
    #[test]
    fn dropped_permissions_are_announced_not_swallowed() {
        use crate::config::{PermissionRule, Permissions};

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            deny: vec![PermissionRule {
                tool: "Bash".into(),
                pattern: Some("curl *".into()),
                ..PermissionRule::default()
            }],
            ..Permissions::default()
        };

        // The warning goes to stderr, which a unit test can't capture without a
        // harness. Assert the condition that triggers it instead, so the test
        // still fails if this slice ever half-renders permissions.
        let dir = tempfile::tempdir().unwrap();
        CodexAdapter.materialize(&manifest, dir.path()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(
            !raw.contains("permissions") && !raw.contains("approval_policy"),
            "this slice must not half-render permissions — the warning is the contract: {raw}"
        );
    }

    // ---- classify_permission_profile / permission-profile rendering (#1102) ----

    fn fs_rule(tool: &str, paths: &[&str]) -> crate::config::PermissionRule {
        crate::config::PermissionRule {
            tool: tool.into(),
            paths: paths.iter().map(|p| (*p).to_string()).collect(),
            ..crate::config::PermissionRule::default()
        }
    }

    /// Every declared rule maps cleanly onto a filesystem entry, so the
    /// profile renders and is activated via `default_permissions`.
    #[test]
    fn clean_filesystem_rules_render_and_activate_a_profile() {
        use crate::config::Permissions;

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            allow: vec![
                fs_rule("Read", &["/repo/docs"]),
                fs_rule("Write", &["/repo/src"]),
            ],
            ..Permissions::default()
        };

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let profile = parsed["permissions"]["llmenv"].as_table().unwrap();
        let filesystem = profile["filesystem"].as_table().unwrap();

        assert_eq!(filesystem["/repo/docs"].as_str(), Some("read"));
        assert_eq!(filesystem["/repo/src"].as_str(), Some("write"));
        assert!(profile.contains_key("description"), "{profile:?}");
        assert_eq!(parsed["default_permissions"].as_str(), Some("llmenv"));
    }

    /// `Edit`/`MultiEdit` both imply write access, same as `Write`.
    #[test]
    fn edit_and_multiedit_rules_render_as_write_access() {
        use crate::config::Permissions;

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            allow: vec![
                fs_rule("Edit", &["/repo/a"]),
                fs_rule("MultiEdit", &["/repo/b"]),
            ],
            ..Permissions::default()
        };

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let filesystem = parsed["permissions"]["llmenv"]["filesystem"]
            .as_table()
            .unwrap();
        assert_eq!(filesystem["/repo/a"].as_str(), Some("write"));
        assert_eq!(filesystem["/repo/b"].as_str(), Some("write"));
    }

    /// Codex's own stated precedence (deny beats write beats read): a `deny`
    /// rule on a path already granted `allow` must win.
    #[test]
    fn deny_wins_over_allow_at_the_same_path() {
        use crate::config::Permissions;

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            allow: vec![fs_rule("Write", &["/repo/secret"])],
            deny: vec![fs_rule("Read", &["/repo/secret"])],
            ..Permissions::default()
        };

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let filesystem = parsed["permissions"]["llmenv"]["filesystem"]
            .as_table()
            .unwrap();
        assert_eq!(
            filesystem["/repo/secret"].as_str(),
            Some("deny"),
            "deny must win regardless of which tool declared it: {filesystem:?}"
        );
    }

    /// Regression for a security-audit finding (#1102): a `deny` and an
    /// `allow` naming the same directory with different but equivalent
    /// spellings (`./repo/secret/` vs `repo/secret`) must still collide at
    /// one entry, or `deny`-wins silently fails to apply just because the
    /// two rules were written differently.
    #[test]
    fn deny_wins_even_when_paths_are_spelled_differently() {
        use crate::config::Permissions;

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            allow: vec![fs_rule("Write", &["repo/secret"])],
            deny: vec![fs_rule("Read", &["./repo/secret/"])],
            ..Permissions::default()
        };

        let (_dir, parsed) = materialize_to_toml(&manifest);
        let filesystem = parsed["permissions"]["llmenv"]["filesystem"]
            .as_table()
            .unwrap();
        assert_eq!(
            filesystem.len(),
            1,
            "equivalent spellings of the same path must collide at one entry: {filesystem:?}"
        );
        assert_eq!(
            filesystem["repo/secret"].as_str(),
            Some("deny"),
            "deny must win even when the allow/deny rules spell the path differently: {filesystem:?}"
        );
    }

    /// An `ask`-tier rule has no per-rule Codex equivalent at all, so it
    /// disqualifies the whole profile even though it names a normally
    /// mappable tool.
    #[test]
    fn ask_tier_rule_refuses_the_whole_profile() {
        use crate::config::Permissions;

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            allow: vec![fs_rule("Read", &["/repo/docs"])],
            ask: vec![fs_rule("Write", &["/repo/src"])],
            ..Permissions::default()
        };

        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(
            !parsed.contains_key("permissions"),
            "an ask-tier rule must refuse the entire profile, including the otherwise-mappable \
             allow rule: {parsed:?}"
        );
        assert!(!parsed.contains_key("default_permissions"));
    }

    /// A `WebFetch` rule has no Codex equivalent in this slice (network
    /// domains are deliberately unmodeled — see the module doc), so it
    /// refuses the whole profile per the all-or-nothing rule, even alongside
    /// an otherwise-mappable filesystem rule.
    #[test]
    fn webfetch_rule_refuses_the_whole_profile() {
        use crate::config::{PermissionRule, Permissions};

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            allow: vec![
                fs_rule("Read", &["/repo/docs"]),
                PermissionRule {
                    tool: "WebFetch".into(),
                    pattern: Some("example.com".into()),
                    ..PermissionRule::default()
                },
            ],
            ..Permissions::default()
        };

        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(!parsed.contains_key("permissions"), "{parsed:?}");
    }

    /// A `Read`/`Edit`/`Write`/`MultiEdit` rule using `pattern` instead of
    /// `paths` doesn't fit this slice's path-based rendering, so it must not
    /// be silently ignored or half-rendered — it refuses the profile.
    #[test]
    fn filesystem_tool_with_pattern_instead_of_paths_refuses_the_profile() {
        use crate::config::{PermissionRule, Permissions};

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            allow: vec![PermissionRule {
                tool: "Read".into(),
                pattern: Some("*.rs".into()),
                ..PermissionRule::default()
            }],
            ..Permissions::default()
        };

        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(!parsed.contains_key("permissions"), "{parsed:?}");
    }

    /// `native.codex` cannot redirect the rendered permission-profile keys any
    /// more than it can redirect `mcp_servers`/`hooks`/`model_instructions_file`
    /// — both are llmenv's own render surface once #1102 models them.
    #[test]
    fn native_codex_cannot_clobber_the_rendered_permissions() {
        use crate::config::Permissions;

        let mut manifest = MergedManifest::default();
        manifest.capabilities.permissions = Permissions {
            allow: vec![fs_rule("Read", &["/repo/docs"])],
            ..Permissions::default()
        };
        manifest.native.insert(
            "codex".into(),
            serde_yaml::from_str("default_permissions: \"attacker-profile\"\n").unwrap(),
        );

        let dir = tempfile::tempdir().unwrap();
        let err = CodexAdapter.materialize(&manifest, dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("default_permissions"),
            "the catch-all must be rejected by name: {err:#}"
        );
    }

    /// The rules pipeline is not redirectable through the catch-all — the same
    /// contract opencode enforces for its `instructions` key.
    #[test]
    fn native_codex_cannot_redirect_the_instructions_file() {
        let mut manifest = MergedManifest {
            agents_md: "# Rules\n".into(),
            ..MergedManifest::default()
        };
        manifest.native.insert(
            "codex".into(),
            serde_yaml::from_str("model_instructions_file: /tmp/attacker.md\n").unwrap(),
        );

        let dir = tempfile::tempdir().unwrap();
        let err = CodexAdapter.materialize(&manifest, dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("model_instructions_file"), "{msg}");
        assert!(
            msg.contains("rules"),
            "the error must point at capabilities.rules, not invent a field: {msg}"
        );
    }

    #[test]
    fn native_codex_cannot_clobber_the_rendered_mcp_servers() {
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(stdio_mcp("icm"));
        manifest.native.insert(
            "codex".into(),
            serde_yaml::from_str("mcp_servers:\n  evil:\n    command: rm\n").unwrap(),
        );

        let dir = tempfile::tempdir().unwrap();
        let err = CodexAdapter.materialize(&manifest, dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("mcp_servers"),
            "the catch-all must be rejected by name: {err:#}"
        );
    }

    // ---- apply_seeded_settings (#1107) ----

    #[test]
    fn seeded_settings_are_merged_into_config_toml_when_absent() {
        let mut manifest = MergedManifest::default();
        manifest.mcps.push(stdio_mcp("icm"));
        let (dir, _parsed) = materialize_to_toml(&manifest);

        let mut seeded = serde_json::Map::new();
        seeded.insert("model".to_string(), serde_json::json!("o3"));
        super::apply_seeded_settings(dir.path(), &seeded).unwrap();

        let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        let parsed: toml::Table = raw.parse().unwrap();
        assert_eq!(parsed["model"].as_str(), Some("o3"));
        assert!(
            parsed.contains_key("mcp_servers"),
            "seeding must not drop what materialize already rendered"
        );
    }

    #[test]
    fn seeded_settings_never_overwrite_an_existing_value() {
        let manifest = MergedManifest::default();
        let (dir, _parsed) = materialize_to_toml(&manifest);
        let path = dir.path().join(super::CODEX_CONFIG_FILE);
        std::fs::write(&path, "model = \"already-set\"\n").unwrap();

        let mut seeded = serde_json::Map::new();
        seeded.insert("model".to_string(), serde_json::json!("o3"));
        super::apply_seeded_settings(dir.path(), &seeded).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = raw.parse().unwrap();
        assert_eq!(
            parsed["model"].as_str(),
            Some("already-set"),
            "must not clobber a value the folder already has"
        );
    }

    /// The rules pipeline is not seedable through `init.seeded_settings` any
    /// more than it is overridable through `native.codex` — both are llmenv's
    /// own render surface.
    #[test]
    fn seeded_settings_cannot_redirect_a_modeled_key() {
        let mut manifest = MergedManifest {
            agents_md: "# Rules\n".into(),
            ..MergedManifest::default()
        };
        let (dir, parsed_before) = materialize_to_toml(&manifest);
        manifest.mcps.push(stdio_mcp("icm"));

        let mut seeded = serde_json::Map::new();
        seeded.insert(
            "model_instructions_file".to_string(),
            serde_json::json!("/tmp/attacker.md"),
        );
        super::apply_seeded_settings(dir.path(), &seeded).unwrap();

        let raw = std::fs::read_to_string(dir.path().join(super::CODEX_CONFIG_FILE)).unwrap();
        let parsed_after: toml::Table = raw.parse().unwrap();
        assert_eq!(
            parsed_after["model_instructions_file"].as_str(),
            parsed_before["model_instructions_file"].as_str(),
            "a modeled key must not be seedable"
        );
    }

    #[test]
    fn seeded_settings_noop_when_config_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut seeded = serde_json::Map::new();
        seeded.insert("model".to_string(), serde_json::json!("o3"));

        super::apply_seeded_settings(dir.path(), &seeded).unwrap();

        assert!(!dir.path().join(super::CODEX_CONFIG_FILE).exists());
    }

    #[test]
    fn seeded_settings_empty_map_is_a_noop() {
        let manifest = MergedManifest::default();
        let (dir, _parsed) = materialize_to_toml(&manifest);
        let path = dir.path().join(super::CODEX_CONFIG_FILE);
        let before = std::fs::read_to_string(&path).unwrap();

        super::apply_seeded_settings(dir.path(), &serde_json::Map::new()).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// A security-sensitive key (#1102 permissions are unmodeled, so these
    /// aren't in `CODEX_MODELED_KEYS`) must never be seedable — otherwise
    /// `init.seeded_settings` could silently run Codex less restrictively than
    /// `capabilities.permissions` establishes on every other engine.
    #[test]
    fn seeded_settings_cannot_seed_a_security_sensitive_key() {
        let manifest = MergedManifest::default();
        let (dir, _parsed) = materialize_to_toml(&manifest);

        let mut seeded = serde_json::Map::new();
        seeded.insert("approval_policy".to_string(), serde_json::json!("never"));
        seeded.insert(
            "sandbox_mode".to_string(),
            serde_json::json!("danger-full-access"),
        );
        super::apply_seeded_settings(dir.path(), &seeded).unwrap();

        let raw = std::fs::read_to_string(dir.path().join(super::CODEX_CONFIG_FILE)).unwrap();
        let parsed: toml::Table = raw.parse().unwrap();
        assert!(
            !parsed.contains_key("approval_policy"),
            "approval_policy must never be seedable: {parsed:?}"
        );
        assert!(
            !parsed.contains_key("sandbox_mode"),
            "sandbox_mode must never be seedable: {parsed:?}"
        );
    }

    proptest! {
        /// `apply_seeded_settings` never crashes on an arbitrary map of
        /// TOML-representable scalar values, always produces re-parseable
        /// TOML, is idempotent, and never lets an arbitrary key collide with
        /// `CODEX_MODELED_KEYS` or `CODEX_SECURITY_SENSITIVE_KEYS` (pbt-gap,
        /// #1421).
        #[test]
        fn prop_apply_seeded_settings_holds_its_invariants(
            entries in proptest::collection::vec(
                (
                    "[a-z][a-z0-9_]{0,12}",
                    prop_oneof![
                        any::<bool>().prop_map(|b| serde_json::json!(b)),
                        any::<i32>().prop_map(|n| serde_json::json!(n)),
                        "[a-zA-Z0-9 _.-]{0,20}".prop_map(|s| serde_json::json!(s)),
                    ],
                ),
                0..8,
            )
        ) {
            let manifest = MergedManifest::default();
            let (dir, _parsed) = materialize_to_toml(&manifest);
            let path = dir.path().join(super::CODEX_CONFIG_FILE);

            let mut seeded = serde_json::Map::new();
            for (k, v) in entries {
                seeded.insert(k, v);
            }

            super::apply_seeded_settings(dir.path(), &seeded).unwrap();
            let once = std::fs::read_to_string(&path).unwrap();
            let parsed_once: toml::Table = once.parse().expect("output must always be valid TOML");

            for key in super::CODEX_MODELED_KEYS {
                prop_assert!(
                    !seeded.contains_key(*key) || !parsed_once.contains_key(*key)
                        || seeded.get(*key) != Some(&serde_json::json!(parsed_once[*key])),
                    "a seeded modeled key must never override materialize's own render: {key}"
                );
            }
            for key in super::CODEX_SECURITY_SENSITIVE_KEYS {
                if seeded.contains_key(*key) {
                    prop_assert!(
                        !parsed_once.contains_key(*key),
                        "a security-sensitive key must never be seeded: {key}"
                    );
                }
            }

            // Idempotence: applying the same settings again changes nothing.
            super::apply_seeded_settings(dir.path(), &seeded).unwrap();
            let twice = std::fs::read_to_string(&path).unwrap();
            prop_assert_eq!(once, twice, "applying the same seeded settings twice must be a no-op the second time");
        }

        /// `classify_permission_profile` never panics on an arbitrary rule
        /// set, and the all-or-nothing gate never leaks a partial result: it
        /// is `Refused` iff some rule is `ask`-tier, uses `pattern` instead of
        /// `paths`, or names a tool with no filesystem equivalent — the same
        /// invariant `warn_about_unrenderable_capabilities`'s reported counts
        /// depend on being exact (pbt-gap, #1106).
        #[test]
        fn prop_classify_permission_profile_holds_its_invariants(
            rules in proptest::collection::vec(
                (
                    prop_oneof![Just("allow"), Just("ask"), Just("deny")],
                    prop_oneof![
                        Just("Read"), Just("Edit"), Just("Write"), Just("MultiEdit"),
                        Just("Bash"), Just("WebFetch"),
                    ],
                    proptest::collection::vec("[a-z/]{1,10}", 0..3),
                    proptest::option::of("[a-z*]{1,10}"),
                ),
                0..8,
            )
        ) {
            use crate::config::{PermissionRule, Permissions};

            let mut perms = Permissions::default();
            let mut expect_refused = false;
            for (tier, tool, paths, pattern) in &rules {
                let rule = PermissionRule {
                    tool: (*tool).to_string(),
                    pattern: pattern.clone(),
                    paths: paths.clone(),
                };
                let mappable_tool = matches!(*tool, "Read" | "Edit" | "Write" | "MultiEdit");
                let clean = mappable_tool && !paths.is_empty() && pattern.is_none();
                if *tier == "ask" || !clean {
                    expect_refused = true;
                }
                match *tier {
                    "allow" => perms.allow.push(rule),
                    "ask" => perms.ask.push(rule),
                    _ => perms.deny.push(rule),
                }
            }
            let total = perms.allow.len() + perms.ask.len() + perms.deny.len();

            let decision = super::classify_permission_profile(&perms);

            if total == 0 {
                prop_assert_eq!(decision, super::PermissionProfileDecision::Empty);
            } else if expect_refused {
                match decision {
                    super::PermissionProfileDecision::Refused { unmappable_rule_count, mappable_rule_count } => {
                        prop_assert_eq!(
                            unmappable_rule_count + mappable_rule_count,
                            total,
                            "reported counts must always sum to the total rule count"
                        );
                    }
                    other => prop_assert!(
                        false,
                        "expected Refused for a rule set with an ask/pattern/unmappable rule, got {:?}",
                        other
                    ),
                }
            } else {
                match decision {
                    super::PermissionProfileDecision::Rendered(entries) => {
                        // Deny always wins: any path a deny rule touches must
                        // resolve to Deny in the rendered map, regardless of
                        // what else also touched that path.
                        for (tier, _tool, paths, _pattern) in &rules {
                            if *tier != "deny" {
                                continue;
                            }
                            for path in paths {
                                let key = super::normalize_permission_path(path);
                                if let Some(access) = entries.get(&key) {
                                    prop_assert_eq!(
                                        *access,
                                        super::CodexFsAccess::Deny,
                                        "a deny rule touching {} must win",
                                        key
                                    );
                                }
                            }
                        }
                    }
                    other => prop_assert!(
                        false,
                        "expected Rendered for an all-clean rule set, got {:?}",
                        other
                    ),
                }
            }
        }
    }

    #[test]
    fn emitted_config_is_parseable_with_both_servers_and_instructions() {
        let mut manifest = MergedManifest {
            agents_md: "# Rules\n".into(),
            ..MergedManifest::default()
        };
        manifest.mcps.push(stdio_mcp("icm"));
        manifest.mcps.push(remote_mcp("remote", McpTransport::Http));

        // The parse inside the helper is the assertion that matters: a scalar
        // emitted after a table would be swallowed into it, which is why the
        // top-level key order is left to the serializer.
        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(parsed.contains_key("model_instructions_file"));
        assert_eq!(parsed["mcp_servers"].as_table().unwrap().len(), 2);
    }

    // ---- skills registration (#1106) ----

    const VALID_SKILL_FRONTMATTER: &str = "---\nname: x\ndescription: y\n---\nbody\n";

    /// Codex has no auto-discovery for skills — a first-class skill must be
    /// both written under `out/skills/` and explicitly registered via
    /// `[[skills.config]]`, naming its absolute materialized path.
    #[test]
    fn first_class_skill_is_written_and_registered() {
        let src_tmp = tempfile::tempdir().unwrap();
        let skill_src = src_tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_src).unwrap();
        std::fs::write(skill_src.join("SKILL.md"), VALID_SKILL_FRONTMATTER).unwrap();

        let mut manifest = MergedManifest::default();
        manifest
            .capabilities
            .skills
            .push(crate::config::SkillSource {
                name: "my-skill".into(),
                path: skill_src.to_str().unwrap().into(),
                when: Vec::new(),
            });

        let (dir, parsed) = materialize_to_toml(&manifest);
        assert!(dir.path().join("skills/my-skill/SKILL.md").exists());

        let entries = parsed["skills"]["config"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let expected_path = dir.path().join("skills/my-skill");
        assert_eq!(
            entries[0]["path"].as_str(),
            expected_path.to_str(),
            "{entries:?}"
        );
        assert_eq!(entries[0]["enabled"].as_bool(), Some(true));
    }

    /// The built-in `llmenv` skill materializes and registers the same way a
    /// first-class skill does, once a first-party feature is enabled.
    #[test]
    fn builtin_llmenv_skill_is_written_and_registered_when_a_feature_is_enabled() {
        let mut manifest = MergedManifest::default();
        manifest.capabilities.features = Some(crate::config::Features {
            context_mode: Some(crate::config::ContextMode {
                enabled: true,
                ..crate::config::ContextMode::default()
            }),
            ..crate::config::Features::default()
        });

        let (dir, parsed) = materialize_to_toml(&manifest);
        assert!(dir.path().join("skills/llmenv/SKILL.md").exists());

        let entries = parsed["skills"]["config"].as_array().unwrap();
        assert!(
            entries.iter().any(|e| e["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("skills/llmenv"))),
            "{entries:?}"
        );
    }

    /// No skills at all (no first-class sources, no first-party feature
    /// enabled) must omit the `skills` key entirely, not emit an empty
    /// `[[skills.config]]` array.
    #[test]
    fn no_skills_omits_the_skills_key() {
        let manifest = MergedManifest::default();
        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(!parsed.contains_key("skills"), "{parsed:?}");
    }

    proptest! {
        /// `render_skills_config` returns exactly one entry per directory
        /// under `out/skills/` (files excluded), in sorted order, and is
        /// deterministic across repeated calls on the same directory
        /// (pbt-gap, #1106).
        #[test]
        fn prop_render_skills_config_counts_and_sorts_directories(
            names in proptest::collection::hash_set("[a-z][a-z0-9_-]{0,8}", 0..8),
            is_dir_flags in proptest::collection::vec(any::<bool>(), 0..8),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let skills_dir = dir.path().join("skills");
            std::fs::create_dir_all(&skills_dir).unwrap();

            let mut expected_dir_names: Vec<String> = Vec::new();
            for (name, is_dir) in names.iter().zip(is_dir_flags.iter().cycle()) {
                let entry_path = skills_dir.join(name);
                if *is_dir {
                    std::fs::create_dir(&entry_path).unwrap();
                    expected_dir_names.push(name.clone());
                } else {
                    std::fs::write(&entry_path, b"not a directory").unwrap();
                }
            }
            expected_dir_names.sort();

            let once = super::render_skills_config(dir.path()).unwrap();
            prop_assert_eq!(
                once.len(),
                expected_dir_names.len(),
                "must return exactly one entry per directory, excluding files"
            );

            let paths: Vec<String> = once
                .iter()
                .map(|e| e["path"].as_str().unwrap().to_string())
                .collect();
            let mut sorted_paths = paths.clone();
            sorted_paths.sort();
            prop_assert_eq!(&paths, &sorted_paths, "entries must be sorted");

            for entry in &once {
                prop_assert_eq!(entry["enabled"].as_bool(), Some(true));
            }

            // Determinism: calling it again on the same directory is identical.
            let twice = super::render_skills_config(dir.path()).unwrap();
            prop_assert_eq!(once, twice, "must be deterministic across repeated calls");
        }
    }

    /// `native.codex` cannot redirect the rendered skills registration any
    /// more than it can redirect `mcp_servers`/`hooks`/`permissions`.
    #[test]
    fn native_codex_cannot_clobber_the_rendered_skills() {
        let mut manifest = MergedManifest::default();
        manifest.capabilities.features = Some(crate::config::Features {
            context_mode: Some(crate::config::ContextMode {
                enabled: true,
                ..crate::config::ContextMode::default()
            }),
            ..crate::config::Features::default()
        });
        manifest.native.insert(
            "codex".into(),
            serde_yaml::from_str("skills:\n  config: []\n").unwrap(),
        );

        let dir = tempfile::tempdir().unwrap();
        let err = CodexAdapter.materialize(&manifest, dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("skills"),
            "the catch-all must be rejected by name: {err:#}"
        );
    }
}
