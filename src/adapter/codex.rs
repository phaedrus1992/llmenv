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

/// No lifecycle hooks are wired yet (#1108). Declared empty rather than
/// optimistically listing Codex's event names: claiming support llmenv doesn't
/// render would silently drop every hook a bundle declares.
const SUPPORTED_HOOK_EVENTS: &[&str] = &[];

/// Keys this adapter renders itself, and which a `native.codex` catch-all
/// fragment must therefore not overwrite.
const CODEX_MODELED_KEYS: &[&str] = &["mcp_servers"];

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
        &[nk::NATIVE_MCP, nk::NATIVE]
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

        warn_about_unrenderable_hooks(manifest);

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
            owned.push(agents_path);
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
            .context_for_codex("failed to render Codex config.toml")?;
        let out_path = out.join(CODEX_CONFIG_FILE);
        crate::paths::write_owner_only(&out_path, rendered.as_bytes())?;
        owned.push(out_path);

        Ok(owned)
    }

    fn emit_hook_context(&self, hook_event_name: &str, text: &str) -> String {
        super::emit_hook_context(hook_event_name, text)
    }
}

/// Adds context to a `toml` serialization error.
///
/// A local trait rather than `anyhow::Context` directly because `toml`'s error
/// type is not `std::error::Error + Send + Sync` in every configuration, and the
/// message matters more than the chain here.
trait CodexTomlContext<T> {
    fn context_for_codex(self, msg: &'static str) -> anyhow::Result<T>;
}

impl<T> CodexTomlContext<T> for Result<T, toml::ser::Error> {
    fn context_for_codex(self, msg: &'static str) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("{msg}: {e}"))
    }
}

/// Warn once per hook that Codex can't express yet, rather than failing the
/// whole render.
///
/// A bundle shared across engines commonly declares hooks for Claude Code. That
/// is a cross-engine gap, not an authoring mistake, and failing here would also
/// drop the MCP servers and AGENTS.md that Codex *can* use — the same reasoning
/// the Crush adapter applies to its own unsupported events.
fn warn_about_unrenderable_hooks(manifest: &MergedManifest) {
    for hook in &manifest.capabilities.hooks {
        eprintln!(
            "warning: the Codex adapter does not wire lifecycle hooks yet \
             (hook event '{}') — skipping it. Tracking issue: \
             https://github.com/phaedrus1992/llmenv/issues/1108",
            hook.event
        );
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
    #[test]
    fn a_native_null_deletes_the_key_instead_of_failing_serialization() {
        let mut manifest = MergedManifest {
            agents_md: "# Rules\n".into(),
            ..MergedManifest::default()
        };
        manifest.native.insert(
            "codex".into(),
            serde_yaml::from_str("model_instructions_file: null\nmodel: o3\n").unwrap(),
        );

        let (_dir, parsed) = materialize_to_toml(&manifest);
        assert!(
            !parsed.contains_key("model_instructions_file"),
            "an explicit null must remove the rendered key"
        );
        assert_eq!(parsed["model"].as_str(), Some("o3"));
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
