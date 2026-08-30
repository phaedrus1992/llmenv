//! icebreaker sealed-token proxy integration (#1651): protects a sandboxed
//! launch's outbound API credential so the container never holds the raw
//! key. Spawns `icebreaker serve` (github.com/windowlickers/icebreaker) as a
//! host subprocess around the launch session, mirroring `proxy.rs`'s
//! bind/teardown lifecycle — but for an external binary, not an in-process
//! Rust future, since the design deliberately avoids depending on
//! icebreaker's own crates (see the design doc's "Credentials: icebreaker"
//! section).
//!
//! **Caveat on the routing mechanism below.** Read from icebreaker's own
//! source at the commit the design doc pins (`ec6bd50`,
//! `crates/icebreaker-proxy/src/serve/proxy_service.rs`): its `ProxyService`
//! takes the destination *authority* from an absolute-form request URI or,
//! failing that, the `Host` header, and the destination *scheme* only from
//! the sealed token's `upstream_scheme`. `ANTHROPIC_BASE_URL` is rewritten to
//! icebreaker's local address and the real upstream authority is carried in
//! a `Host` custom header (the same `ANTHROPIC_CUSTOM_HEADERS` mechanism
//! `launch_proxy`'s peer-auth header already relies on) so icebreaker's
//! Host-header fallback resolves to the real upstream. This has been read
//! out of icebreaker's source, not validated against a running instance and
//! a live engine — confirm it end-to-end before relying on it in
//! production.

use std::collections::BTreeMap;

use anyhow::Context;

use super::sandbox::{ContainerRuntime, gateway_host};

/// The credential env var this integration protects: Anthropic's documented
/// API-key env var. Scoped to Claude Code only, matching `launch_proxy`'s own
/// scoping (`adapter.name() == "claude-code"`).
const CREDENTIAL_VAR: &str = "ANTHROPIC_API_KEY";
/// Anthropic's real auth header — not `Authorization` (icebreaker's own
/// `--header` default).
const CREDENTIAL_HEADER: &str = "x-api-key";
/// Sealed-token lifetime: bounds exposure if a container image is somehow
/// exfiltrated. A launch session is expected to complete well inside this
/// window; a fresh token is sealed on every launch regardless.
const TOKEN_LIFETIME_SECS: u64 = 12 * 60 * 60;
/// How long to wait for `icebreaker serve` to start accepting connections
/// before giving up.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct Keypair {
    secret: String,
    public: String,
}

/// A running `icebreaker serve` subprocess for one launch session, killed
/// (best-effort) on drop — mirrors `SocketCleanup`'s pattern for the notice
/// socket.
struct IcebreakerServer {
    child: tokio::process::Child,
}

impl Drop for IcebreakerServer {
    fn drop(&mut self) {
        // `error!`, not `warn!`: llmenv's default `EnvFilter` is ERROR-only
        // (see `forward_signal`'s doc comment in `mod.rs`), and a failure
        // here means the process holding the session's sealed-token keypair
        // may keep running past the end of the launch, orphaned.
        if let Err(e) = self.child.start_kill() {
            tracing::error!("launch: could not stop icebreaker server, it may keep running: {e}");
        }
    }
}

/// One sealed-token session for a sandboxed launch: the running icebreaker
/// server (kept alive, killed on drop) plus the env vars the container
/// should get in place of the plain resolved vars.
pub(crate) struct IcebreakerSession {
    _server: IcebreakerServer,
    pub(crate) container_vars: BTreeMap<String, String>,
}

/// Set up icebreaker for this sandboxed launch, if there is a credential
/// worth protecting: Claude Code plus a raw [`CREDENTIAL_VAR`] in
/// `resolved_vars`. Returns `Ok(None)` when there's nothing to seal — a
/// non-Claude-Code engine, or an OAuth-only Claude Code session with no
/// plain API key — in which case the container gets `resolved_vars`
/// unchanged. An OAuth-cached credential is a local file, not an env var,
/// and isn't covered by this integration; that's a known gap (see the
/// caller's tracking issue).
///
/// # Errors
/// Once a credential IS present, any failure is fatal — the design's
/// error-handling section requires failing before the container starts
/// rather than starting it with no or an unsealed credential.
pub(crate) async fn prepare(
    adapter_name: &str,
    runtime: ContainerRuntime,
    resolved_vars: &BTreeMap<String, String>,
) -> anyhow::Result<Option<IcebreakerSession>> {
    if adapter_name != "claude-code" {
        return Ok(None);
    }
    let Some(secret) = resolved_vars.get(CREDENTIAL_VAR) else {
        return Ok(None);
    };

    let Some(icebreaker_bin) = crate::paths::resolve_on_path("icebreaker") else {
        anyhow::bail!(
            "sandbox mode needs to protect {CREDENTIAL_VAR}, but 'icebreaker' was not found on \
             PATH — install it, or unset {CREDENTIAL_VAR} for an unauthenticated sandboxed session"
        );
    };

    let authority = upstream_authority(resolved_vars)?;
    let key_id = format!("llmenv-{}", std::process::id());
    let keypair = run_keygen(icebreaker_bin.clone(), key_id.clone()).await?;
    let port = reserve_ephemeral_port()?;
    let child = spawn_server(&icebreaker_bin, port, &keypair.secret, &key_id).await?;
    let mut server = IcebreakerServer { child };
    wait_ready(&mut server.child, port, READY_TIMEOUT).await?;

    let sealed = run_seal(
        icebreaker_bin,
        SealParams {
            secret: secret.clone(),
            allowed_hosts: authority.clone(),
            public_key: keypair.public,
            key_id,
            expires_in_secs: TOKEN_LIFETIME_SECS,
        },
    )
    .await?;
    let container_vars = build_container_vars(
        resolved_vars,
        gateway_host(runtime),
        port,
        &authority,
        &sealed,
    );
    Ok(Some(IcebreakerSession {
        _server: server,
        container_vars,
    }))
}

/// Extract the authority (`host[:port]`) icebreaker should allow and receive
/// as the `Host` header, from `resolved_vars`'s `ANTHROPIC_BASE_URL` — or
/// Anthropic's default when unset, mirroring `run`'s own launch_proxy
/// resolution of the same variable.
fn upstream_authority(resolved_vars: &BTreeMap<String, String>) -> anyhow::Result<String> {
    let base_url = resolved_vars
        .get("ANTHROPIC_BASE_URL")
        .cloned()
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    let url: url::Url = base_url
        .parse()
        .with_context(|| format!("parsing ANTHROPIC_BASE_URL '{base_url}'"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("ANTHROPIC_BASE_URL '{base_url}' has no host"))?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// Build the env vars a sandboxed container gets in place of `resolved_vars`
/// once a credential has been sealed: the raw credential is dropped,
/// `ANTHROPIC_BASE_URL` points at the local icebreaker proxy, and a custom
/// `Host`/`X-Tokenizer-Token` pair is injected the same way `launch_proxy`
/// injects its own peer-auth header (`super::append_custom_header`).
fn build_container_vars(
    resolved_vars: &BTreeMap<String, String>,
    gateway_host: &str,
    icebreaker_port: u16,
    upstream_authority: &str,
    sealed_token: &str,
) -> BTreeMap<String, String> {
    let mut vars = resolved_vars.clone();
    vars.remove(CREDENTIAL_VAR);
    vars.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        format!("http://{gateway_host}:{icebreaker_port}"),
    );
    super::append_custom_header(&mut vars, "Host", upstream_authority);
    super::append_custom_header(&mut vars, "X-Tokenizer-Token", sealed_token);
    vars
}

/// Reserve a free loopback port by binding then immediately releasing it, so
/// the port number is known before `icebreaker serve` (an external process
/// whose own bound address llmenv cannot introspect) starts. Carries a small
/// TOCTOU window between release and icebreaker's own bind — acceptable here
/// since both ends are loopback-only and short-lived.
fn reserve_ephemeral_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("reserving an ephemeral port for the icebreaker proxy")?;
    Ok(listener
        .local_addr()
        .context("reading the reserved icebreaker port")?
        .port())
}

/// Returns the first non-blank line strictly after a line equal to `header`
/// (trimmed), itself trimmed. `icebreaker keygen`/`seal` print a label line,
/// a blank line, then the value indented by two spaces — this is the shared
/// shape both outputs use.
fn value_after_header(text: &str, header: &str) -> Option<String> {
    let mut lines = text.lines();
    lines.find(|line| line.trim() == header)?;
    lines
        .find(|line| !line.trim().is_empty())
        .map(|l| l.trim().to_string())
}

fn parse_keygen_output(stdout: &str) -> anyhow::Result<Keypair> {
    let secret = value_after_header(stdout, "Secret key (keep private):").ok_or_else(|| {
        anyhow::anyhow!("could not find the secret key in `icebreaker keygen` output")
    })?;
    let public = value_after_header(stdout, "Public key (safe to share):").ok_or_else(|| {
        anyhow::anyhow!("could not find the public key in `icebreaker keygen` output")
    })?;
    Ok(Keypair { secret, public })
}

/// Both blocking subprocess calls below run on a `spawn_blocking` thread with
/// this bound — `run()`'s tokio runtime is single-threaded (`mod.rs`'s
/// `Builder::new_current_thread()`), so an unbounded `icebreaker
/// keygen`/`seal` hang (a wedged lock file, a broken build) would otherwise
/// block the entire launch indefinitely with no diagnostic, unlike
/// `wait_ready`'s already-bounded poll a few lines below.
const SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn run_keygen(icebreaker_bin: std::path::PathBuf, key_id: String) -> anyhow::Result<Keypair> {
    let joined = tokio::time::timeout(
        SUBPROCESS_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let output = std::process::Command::new(&icebreaker_bin)
                .args(["keygen", "--format", "base64", "--key-id", &key_id])
                .output()
                .context("running `icebreaker keygen`")?;
            if !output.status.success() {
                anyhow::bail!(
                    "`icebreaker keygen` failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            parse_keygen_output(&String::from_utf8_lossy(&output.stdout))
        }),
    )
    .await
    .context("`icebreaker keygen` timed out")?;
    joined.context("`icebreaker keygen` task panicked")?
}

struct SealParams {
    secret: String,
    allowed_hosts: String,
    public_key: String,
    key_id: String,
    expires_in_secs: u64,
}

async fn run_seal(
    icebreaker_bin: std::path::PathBuf,
    params: SealParams,
) -> anyhow::Result<String> {
    let joined = tokio::time::timeout(
        SUBPROCESS_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let expires_in = params.expires_in_secs.to_string();
            let output = std::process::Command::new(&icebreaker_bin)
                .args([
                    "seal",
                    "--secret",
                    &params.secret,
                    "--allowed-hosts",
                    &params.allowed_hosts,
                    "--header",
                    CREDENTIAL_HEADER,
                    "--public-key",
                    &params.public_key,
                    "--key-id",
                    &params.key_id,
                    "--expires-in",
                    &expires_in,
                ])
                .output()
                .context("running `icebreaker seal`")?;
            if !output.status.success() {
                anyhow::bail!(
                    "`icebreaker seal` failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            value_after_header(&String::from_utf8_lossy(&output.stdout), "Sealed token:")
                .ok_or_else(|| {
                    anyhow::anyhow!("could not find the sealed token in `icebreaker seal` output")
                })
        }),
    )
    .await
    .context("`icebreaker seal` timed out")?;
    joined.context("`icebreaker seal` task panicked")?
}

async fn spawn_server(
    icebreaker_bin: &std::path::Path,
    port: u16,
    secret_key: &str,
    key_id: &str,
) -> anyhow::Result<tokio::process::Child> {
    let port_str = port.to_string();
    let mut cmd = tokio::process::Command::new(icebreaker_bin);
    cmd.args([
        "serve",
        "--bind",
        "127.0.0.1",
        "--port",
        &port_str,
        "--key-id",
        key_id,
        "--health-enabled",
        "false",
    ]);
    // The keypair secret goes through the env, not argv: `icebreaker serve`
    // documents `ICEBREAKER_SECRET_KEY` as an equivalent to `--secret-key`,
    // and argv is world-readable via `/proc/<pid>/cmdline` on Linux (any
    // local uid, not just the same one) for as long as this process runs —
    // which, unlike the short-lived `seal` subprocess below, is the whole
    // sandboxed session.
    cmd.env("ICEBREAKER_SECRET_KEY", secret_key);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("spawning `icebreaker serve`")?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(forward_stderr_to_tracing(stderr));
    }
    Ok(child)
}

/// Forwards each line of icebreaker's stderr to `tracing::debug!` rather than
/// discarding it or inheriting it — inheriting would interleave icebreaker's
/// own logs with the supervised engine's stdio. Stops (and says so) on the
/// first read error rather than silently exiting the loop.
async fn forward_stderr_to_tracing(stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => tracing::debug!("icebreaker: {line}"),
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("icebreaker: stderr forwarding stopped: {e}");
                return;
            }
        }
    }
}

/// Poll-connects to `port` until something accepts or `timeout` elapses.
///
/// Also checks `child` hasn't already exited on each poll — otherwise a
/// connect success only proves *something* is listening on the released
/// port, not that it's the icebreaker process this call is waiting on (the
/// reservation in [`reserve_ephemeral_port`] has a TOCTOU window another
/// local process could win).
async fn wait_ready(
    child: &mut tokio::process::Child,
    port: u16,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("checking whether icebreaker is still running")?
        {
            anyhow::bail!("icebreaker exited before it started listening: {status}");
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("icebreaker did not start listening on port {port} within {timeout:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parse_keygen_output_extracts_secret_and_public_keys() {
        let stdout = "Generated keypair for key ID: primary\n\n\
             Secret key (keep private):\n  SECRETB64\n\n\
             Public key (safe to share):\n  PUBLICB64\n\n\
             Environment variables:\n  export ICEBREAKER_SECRET_KEY=\"SECRETB64\"\n";
        let keypair = parse_keygen_output(stdout).unwrap();
        assert_eq!(keypair.secret, "SECRETB64");
        assert_eq!(keypair.public, "PUBLICB64");
    }

    #[test]
    fn parse_keygen_output_fails_on_unexpected_shape() {
        assert!(parse_keygen_output("nothing useful here").is_err());
    }

    #[test]
    fn value_after_header_extracts_the_sealed_token_line() {
        let stdout =
            "Sealed token:\n\nTokenizer eyJhbGc...\n\nUse this in the X-Tokenizer-Token header.\n";
        assert_eq!(
            value_after_header(stdout, "Sealed token:").as_deref(),
            Some("Tokenizer eyJhbGc...")
        );
    }

    #[test]
    fn value_after_header_returns_none_when_header_absent() {
        assert_eq!(
            value_after_header("no such header here", "Sealed token:"),
            None
        );
    }

    #[test]
    fn upstream_authority_defaults_to_anthropic_when_unset() {
        let vars = BTreeMap::new();
        assert_eq!(upstream_authority(&vars).unwrap(), "api.anthropic.com");
    }

    #[test]
    fn upstream_authority_reads_configured_base_url_with_port() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            "https://gw.example.com:8443/anthropic".to_string(),
        );
        assert_eq!(upstream_authority(&vars).unwrap(), "gw.example.com:8443");
    }

    #[test]
    fn build_container_vars_drops_raw_credential_and_injects_headers() {
        let mut vars = BTreeMap::new();
        vars.insert(CREDENTIAL_VAR.to_string(), "sk-raw-secret".to_string());
        vars.insert("OTHER_VAR".to_string(), "kept".to_string());

        let container_vars = build_container_vars(
            &vars,
            "host.docker.internal",
            4321,
            "api.anthropic.com",
            "Tokenizer abc",
        );

        assert!(!container_vars.contains_key(CREDENTIAL_VAR));
        assert_eq!(
            container_vars.get("OTHER_VAR").map(String::as_str),
            Some("kept")
        );
        assert_eq!(
            container_vars.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://host.docker.internal:4321")
        );
        let headers = container_vars
            .get("ANTHROPIC_CUSTOM_HEADERS")
            .cloned()
            .unwrap_or_default();
        assert!(headers.contains("Host: api.anthropic.com"));
        assert!(headers.contains("X-Tokenizer-Token: Tokenizer abc"));
    }

    #[tokio::test]
    async fn prepare_returns_none_for_a_non_claude_code_adapter() {
        let mut vars = BTreeMap::new();
        vars.insert(CREDENTIAL_VAR.to_string(), "sk-raw-secret".to_string());
        let result = prepare("crush", ContainerRuntime::Docker, &vars)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn prepare_returns_none_when_no_credential_is_present() {
        let vars = BTreeMap::new();
        let result = prepare("claude-code", ContainerRuntime::Docker, &vars)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn gateway_host_differs_by_runtime() {
        assert_eq!(
            gateway_host(ContainerRuntime::Docker),
            "host.docker.internal"
        );
        assert_eq!(
            gateway_host(ContainerRuntime::Podman),
            "host.containers.internal"
        );
    }

    #[test]
    fn reserve_ephemeral_port_returns_a_usable_port() {
        let port = reserve_ephemeral_port().unwrap();
        assert!(port > 0);
    }

    proptest! {
        /// Excludes the default ports (80/443) from the range: `url::Url`
        /// omits a port matching the scheme default from `Url::port()`, which
        /// would make the extracted authority disagree with the input port
        /// for no reason related to this function's own correctness.
        #[test]
        fn prop_upstream_authority_extracts_host_and_optional_port(
            host in "[a-z][a-z0-9-]{0,15}(\\.[a-z][a-z0-9-]{0,15}){0,3}",
            port in proptest::option::of(1024u16..=65535),
            scheme in prop_oneof![Just("http"), Just("https")],
        ) {
            let mut vars = BTreeMap::new();
            let url = match port {
                Some(p) => format!("{scheme}://{host}:{p}/anthropic"),
                None => format!("{scheme}://{host}/anthropic"),
            };
            vars.insert("ANTHROPIC_BASE_URL".to_string(), url);
            let authority = upstream_authority(&vars).unwrap();
            let expected = match port {
                Some(p) => format!("{host}:{p}"),
                None => host.clone(),
            };
            prop_assert_eq!(authority, expected);
        }

        #[test]
        fn prop_upstream_authority_never_panics_on_arbitrary_input(base_url in ".{0,60}") {
            let mut vars = BTreeMap::new();
            vars.insert("ANTHROPIC_BASE_URL".to_string(), base_url);
            let _ = upstream_authority(&vars);
        }

        #[test]
        fn prop_value_after_header_never_panics_on_arbitrary_input(
            text in ".{0,200}",
            header in ".{0,50}",
        ) {
            let _ = value_after_header(&text, &header);
        }

        #[test]
        fn prop_parse_keygen_output_roundtrips_arbitrary_keys(
            secret in "[A-Za-z0-9+/=]{1,60}",
            public in "[A-Za-z0-9+/=]{1,60}",
        ) {
            let stdout = format!(
                "Generated keypair for key ID: primary\n\n\
                 Secret key (keep private):\n  {secret}\n\n\
                 Public key (safe to share):\n  {public}\n\n"
            );
            let keypair = parse_keygen_output(&stdout).unwrap();
            prop_assert_eq!(keypair.secret, secret);
            prop_assert_eq!(keypair.public, public);
        }

        #[test]
        fn prop_build_container_vars_drops_credential_preserves_others_and_injects_headers(
            mut vars in proptest::collection::btree_map("[A-Z_]{1,10}", "[a-zA-Z0-9]{0,20}", 0..5),
            include_credential in proptest::bool::ANY,
            gateway in prop_oneof![Just("host.docker.internal"), Just("host.containers.internal")],
            port in any::<u16>(),
            authority in "[a-z.]{1,20}",
            token in "[A-Za-z0-9 ]{1,20}",
        ) {
            if include_credential {
                vars.insert(CREDENTIAL_VAR.to_string(), "raw-secret".to_string());
            }
            let other_vars_before: BTreeMap<_, _> = vars
                .iter()
                .filter(|(k, _)| k.as_str() != CREDENTIAL_VAR)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let result = build_container_vars(&vars, gateway, port, &authority, &token);

            prop_assert!(!result.contains_key(CREDENTIAL_VAR));
            for (k, v) in &other_vars_before {
                prop_assert_eq!(result.get(k), Some(v));
            }
            prop_assert_eq!(
                result.get("ANTHROPIC_BASE_URL").cloned(),
                Some(format!("http://{gateway}:{port}"))
            );
            let headers = result
                .get("ANTHROPIC_CUSTOM_HEADERS")
                .cloned()
                .unwrap_or_default();
            let expected_host_header = format!("Host: {}", authority);
            let expected_token_header = format!("X-Tokenizer-Token: {}", token);
            prop_assert!(headers.contains(&expected_host_header));
            prop_assert!(headers.contains(&expected_token_header));
        }
    }
}
