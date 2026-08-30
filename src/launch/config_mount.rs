//! Mounts the materialized Claude Code config directory into a sandboxed
//! launch's container (#1652), and rewrites the ICM MCP server's URL so it
//! resolves from inside the container's own network namespace.
//!
//! Before this, `sandbox::container_command` only mounted the project tree,
//! `SSH_AUTH_SOCK`, and the resolved env vars — `CLAUDE_CONFIG_DIR` itself
//! was never mounted, so a containerized Claude Code saw none of llmenv's
//! materialized `mcpServers`, skills, plugins, or settings. This mounts that
//! directory read-only at its own path (so the `CLAUDE_CONFIG_DIR` env var
//! stays valid unmodified), and — when the ICM MCP entry in `.claude.json`
//! points at a loopback address, which it does whenever the ICM host is this
//! same machine — overlays a patched copy of just that one file with the URL
//! rewritten to the container gateway host (`sandbox::gateway_host`),
//! mirroring `icebreaker.rs`'s credential-proxy rewrite for the same reason:
//! `127.0.0.1` inside the container is the container itself.
//!
//! Scoped to Claude Code only, matching `icebreaker.rs`'s precedent — no
//! other adapter has an equivalent config-dir mount into the sandbox yet
//! (tracked separately, see the issue this module's own tracking references).

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::sandbox::ContainerRuntime;

const CLAUDE_JSON_FILE: &str = ".claude.json";

/// A patched copy of `.claude.json`, deleted on drop once the container that
/// mounted it has exited — mirrors `sandbox::EnvFileGuard`'s pattern.
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

/// What to bind-mount into a sandboxed launch's container so Claude Code
/// sees its materialized config: `config_dir` mounted read-only at the same
/// in-container path, plus (when ICM's URL needed rewriting) a
/// [`PatchedFileGuard`] overlay-mounted at `config_dir/.claude.json`.
pub(crate) struct ConfigMount {
    pub(crate) config_dir: PathBuf,
    pub(crate) patched_claude_json: Option<PatchedFileGuard>,
}

/// Disambiguates concurrent [`write_patched_claude_json`] calls within the
/// same process, mirroring `sandbox::ENV_FILE_COUNTER`.
static PATCH_FILE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Resolve what to mount for this launch. `Ok(None)` when there's nothing to
/// mount: a non-Claude-Code adapter, or no config directory resolved for
/// this launch (the caller has nothing to pass in that case).
///
/// # Errors
/// Propagates a read/parse/write failure preparing the patched file. A
/// missing or malformed `.claude.json` is not an error here — the container
/// still gets the directory mount unpatched; `patch_claude_json_icm_url`
/// treats "nothing to patch" as `Ok(None)`, not a failure.
pub(crate) fn prepare(
    adapter_name: &str,
    runtime: ContainerRuntime,
    config_dir: Option<&Path>,
) -> anyhow::Result<Option<ConfigMount>> {
    if adapter_name != "claude-code" {
        return Ok(None);
    }
    let Some(config_dir) = config_dir else {
        return Ok(None);
    };

    let claude_json_path = config_dir.join(CLAUDE_JSON_FILE);
    let patched_claude_json = match patch_claude_json_icm_url(
        &claude_json_path,
        super::sandbox::gateway_host(runtime),
    )? {
        Some(patched_bytes) => Some(write_patched_claude_json(&patched_bytes)?),
        None => None,
    };

    Ok(Some(ConfigMount {
        config_dir: config_dir.to_path_buf(),
        patched_claude_json,
    }))
}

/// Write `bytes` to a fresh owner-only temp file, returning a guard that
/// deletes it on drop. Mirrors `sandbox::write_env_file`.
fn write_patched_claude_json(bytes: &[u8]) -> anyhow::Result<PatchedFileGuard> {
    let n = PATCH_FILE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "llmenv-sandbox-claude-json-{}-{n}",
        std::process::id()
    ));
    crate::paths::write_owner_only(&path, bytes).context("writing the patched claude.json")?;
    Ok(PatchedFileGuard(path))
}

/// Read `claude_json_path` (if present) and, when its ICM (`icm`)
/// `mcpServers` entry has a loopback-host URL, return the same document with
/// that URL rewritten to `gateway_host`, serialized back to bytes.
///
/// Returns `Ok(None)` when there's nothing to patch: the file doesn't exist,
/// doesn't parse as JSON, has no ICM entry, or that entry's URL isn't a
/// loopback address in the first place (e.g. a remote ICM host — already
/// reachable from inside the container exactly as it is on the host).
fn patch_claude_json_icm_url(
    claude_json_path: &Path,
    gateway_host: &'static str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let bytes = match std::fs::read(claude_json_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", claude_json_path.display()));
        }
    };
    let Ok(mut doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        // Malformed .claude.json is a hard error elsewhere (materialization
        // refuses to write over it); here it just means there's nothing this
        // function can safely rewrite, so the container gets the file as-is.
        return Ok(None);
    };
    let Some(url) = doc
        .pointer(&format!(
            "/mcpServers/{}/url",
            llmenv_config::MEMORY_MCP_NAME
        ))
        .and_then(|v| v.as_str())
    else {
        return Ok(None);
    };
    let Some(rewritten) = rewrite_loopback_url(url, gateway_host)? else {
        return Ok(None);
    };
    let pointer = format!("/mcpServers/{}/url", llmenv_config::MEMORY_MCP_NAME);
    if let Some(slot) = doc.pointer_mut(&pointer) {
        *slot = serde_json::Value::String(rewritten);
    }
    Ok(Some(serde_json::to_vec_pretty(&doc)?))
}

/// `true` for a host string that means "this machine, from this machine's
/// own point of view" — unreachable from inside a container's own network
/// namespace under that same name. Mirrors `cli::doctor::is_local_addr`'s
/// semantics; kept as a local copy rather than a cross-module `pub(crate)`
/// reach into `doctor`, which doesn't otherwise expose helpers outside its
/// own module.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "0.0.0.0" | "::" | "::0")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Rewrite `url_str`'s host to `gateway_host` when it's loopback, preserving
/// scheme/port/path/query. Returns `Ok(None)` unchanged when the host isn't
/// loopback (a remote ICM host needs no rewrite) or `url_str` doesn't parse.
fn rewrite_loopback_url(url_str: &str, gateway_host: &str) -> anyhow::Result<Option<String>> {
    let Ok(mut url) = url_str.parse::<url::Url>() else {
        return Ok(None);
    };
    let Some(host) = url.host_str() else {
        return Ok(None);
    };
    if !is_loopback_host(host) {
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

    #[test]
    fn is_loopback_host_accepts_localhost_and_loopback_ips() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
    }

    #[test]
    fn is_loopback_host_rejects_remote_addresses() {
        assert!(!is_loopback_host("10.0.0.4"));
        assert!(!is_loopback_host("icm.example.com"));
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
    fn patch_claude_json_icm_url_rewrites_loopback_and_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(&path, claude_json_with_icm_url("http://127.0.0.1:9092/mcp")).unwrap();

        let patched = patch_claude_json_icm_url(&path, "host.docker.internal")
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
    fn patch_claude_json_icm_url_returns_none_for_remote_icm_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(
            &path,
            claude_json_with_icm_url("http://icm.example.com:9092/mcp"),
        )
        .unwrap();

        assert_eq!(
            patch_claude_json_icm_url(&path, "host.docker.internal").unwrap(),
            None
        );
    }

    #[test]
    fn patch_claude_json_icm_url_returns_none_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        assert_eq!(
            patch_claude_json_icm_url(&path, "host.docker.internal").unwrap(),
            None
        );
    }

    #[test]
    fn patch_claude_json_icm_url_returns_none_when_no_icm_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(&path, r#"{"mcpServers": {}}"#).unwrap();
        assert_eq!(
            patch_claude_json_icm_url(&path, "host.docker.internal").unwrap(),
            None
        );
    }

    #[test]
    fn patch_claude_json_icm_url_returns_none_when_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CLAUDE_JSON_FILE);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(
            patch_claude_json_icm_url(&path, "host.docker.internal").unwrap(),
            None
        );
    }

    #[test]
    fn prepare_returns_none_for_non_claude_code_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let result = prepare("crush", ContainerRuntime::Docker, Some(dir.path())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn prepare_returns_none_when_no_config_dir_resolved() {
        let result = prepare("claude-code", ContainerRuntime::Docker, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn prepare_mounts_dir_unpatched_when_claude_json_absent() {
        let dir = tempfile::tempdir().unwrap();
        let result = prepare("claude-code", ContainerRuntime::Docker, Some(dir.path()))
            .unwrap()
            .unwrap();
        assert_eq!(result.config_dir, dir.path());
        assert!(result.patched_claude_json.is_none());
    }

    #[test]
    fn prepare_produces_a_patched_file_for_a_loopback_icm_url() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CLAUDE_JSON_FILE),
            claude_json_with_icm_url("http://127.0.0.1:9092/mcp"),
        )
        .unwrap();

        let result = prepare("claude-code", ContainerRuntime::Docker, Some(dir.path()))
            .unwrap()
            .unwrap();
        let guard = result.patched_claude_json.unwrap();
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

    proptest! {
        #[test]
        fn prop_rewrite_loopback_url_never_panics_on_arbitrary_input(url in ".{0,80}") {
            let _ = rewrite_loopback_url(&url, "host.docker.internal");
        }

        #[test]
        fn prop_patch_claude_json_icm_url_never_panics_on_arbitrary_bytes(bytes in ".{0,200}") {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(CLAUDE_JSON_FILE);
            std::fs::write(&path, &bytes).unwrap();
            let _ = patch_claude_json_icm_url(&path, "host.docker.internal");
        }
    }
}
