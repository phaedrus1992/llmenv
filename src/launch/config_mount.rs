//! Mounts an adapter's materialized config directory into a sandboxed
//! launch's container (#1652, generalized to every adapter in #1698), and
//! rewrites any MCP-server entry's loopback URL (ICM's included) so it
//! resolves from inside the container's own network namespace.
//!
//! Before #1652, `sandbox::container_command` only mounted the project tree,
//! `SSH_AUTH_SOCK`, and the resolved env vars — the config directory itself
//! was never mounted, so a containerized engine saw none of llmenv's
//! materialized MCP servers, skills, plugins, or settings. This mounts that
//! directory read-only at its own path (so the adapter's own config-dir env
//! var stays valid unmodified), and — when any MCP-server entry in the
//! adapter's own MCP-config file (`AgentAdapter::config_dir_mount`'s
//! `mcp_config_file`/`mcp_servers_key`) points at a loopback address, which
//! it does whenever that server runs on this same machine — overlays a
//! patched copy of just that one file with every such URL rewritten to the
//! container gateway host (`sandbox::gateway_host`), mirroring
//! `icebreaker.rs`'s credential-proxy rewrite for the same reason:
//! `127.0.0.1` inside the container is the container itself.
//!
//! Through #1652, this was scoped to Claude Code only, matching
//! `icebreaker.rs`'s then-precedent. #1698 generalized it to every adapter
//! via `AgentAdapter::config_dir_mount` — including, critically, masking
//! whatever runtime-written credential file that adapter keeps outside its
//! own materialized config (`ConfigDirMount::credential_file`): mounting the
//! directory must not also hand the container a live OAuth/API-key store
//! llmenv never authored.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::adapter::{ConfigDirMount, McpConfigFormat};

use super::sandbox::ContainerRuntime;

/// A patched copy of an adapter's MCP-config file, deleted on drop once the
/// container that mounted it has exited — mirrors `sandbox::EnvFileGuard`'s
/// pattern.
pub(crate) struct PatchedFileGuard(PathBuf);

impl PatchedFileGuard {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PatchedFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                "launch: could not remove patched claude.json {}: {e}",
                self.0.display()
            );
        }
    }
}

/// What to bind-mount into a sandboxed launch's container so the adapter
/// sees its materialized config: `config_dir` mounted read-only at the same
/// in-container path, plus (when an MCP-server URL needed rewriting) a
/// [`PatchedFileGuard`] overlay-mounted at `mcp_config_path`, plus (when the
/// adapter has one) a `/dev/null` mask over `credential_file`.
pub(crate) struct ConfigMount {
    pub(crate) config_dir: PathBuf,
    /// Absolute path to the adapter's MCP-config file within `config_dir`
    /// (e.g. `config_dir/.claude.json`) — the overlay-mount target for
    /// `patched_mcp_config`.
    pub(crate) mcp_config_path: PathBuf,
    pub(crate) patched_mcp_config: Option<PatchedFileGuard>,
    /// Absolute path to the adapter's runtime-written credential file within
    /// `config_dir`, if it has one — masked with a `/dev/null` overlay so
    /// the directory mount doesn't also expose a live credential.
    pub(crate) credential_file: Option<PathBuf>,
}

/// Disambiguates concurrent [`write_patched_claude_json`] calls within the
/// same process, mirroring `sandbox::ENV_FILE_COUNTER`.
static PATCH_FILE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Resolve what to mount for this launch. `Ok(None)` when there's nothing to
/// mount: the adapter has no [`ConfigDirMount`] (`mount` is `None`), or no
/// config directory was resolved for this launch (`config_dir` is `None`).
///
/// # Errors
/// Propagates a read/parse/write failure preparing the patched file. A
/// missing or malformed MCP-config file is not an error here — the
/// container still gets the directory mount unpatched;
/// [`patch_mcp_config_loopback_urls`] treats "nothing to patch" as
/// `Ok(None)`, not a failure.
pub(crate) fn prepare(
    mount: Option<&ConfigDirMount>,
    runtime: ContainerRuntime,
    config_dir: Option<&Path>,
) -> anyhow::Result<Option<ConfigMount>> {
    let (Some(mount), Some(config_dir)) = (mount, config_dir) else {
        return Ok(None);
    };

    let mcp_config_path = config_dir.join(mount.mcp_config_file);
    let patched = patch_mcp_config_loopback_urls(
        &mcp_config_path,
        mount.mcp_servers_key,
        mount.format,
        super::sandbox::gateway_host(runtime),
    )?;
    // pre-pr-review P1 (#1652): a rewritten URL is only reachable if the
    // rewritten server actually accepts connections arriving via the
    // container's bridge/gateway interface, not just its own loopback —
    // e.g. ICM's mcp-proxy defaults to `listen_host: 127.0.0.1`
    // (`llmenv_config::Memory::default`), which does NOT satisfy that on
    // native Linux Docker/Podman (only Docker Desktop's host.docker.internal
    // is documented to reach a loopback-only host service). This is the
    // same open question `icebreaker.rs`'s own module doc already flags for
    // its credential proxy ("confirm this end-to-end before relying on it
    // in production") — tracked, not re-solved here, see #1702.
    if patched.is_some() {
        tracing::debug!(
            "launch: rewrote a loopback MCP-server URL to {} for the sandboxed container — this \
             only actually connects if the rewritten server accepts connections on a \
             non-loopback interface (see #1702)",
            super::sandbox::gateway_host(runtime)
        );
    }
    let patched_mcp_config = match patched {
        Some(patched_bytes) => Some(write_patched_mcp_config(&patched_bytes)?),
        None => None,
    };

    Ok(Some(ConfigMount {
        config_dir: config_dir.to_path_buf(),
        mcp_config_path,
        patched_mcp_config,
        credential_file: mount.credential_file.map(|f| config_dir.join(f)),
    }))
}

/// Write `bytes` to a fresh owner-only temp file, returning a guard that
/// deletes it on drop. Mirrors `sandbox::write_env_file`.
fn write_patched_mcp_config(bytes: &[u8]) -> anyhow::Result<PatchedFileGuard> {
    let n = PATCH_FILE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = super::sandbox::sandbox_tmp_dir()?.join(format!(
        "llmenv-sandbox-mcp-config-{}-{n}",
        std::process::id()
    ));
    crate::paths::write_owner_only(&path, bytes).context("writing the patched MCP-config file")?;
    Ok(PatchedFileGuard(path))
}

/// Read `config_path` (if present) and rewrite every entry under
/// `servers_key` whose `url` is a loopback host to `gateway_host`,
/// serialized back to bytes in `format`. Not just ICM's own entry
/// (pre-pr-review P2, #1652) — any plain `mcp:`-declared server with an
/// HTTP/SSE transport has the identical problem: a URL naming this
/// machine's loopback is unreachable from inside the container's own
/// network namespace regardless of which server it is.
///
/// Returns `Ok(None)` when there's nothing to patch: the file doesn't
/// exist, doesn't parse, has no `servers_key` table, or none of its entries
/// have a loopback-host URL in the first place (e.g. every server is remote
/// — already reachable from inside the container exactly as it is on the
/// host).
fn patch_mcp_config_loopback_urls(
    config_path: &Path,
    servers_key: &str,
    format: McpConfigFormat,
    gateway_host: &'static str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let bytes = match std::fs::read(config_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", config_path.display())),
    };
    match format {
        McpConfigFormat::Json => patch_json_loopback_urls(&bytes, servers_key, gateway_host),
        McpConfigFormat::Toml => patch_toml_loopback_urls(&bytes, servers_key, gateway_host),
    }
}

/// [`patch_mcp_config_loopback_urls`]'s JSON-format branch (Claude Code's
/// `mcpServers`, Crush/opencode's `mcp`).
fn patch_json_loopback_urls(
    bytes: &[u8],
    servers_key: &str,
    gateway_host: &'static str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Ok(mut doc) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        // A malformed config file is a hard error elsewhere (materialization
        // refuses to write over it); here it just means there's nothing this
        // function can safely rewrite, so the container gets the file as-is.
        return Ok(None);
    };
    let Some(servers) = doc.get_mut(servers_key).and_then(|v| v.as_object_mut()) else {
        return Ok(None);
    };
    let mut rewrote_any = false;
    for entry in servers.values_mut() {
        let Some(url) = entry.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(rewritten) = rewrite_loopback_url(url, gateway_host)? else {
            continue;
        };
        entry["url"] = serde_json::Value::String(rewritten);
        rewrote_any = true;
    }
    if !rewrote_any {
        return Ok(None);
    }
    Ok(Some(serde_json::to_vec_pretty(&doc)?))
}

/// [`patch_mcp_config_loopback_urls`]'s TOML-format branch (Codex's
/// `mcp_servers`). Mirrors [`patch_json_loopback_urls`]; `toml::Table` (not
/// `toml::Value`) is the parse target, matching `adapter::codex`'s own
/// convention for the same file.
fn patch_toml_loopback_urls(
    bytes: &[u8],
    servers_key: &str,
    gateway_host: &'static str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Ok(raw) = std::str::from_utf8(bytes) else {
        return Ok(None);
    };
    let Ok(mut doc) = raw.parse::<toml::Table>() else {
        return Ok(None);
    };
    let Some(servers) = doc.get_mut(servers_key).and_then(toml::Value::as_table_mut) else {
        return Ok(None);
    };
    let mut rewrote_any = false;
    for (_, entry) in servers.iter_mut() {
        let Some(url) = entry.get("url").and_then(toml::Value::as_str) else {
            continue;
        };
        let Some(rewritten) = rewrite_loopback_url(url, gateway_host)? else {
            continue;
        };
        entry["url"] = toml::Value::String(rewritten);
        rewrote_any = true;
    }
    if !rewrote_any {
        return Ok(None);
    }
    Ok(Some(toml::to_string_pretty(&doc)?.into_bytes()))
}

/// Rewrite `url_str`'s host to `gateway_host` when it's loopback, preserving
/// scheme/port/path/query. Returns `Ok(None)` unchanged when the host isn't
/// loopback (a remote ICM host needs no rewrite) or `url_str` doesn't parse.
///
/// Loopback judgment reuses `cli::doctor::is_local_addr` — the identical
/// "unreachable under this name from outside the host" check, shared rather
/// than duplicated (pre-pr-review P2, #1652) to avoid the two definitions
/// silently drifting apart.
fn rewrite_loopback_url(url_str: &str, gateway_host: &str) -> anyhow::Result<Option<String>> {
    let Ok(mut url) = url_str.parse::<url::Url>() else {
        return Ok(None);
    };
    let Some(host) = url.host_str() else {
        return Ok(None);
    };
    if !crate::cli::doctor::is_local_addr(host) {
        return Ok(None);
    }
    url.set_host(Some(gateway_host))
        .with_context(|| format!("setting host '{gateway_host}' on '{url_str}'"))?;
    Ok(Some(url.to_string()))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CLAUDE_JSON_FILE: &str = ".claude.json";

    fn claude_code_mount() -> ConfigDirMount {
        ConfigDirMount {
            env_var: "CLAUDE_CONFIG_DIR",
            mcp_config_file: CLAUDE_JSON_FILE,
            mcp_servers_key: "mcpServers",
            format: McpConfigFormat::Json,
            credential_file: Some(".credentials.json"),
        }
    }

    fn codex_mount() -> ConfigDirMount {
        ConfigDirMount {
            env_var: "CODEX_HOME",
            mcp_config_file: "config.toml",
            mcp_servers_key: "mcp_servers",
            format: McpConfigFormat::Toml,
            credential_file: Some("auth.json"),
        }
    }

    fn crush_mount() -> ConfigDirMount {
        ConfigDirMount {
            env_var: "CRUSH_GLOBAL_CONFIG",
            mcp_config_file: "crush.json",
            mcp_servers_key: "mcp",
            format: McpConfigFormat::Json,
            credential_file: None,
        }
    }

    #[test]
    fn write_patched_mcp_config_writes_under_llmenv_state_dir_not_the_shared_os_temp_dir() {
        let guard = write_patched_mcp_config(b"{}").unwrap();
        let path = guard.path();
        let state_dir = crate::paths::state_dir().unwrap();
        assert!(
            path.starts_with(&state_dir),
            "patched MCP-config file {} must live under llmenv's own state dir {}",
            path.display(),
            state_dir.display()
        );
        assert!(
            !path.starts_with(std::env::temp_dir()),
            "patched MCP-config file {} must not live in the shared OS temp dir",
            path.display()
        );
    }

    #[test]
    fn rewrite_loopback_url_replaces_loopback_host_preserving_port_and_path() {
        let rewritten = rewrite_loopback_url("http://127.0.0.1:9092/mcp", "host.docker.internal")
            .unwrap()
            .unwrap();
        assert_eq!(rewritten, "http://host.docker.internal:9092/mcp");
    }

    #[test]
    fn rewrite_loopback_url_leaves_remote_host_untouched() {
        let result =
            rewrite_loopback_url("http://icm.example.com:9092/mcp", "host.docker.internal")
                .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_loopback_url_returns_none_on_unparseable_url() {
        assert_eq!(
            rewrite_loopback_url("not a url", "host.docker.internal").unwrap(),
            None
        );
    }

    fn claude_json_with_icm_url(url: &str) -> String {
        serde_json::json!({
            "mcpServers": { llmenv_config::MEMORY_MCP_NAME: { "type": "http", "url": url } },
            "oauthAccount": { "unrelated": true },
        })
        .to_string()
    }

    #[test]
    fn patch_mcp_config_loopback_urls_rewrites_loopback_and_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(&path, claude_json_with_icm_url("http://127.0.0.1:9092/mcp")).unwrap();

        let patched = patch_mcp_config_loopback_urls(
            &path,
            "mcpServers",
            McpConfigFormat::Json,
            "host.docker.internal",
        )
        .unwrap()
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&patched).unwrap();
        assert_eq!(
            doc.pointer(&format!(
                "/mcpServers/{}/url",
                llmenv_config::MEMORY_MCP_NAME
            ))
            .and_then(|v| v.as_str()),
            Some("http://host.docker.internal:9092/mcp")
        );
        assert_eq!(
            doc.pointer("/oauthAccount/unrelated")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn patch_mcp_config_loopback_urls_returns_none_for_remote_icm_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(
            &path,
            claude_json_with_icm_url("http://icm.example.com:9092/mcp"),
        )
        .unwrap();

        assert_eq!(
            patch_mcp_config_loopback_urls(
                &path,
                "mcpServers",
                McpConfigFormat::Json,
                "host.docker.internal"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn patch_mcp_config_loopback_urls_returns_none_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        assert_eq!(
            patch_mcp_config_loopback_urls(
                &path,
                "mcpServers",
                McpConfigFormat::Json,
                "host.docker.internal"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn patch_mcp_config_loopback_urls_returns_none_for_empty_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(&path, r#"{"mcpServers": {}}"#).unwrap();
        assert_eq!(
            patch_mcp_config_loopback_urls(
                &path,
                "mcpServers",
                McpConfigFormat::Json,
                "host.docker.internal"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn patch_mcp_config_loopback_urls_rewrites_a_non_icm_server_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(
            &path,
            serde_json::json!({
                "mcpServers": {
                    "my-local-tool": {
                        "type": "http",
                        "url": "http://127.0.0.1:4400/mcp",
                        "headers": { "Authorization": "Bearer secret" },
                    },
                },
            })
            .to_string(),
        )
        .unwrap();

        let patched = patch_mcp_config_loopback_urls(
            &path,
            "mcpServers",
            McpConfigFormat::Json,
            "host.docker.internal",
        )
        .unwrap()
        .unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&patched).unwrap();
        assert_eq!(
            doc.pointer("/mcpServers/my-local-tool/url")
                .and_then(|v| v.as_str()),
            Some("http://host.docker.internal:4400/mcp")
        );
        // Non-URL fields on the same entry survive untouched.
        assert_eq!(
            doc.pointer("/mcpServers/my-local-tool/headers/Authorization")
                .and_then(|v| v.as_str()),
            Some("Bearer secret")
        );
    }

    #[test]
    fn patch_mcp_config_loopback_urls_returns_none_when_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(
            patch_mcp_config_loopback_urls(
                &path,
                "mcpServers",
                McpConfigFormat::Json,
                "host.docker.internal"
            )
            .unwrap(),
            None
        );
    }

    // TOML branch (Codex's config.toml / mcp_servers)
    #[test]
    fn patch_mcp_config_loopback_urls_rewrites_a_toml_server_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
model = "gpt-5"

[mcp_servers.icm]
url = "http://127.0.0.1:9092/mcp"
"#,
        )
        .unwrap();

        let patched = patch_mcp_config_loopback_urls(
            &path,
            "mcp_servers",
            McpConfigFormat::Toml,
            "host.docker.internal",
        )
        .unwrap()
        .unwrap();
        let doc: toml::Table = std::str::from_utf8(&patched).unwrap().parse().unwrap();
        assert_eq!(
            doc["mcp_servers"]["icm"]["url"].as_str(),
            Some("http://host.docker.internal:9092/mcp")
        );
        // Non-server keys survive untouched.
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
    }

    #[test]
    fn patch_mcp_config_loopback_urls_toml_returns_none_for_remote_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[mcp_servers.icm]
url = "http://icm.example.com:9092/mcp"
"#,
        )
        .unwrap();

        assert_eq!(
            patch_mcp_config_loopback_urls(
                &path,
                "mcp_servers",
                McpConfigFormat::Toml,
                "host.docker.internal"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn patch_mcp_config_loopback_urls_toml_returns_none_when_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not = [valid toml").unwrap();
        assert_eq!(
            patch_mcp_config_loopback_urls(
                &path,
                "mcp_servers",
                McpConfigFormat::Toml,
                "host.docker.internal"
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn prepare_returns_none_when_adapter_has_no_config_dir_mount() {
        let dir = tempfile::tempdir().unwrap();
        let result = prepare(None, ContainerRuntime::Docker, Some(dir.path())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn prepare_returns_none_when_no_config_dir_resolved() {
        let mount = claude_code_mount();
        let result = prepare(Some(&mount), ContainerRuntime::Docker, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn prepare_mounts_dir_unpatched_when_claude_json_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mount = claude_code_mount();
        let result = prepare(Some(&mount), ContainerRuntime::Docker, Some(dir.path()))
            .unwrap()
            .unwrap();
        assert_eq!(result.config_dir, dir.path());
        assert_eq!(result.mcp_config_path, dir.path().join(CLAUDE_JSON_FILE));
        assert!(result.patched_mcp_config.is_none());
    }

    #[test]
    fn prepare_produces_a_patched_file_for_a_loopback_icm_url() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CLAUDE_JSON_FILE),
            claude_json_with_icm_url("http://127.0.0.1:9092/mcp"),
        )
        .unwrap();

        let mount = claude_code_mount();
        let result = prepare(Some(&mount), ContainerRuntime::Docker, Some(dir.path()))
            .unwrap()
            .unwrap();
        let guard = result.patched_mcp_config.unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(guard.path()).unwrap()).unwrap();
        assert_eq!(
            doc.pointer(&format!(
                "/mcpServers/{}/url",
                llmenv_config::MEMORY_MCP_NAME
            ))
            .and_then(|v| v.as_str()),
            Some("http://host.docker.internal:9092/mcp")
        );
    }

    #[test]
    fn prepare_sets_credential_file_when_the_adapter_has_one() {
        let dir = tempfile::tempdir().unwrap();
        let mount = claude_code_mount();
        let result = prepare(Some(&mount), ContainerRuntime::Docker, Some(dir.path()))
            .unwrap()
            .unwrap();
        assert_eq!(
            result.credential_file,
            Some(dir.path().join(".credentials.json"))
        );
    }

    #[test]
    fn prepare_leaves_credential_file_none_when_the_adapter_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let mount = crush_mount();
        let result = prepare(Some(&mount), ContainerRuntime::Docker, Some(dir.path()))
            .unwrap()
            .unwrap();
        assert_eq!(result.credential_file, None);
    }

    #[test]
    fn prepare_mounts_codexs_toml_config_and_masks_auth_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[mcp_servers.icm]\nurl = \"http://127.0.0.1:9092/mcp\"\n",
        )
        .unwrap();

        let mount = codex_mount();
        let result = prepare(Some(&mount), ContainerRuntime::Docker, Some(dir.path()))
            .unwrap()
            .unwrap();
        assert_eq!(result.mcp_config_path, dir.path().join("config.toml"));
        assert_eq!(result.credential_file, Some(dir.path().join("auth.json")));
        let guard = result.patched_mcp_config.unwrap();
        let doc: toml::Table = std::fs::read_to_string(guard.path())
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            doc["mcp_servers"]["icm"]["url"].as_str(),
            Some("http://host.docker.internal:9092/mcp")
        );
    }

    proptest! {
        #[test]
        fn prop_rewrite_loopback_url_never_panics_on_arbitrary_input(url in ".{0,80}") {
            let _ = rewrite_loopback_url(&url, "host.docker.internal");
        }

        #[test]
        fn prop_patch_mcp_config_loopback_urls_never_panics_on_arbitrary_bytes(bytes in ".{0,200}") {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(CLAUDE_JSON_FILE);
            std::fs::write(&path, &bytes).unwrap();
            let _ = patch_mcp_config_loopback_urls(&path, "mcpServers", McpConfigFormat::Json, "host.docker.internal");
            let _ = patch_mcp_config_loopback_urls(&path, "mcp_servers", McpConfigFormat::Toml, "host.docker.internal");
        }

        // pre-pr-review pbt-gap (#1652): roundtrip properties, not just
        // no-crash — the earlier proptests above never asserted anything
        // about the *result* of a rewrite.
        #[test]
        fn prop_rewrite_loopback_url_replaces_any_loopback_ip_preserving_rest(
            loopback_octet in 0u8..=255,
            port in 1u16..=65535,
            path_segment in "[a-z]{1,10}",
            gateway in prop_oneof![Just("host.docker.internal"), Just("host.containers.internal")],
        ) {
            let url = format!("http://127.0.0.{loopback_octet}:{port}/{path_segment}");
            let rewritten = rewrite_loopback_url(&url, gateway).unwrap().unwrap();
            prop_assert_eq!(rewritten, format!("http://{gateway}:{port}/{path_segment}"));
        }

        #[test]
        fn prop_rewrite_loopback_url_is_none_for_non_loopback_hosts(
            label in "[a-z][a-z0-9-]{0,15}",
            tld in "[a-z]{2,5}",
            port in 1u16..=65535,
        ) {
            let url = format!("http://{label}.{tld}:{port}/mcp");
            prop_assert_eq!(
                rewrite_loopback_url(&url, "host.docker.internal").unwrap(),
                None
            );
        }

        #[test]
        fn prop_patch_mcp_config_loopback_urls_rewrites_only_the_loopback_entries(
            // `is_loopback[i]` decides whether server `i` gets a loopback or
            // remote URL; index-based names avoid collisions between the two
            // groups without a separate dedup step.
            is_loopback in proptest::collection::vec(proptest::bool::ANY, 1..6),
            port in 1u16..=65535,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(CLAUDE_JSON_FILE);
            let mut servers = serde_json::Map::new();
            for (i, loopback) in is_loopback.iter().enumerate() {
                let host = if *loopback { "127.0.0.1".to_string() } else { format!("remote{i}.example.com") };
                servers.insert(
                    format!("server{i}"),
                    serde_json::json!({ "type": "http", "url": format!("http://{host}:{port}/mcp") }),
                );
            }
            std::fs::write(
                &path,
                serde_json::json!({ "mcpServers": servers, "unrelated": "kept" }).to_string(),
            )
            .unwrap();

            let any_loopback = is_loopback.iter().any(|b| *b);
            let result = patch_mcp_config_loopback_urls(&path, "mcpServers", McpConfigFormat::Json, "host.docker.internal").unwrap();
            prop_assert_eq!(result.is_some(), any_loopback);
            if let Some(patched) = result {
                let doc: serde_json::Value = serde_json::from_slice(&patched).unwrap();
                prop_assert_eq!(doc.pointer("/unrelated").and_then(|v| v.as_str()), Some("kept"));
                for (i, loopback) in is_loopback.iter().enumerate() {
                    let url = doc
                        .pointer(&format!("/mcpServers/server{i}/url"))
                        .and_then(|v| v.as_str())
                        .unwrap();
                    if *loopback {
                        prop_assert_eq!(url, format!("http://host.docker.internal:{port}/mcp"));
                    } else {
                        prop_assert_eq!(url, format!("http://remote{i}.example.com:{port}/mcp"));
                    }
                }
            }
        }

        // #1698 pre-pr-review pbt-gap: the TOML branch got only a no-crash
        // proptest — mirrors the JSON roundtrip property above, for Codex's
        // config.toml/mcp_servers shape.
        #[test]
        fn prop_patch_toml_config_loopback_urls_rewrites_only_the_loopback_entries(
            is_loopback in proptest::collection::vec(proptest::bool::ANY, 1..6),
            port in 1u16..=65535,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            let mut doc = String::from("model = \"gpt-5\"\n");
            for (i, loopback) in is_loopback.iter().enumerate() {
                let host = if *loopback { "127.0.0.1".to_string() } else { format!("remote{i}.example.com") };
                doc.push_str(&format!(
                    "\n[mcp_servers.server{i}]\nurl = \"http://{host}:{port}/mcp\"\n"
                ));
            }
            std::fs::write(&path, &doc).unwrap();

            let any_loopback = is_loopback.iter().any(|b| *b);
            let result = patch_mcp_config_loopback_urls(&path, "mcp_servers", McpConfigFormat::Toml, "host.docker.internal").unwrap();
            prop_assert_eq!(result.is_some(), any_loopback);
            if let Some(patched) = result {
                let parsed: toml::Table = std::str::from_utf8(&patched).unwrap().parse().unwrap();
                prop_assert_eq!(parsed["model"].as_str(), Some("gpt-5"));
                for (i, loopback) in is_loopback.iter().enumerate() {
                    let server_name = format!("server{i}");
                    let url = parsed["mcp_servers"][server_name.as_str()]["url"]
                        .as_str()
                        .unwrap();
                    if *loopback {
                        prop_assert_eq!(url, format!("http://host.docker.internal:{port}/mcp"));
                    } else {
                        prop_assert_eq!(url, format!("http://remote{i}.example.com:{port}/mcp"));
                    }
                }
            }
        }
    }
}
