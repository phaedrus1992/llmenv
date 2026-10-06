//! Doctor check: a cleartext MCP URL whose hostname resolves to a public address (#2483).
//!
//! The resolver refuses a public IP literal outright. A hostname needs DNS, which is too slow
//! for every config render, so doctor does the lookup here.

use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use url::{Host, Url};

use crate::config::{Config, McpServer};
use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
use llmenv_mcp::cleartext::is_cleartext_safe;

/// Bound on one DNS lookup, so a dead resolver cannot stall `llmenv doctor`.
const DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// What the hostname check found across the cleartext servers.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    /// Servers whose hostname resolves outside the operator's network.
    warnings: Vec<String>,
    /// Hostnames that did not resolve, so the check could not run for them.
    unresolved: Vec<String>,
    /// Count of `http://` hostnames that resolved only inside the operator's network.
    safe: usize,
}

/// Check each `http://` server whose hostname may resolve outside the operator's network.
///
/// `lookup` maps `(host, port)` to the addresses of the host, or `None` when the lookup fails.
fn check_hostnames(
    servers: &[ResolvedMcp],
    lookup: impl Fn(&str, u16) -> Option<Vec<IpAddr>>,
) -> Report {
    let mut report = Report::default();
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
        let Some(addrs) = lookup(host, port) else {
            report
                .unresolved
                .push(format!("mcp '{}': cannot resolve {host}", server.name));
            continue;
        };
        match addrs.into_iter().find(|ip| !is_cleartext_safe(ip)) {
            Some(ip) => report.warnings.push(format!(
                "mcp '{}': cleartext http:// host {host} resolves to public address {ip}. \
                 Use an https:// URL, or move the server onto a loopback, private, or \
                 Tailscale address",
                server.name
            )),
            None => report.safe += 1,
        }
    }
    report
}

fn dns_lookup(host: &str, port: u16) -> Option<Vec<IpAddr>> {
    llmenv_mcp::mcp_client::resolve_with_timeout(host, port, DNS_TIMEOUT)
        .ok()
        .map(|addrs| addrs.into_iter().map(|a| a.ip()).collect())
}

/// Collect the resolved servers from each source. A source that fails to resolve adds one
/// message, so one broken source does not hide the others.
fn collect_servers(
    config: &Config,
    config_dir: &Path,
    active: &crate::scope::ActiveScopes,
    bundle_mcp: &[McpServer],
) -> (Vec<ResolvedMcp>, Vec<String>) {
    let mut servers = Vec::new();
    let mut errors = Vec::new();
    let mut take = |source: &str, result: anyhow::Result<Vec<ResolvedMcp>>| match result {
        Ok(found) => servers.extend(found),
        Err(e) => errors.push(format!("cannot check the {source} MCP servers: {e:#}")),
    };
    take(
        "memory",
        crate::hook_run::mcp_health::managed_servers(config, config_dir, active),
    );
    take(
        "configured",
        crate::mcp::resolve::resolve_mcps(&config.mcp, &[], &config.host, &active.tags)
            .map_err(anyhow::Error::from),
    );
    take(
        "bundle",
        crate::mcp::resolve::resolve_bundle_mcps(bundle_mcp, &active.tags)
            .map_err(anyhow::Error::from),
    );
    (servers, errors)
}

/// Warn about each cleartext MCP URL whose hostname resolves to a public address.
/// Prints nothing when no MCP server uses an `http://` hostname.
pub(super) fn run_doctor_cleartext(
    use_color: bool,
    config: &Config,
    config_dir: &Path,
    active: &crate::scope::ActiveScopes,
    bundle_mcp: &[McpServer],
) {
    let (servers, errors) = collect_servers(config, config_dir, active, bundle_mcp);
    let report = check_hostnames(&servers, dns_lookup);
    if errors.is_empty()
        && report.warnings.is_empty()
        && report.unresolved.is_empty()
        && report.safe == 0
    {
        return;
    }
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    eprintln!();
    eprintln!("MCP cleartext:");
    for e in &errors {
        eprintln!("{warn} {e}");
    }
    for w in &report.warnings {
        eprintln!("{warn} {w}");
    }
    for u in &report.unresolved {
        eprintln!("{info} {u}, so the public-address check did not run");
    }
    if report.safe > 0 && report.warnings.is_empty() {
        eprintln!(
            "{pass} {} http:// hostname(s) resolve inside the operator's network",
            report.safe
        );
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
        let got = check_hostnames(&servers, |host, port| {
            assert_eq!((host, port), ("ctx7.example", 8080));
            Some(ips(&["93.184.216.34"]))
        });
        assert_eq!(got.warnings.len(), 1);
        let w = &got.warnings[0];
        assert!(w.contains("ctx7") && w.contains("93.184.216.34"), "{w}");
        assert!(w.contains("https://"), "{w}");
    }

    #[test]
    fn warns_when_any_one_address_is_public() {
        let servers = [remote("a", "http://mixed.example/mcp")];
        let got = check_hostnames(&servers, |_, _| Some(ips(&["10.0.0.4", "8.8.8.8"])));
        assert_eq!(got.warnings.len(), 1);
        assert_eq!(got.safe, 0);
    }

    #[test]
    fn counts_private_hosts_and_skips_https_literals_and_stdio() {
        let mut stdio = remote("local", "http://x.example/");
        stdio.kind = ResolvedKind::Stdio {
            command: "echo".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let servers = [
            remote("lan", "http://still.local:7878/mcp"),
            remote("tls", "https://ctx7.example/mcp"),
            remote("lit", "http://10.0.0.4/mcp"),
            stdio,
        ];
        let got = check_hostnames(&servers, |host, _| {
            assert_eq!(host, "still.local", "only an http:// hostname is looked up");
            Some(ips(&["192.168.1.5"]))
        });
        assert_eq!(
            got,
            Report {
                warnings: vec![],
                unresolved: vec![],
                safe: 1
            }
        );
    }

    #[test]
    fn a_failed_lookup_is_reported_not_treated_as_safe() {
        let servers = [remote("dead", "http://gone.example/mcp")];
        let got = check_hostnames(&servers, |_, _| None);
        assert!(got.warnings.is_empty());
        assert_eq!(got.safe, 0);
        assert_eq!(got.unresolved.len(), 1);
        assert!(got.unresolved[0].contains("gone.example"), "{got:?}");
    }
}
