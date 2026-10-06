//! Doctor check: a cleartext MCP URL whose hostname resolves to a public address (#2483).
//!
//! The resolver refuses a public IP literal outright. A hostname needs DNS, which is too slow
//! for every config render, so doctor does the lookup here.

use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use url::{Host, Url};

use crate::config::{Config, McpServer};
use crate::mcp::cleartext::is_cleartext_safe;
use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};

/// Bound on one DNS lookup, so a dead resolver cannot stall `llmenv doctor`.
const DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// One warning for each `http://` server whose hostname resolves outside the operator's network.
///
/// `lookup` maps `(host, port)` to the addresses of the host. A lookup that fails yields no
/// warning, because an unresolvable host is a different problem from a public one.
fn public_hostname_warnings(
    servers: &[ResolvedMcp],
    lookup: impl Fn(&str, u16) -> Vec<IpAddr>,
) -> Vec<String> {
    let mut out = Vec::new();
    for server in servers {
        let ResolvedKind::Remote { url, .. } = &server.kind else {
            continue;
        };
        let Ok(parsed) = Url::parse(url) else {
            continue;
        };
        let (true, Some(Host::Domain(host))) = (parsed.scheme() == "http", parsed.host()) else {
            continue;
        };
        let port = parsed.port_or_known_default().unwrap_or(80);
        if let Some(ip) = lookup(host, port)
            .into_iter()
            .find(|ip| !is_cleartext_safe(ip))
        {
            out.push(format!(
                "mcp '{}': cleartext http:// host {host} resolves to public address {ip}. \
                 Use an https:// URL, or move the server onto a loopback, private, or \
                 Tailscale address",
                server.name
            ));
        }
    }
    out
}

fn dns_lookup(host: &str, port: u16) -> Vec<IpAddr> {
    crate::hook_run::mcp_client::resolve_with_timeout(host, port, DNS_TIMEOUT)
        .map(|addrs| addrs.into_iter().map(|a| a.ip()).collect())
        .unwrap_or_default()
}

/// Warn about each cleartext MCP URL whose hostname resolves to a public address.
/// Prints nothing when every URL is safe.
pub(super) fn run_doctor_cleartext(
    use_color: bool,
    config: &Config,
    config_dir: &Path,
    active: &crate::scope::ActiveScopes,
    bundle_mcp: &[McpServer],
) {
    let mut servers = crate::hook_run::mcp_health::managed_servers(config, config_dir, active)
        .unwrap_or_default();
    // A resolve error is already fatal in `build_manifest`, so a failure here adds nothing.
    servers.extend(
        crate::mcp::resolve::resolve_mcps(&config.mcp, &[], &config.host, &active.tags)
            .unwrap_or_default(),
    );
    servers.extend(
        crate::mcp::resolve::resolve_bundle_mcps(bundle_mcp, &active.tags).unwrap_or_default(),
    );
    let warnings = public_hostname_warnings(&servers, dns_lookup);
    if warnings.is_empty() {
        return;
    }
    let warn = super::super::doctor_warning(use_color);
    eprintln!();
    eprintln!("MCP cleartext:");
    for w in warnings {
        eprintln!("{warn} {w}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::McpTransport;
    use std::collections::BTreeMap;

    fn remote(name: &str, url: &str) -> ResolvedMcp {
        ResolvedMcp {
            name: name.into(),
            kind: ResolvedKind::Remote {
                url: url.into(),
                transport: McpTransport::Http,
            },
            headers: BTreeMap::new(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
            always_load: None,
        }
    }

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn warns_when_an_http_hostname_resolves_public() {
        let servers = [remote("ctx7", "http://ctx7.example:8080/mcp")];
        let got = public_hostname_warnings(&servers, |host, port| {
            assert_eq!((host, port), ("ctx7.example", 8080));
            ips(&["93.184.216.34"])
        });
        assert_eq!(got.len(), 1);
        assert!(
            got[0].contains("ctx7") && got[0].contains("93.184.216.34"),
            "{got:?}"
        );
        assert!(got[0].contains("https://"), "{got:?}");
    }

    #[test]
    fn warns_when_any_one_address_is_public() {
        let servers = [remote("a", "http://mixed.example/mcp")];
        let got = public_hostname_warnings(&servers, |_, _| ips(&["10.0.0.4", "8.8.8.8"]));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn stays_quiet_for_private_https_literal_and_unresolved_hosts() {
        let servers = [
            remote("lan", "http://still.local:7878/mcp"),
            remote("tls", "https://ctx7.example/mcp"),
            remote("lit", "http://10.0.0.4/mcp"),
            remote("dead", "http://gone.example/mcp"),
        ];
        let got = public_hostname_warnings(&servers, |host, _| match host {
            "still.local" => ips(&["192.168.1.5"]),
            "ctx7.example" => ips(&["93.184.216.34"]),
            _ => vec![],
        });
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn skips_a_stdio_server() {
        let mut s = remote("local", "http://x.example/");
        s.kind = ResolvedKind::Stdio {
            command: "echo".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        assert!(public_hostname_warnings(&[s], |_, _| ips(&["8.8.8.8"])).is_empty());
    }
}
