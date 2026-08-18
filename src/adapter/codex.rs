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
//! - permissions (#1102) — Codex models these as named profiles under
//!   `permissions.entries` plus `approval_policy`/`sandbox_mode`, which does not
//!   map onto llmenv's `allow`/`ask`/`deny` lists. Needs a design decision, not
//!   a translation.
//! - lifecycle hooks (#1108) — Codex's `hooks.events.*` takes the same
//!   matcher-group shape as Claude Code and its event names match, so this is
//!   smaller than it looks, but it is not wired yet.
//! - statusline (#1104), auth (#1105), plugins/skills (#1106), rules beyond the
//!   merged AGENTS.md (#1103), doctor diagnostics (#1100).
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
const CODEX_CONFIG_FILE: &str = "config.toml";

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
const CODEX_MODELED_KEYS: &[&str] = &["mcp_servers", "model_instructions_file", "hooks"];

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
        // #1106.
        false
    }

    fn supports_lsp(&self) -> bool {
        // Codex has no LSP config block.
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

        warn_about_unrenderable_capabilities(manifest);

        let mut doc = serde_json::Map::new();

        // AGENTS.md, pointed at explicitly. Codex discovers a project's
        // AGENTS.md on its own, but this one is llmenv's merged output living in
        // the cache dir, which is not a project root — `model_instructions_file`
        // is the field that takes an absolute path to it.
        super::skills::reject_hardcoded_config_path(&manifest.agents_md, "AGENTS.md")?;
        if !manifest.agents_md.trim().is_empty() {
            let agents_path = out.join("AGENTS.md");
            crate::paths::write_owner_only(&agents_path, manifest.agents_md.as_bytes())?;
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
///
/// Permissions get a warning for a stronger reason than hooks do: a `deny` rule
/// that evaporates leaves Codex running under its own default
/// `approval_policy`/`sandbox_mode`, so the posture is silently weaker on this
/// engine than on the one the bundle was tested against.
fn warn_about_unrenderable_capabilities(manifest: &MergedManifest) {
    let perms = &manifest.capabilities.permissions;
    let rule_count = perms.allow.len() + perms.ask.len() + perms.deny.len();
    if rule_count > 0 {
        eprintln!(
            "warning: the Codex adapter does not render permissions yet — {rule_count} rule(s), \
             including {} deny rule(s), will NOT constrain Codex, which runs under its own \
             default approval policy and sandbox mode. Tracking issue: \
             https://github.com/phaedrus1992/llmenv/issues/1102",
            perms.deny.len()
        );
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

    // Read-once dedup (#318). Registered unconditionally: the `hook-run`
    // handler checks `features.read_once.enabled` and passes through when off,
    // so the matcher is the only cost when the feature is disabled.
    by_event
        .entry("PreToolUse".into())
        .or_default()
        .push(matched_group(
            "^Read$",
            format!("{HOOK_RUN_COMMAND} pre_tool_use"),
        ));

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
}

/// Render `manifest.mcps` into Codex's `mcp_servers` table.
///
/// The shape is Codex's `RawMcpServerConfig`, which is what `config.toml` is
/// actually deserialized into: the transport is `#[serde(flatten)]`ed and
/// `untagged`, so there is **no** `type` key — a `command` means stdio and a
/// `url` means streamable HTTP. Emitting a `type` field would be rejected
/// outright, since the transport enum is `deny_unknown_fields`.
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

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test scaffolding")]
mod tests {
    use super::{CodexAdapter, render_mcp_servers};
    use crate::adapter::AgentAdapter;
    use crate::config::McpTransport;
    use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
    use crate::merge::MergedManifest;
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

    /// A `deny` rule that silently evaporates leaves Codex running under its own
    /// default approval policy — a weaker posture than the engine the bundle was
    /// tested against, with nothing to say so. Warned about until #1102 lands.
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
}
