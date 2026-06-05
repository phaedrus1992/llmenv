//! Resolves configured MCP servers into concrete, render-ready [`ResolvedMcp`]
//! entries for the active host.
//!
//! Two sources feed the resolver:
//! - `config.mcp` — plain user-declared servers, selected when any of their
//!   `tags` intersect the active scope tag set (same model as bundles).
//! - `config.features.memory` — llmenv's own memory backend (ICM). One host runs the
//!   daemon (`icm serve`, stdio-only) wrapped in `mcp-proxy` to expose it on
//!   the network; the CLI launches that proxy on the designated `server_host`.
//!   Every agent — including the one on the server host — connects to the
//!   *network* endpoint, so the resolved entry is always a remote HTTP client.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Config, HostEntry, McpServer, McpTransport, Memory};

/// A fully resolved MCP entry ready for an adapter to render. Transport-shaped:
/// `Stdio` carries a launch command; `Remote` carries a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMcp {
    /// Registration name in the agent's MCP config.
    pub name: String,
    pub kind: ResolvedKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedKind {
    /// Local subprocess launched over stdio.
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    /// Remote endpoint reached by URL. `transport` distinguishes `http`/`sse`
    /// for adapters that care; `stdio` never reaches this variant.
    Remote {
        url: String,
        transport: McpTransport,
    },
}

/// Registration name for the memory backend in the agent's MCP config.
pub const MEMORY_MCP_NAME: &str = "icm";

/// Errors raised while resolving MCP config for the active host.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("memory: server_host '{0}' has no entry in the `host:` table")]
    MemoryUnknownServerHost(String),
    #[error("mcp '{name}': stdio transport requires a `command`")]
    StdioMissingCommand { name: String },
    #[error("mcp '{name}': {transport} transport requires a `url`")]
    RemoteMissingUrl { name: String, transport: String },
    #[error("bundle mcp '{0}': name is reserved for the memory backend")]
    BundleMcpReservedName(String),
}

/// Select and resolve all MCP servers for the active host.
///
/// `active_tags` is the union of tags emitted by matching scopes. Plain `mcp`
/// entries come first in declaration order, followed by the memory backend when
/// selected. The memory backend always resolves to a network (HTTP) client —
/// even the host running the daemon connects through the proxy — so the role of
/// this host doesn't affect what gets rendered, only whether the CLI launches
/// the proxy locally (see [`crate::cli`]).
///
/// # Errors
/// Returns the first [`ResolveError`] encountered: a plain server missing its
/// required `command`/`url`, or a memory backend referencing an unknown host.
pub fn resolve_mcps(
    config: &Config,
    active_tags: &BTreeSet<String>,
) -> Result<Vec<ResolvedMcp>, ResolveError> {
    let mut out = Vec::new();
    for m in &config.mcp {
        if !m.tags.iter().any(|t| active_tags.contains(t)) {
            continue;
        }
        out.push(resolve_static(m)?);
    }
    if let Some(mem) = config.features.as_ref().and_then(|f| f.memory.as_ref())
        && mem.tags.iter().any(|t| active_tags.contains(t))
    {
        out.push(resolve_memory(mem, &config.host)?);
    }
    Ok(out)
}

/// Select and resolve MCP servers contributed by bundle `capabilities.mcp`
/// entries.
///
/// Bundle-level MCP entries follow a relaxed tag rule compared to top-level
/// servers: an entry with **no tags** is always active (the bundle's own scope
/// selection already acted as the gate). An entry that *does* carry tags is
/// further filtered against `active_tags` as usual.
///
/// # Errors
/// Returns the first [`ResolveError`] encountered: a server using the
/// reserved `"icm"` name, or a server missing its required `command`/`url`.
pub fn resolve_bundle_mcps(
    bundle_mcps: &[crate::config::McpServer],
    active_tags: &BTreeSet<String>,
) -> Result<Vec<ResolvedMcp>, ResolveError> {
    let mut out = Vec::new();
    for m in bundle_mcps {
        if m.name == MEMORY_MCP_NAME {
            return Err(ResolveError::BundleMcpReservedName(m.name.clone()));
        }
        let active = m.tags.is_empty() || m.tags.iter().any(|t| active_tags.contains(t));
        if active {
            out.push(resolve_static(m)?);
        }
    }
    Ok(out)
}

/// Render a plain server entry by its transport.
fn resolve_static(m: &McpServer) -> Result<ResolvedMcp, ResolveError> {
    let kind = match m.transport {
        McpTransport::Stdio => stdio_kind(m)?,
        McpTransport::Http | McpTransport::Sse => remote_kind(m, m.transport)?,
    };
    Ok(ResolvedMcp {
        name: m.name.clone(),
        kind,
    })
}

/// Resolve the memory backend to a network HTTP client at the server host's
/// address. Every agent connects this way, including the one on the host that
/// runs the daemon — the local proxy (launched by the CLI) is what bridges the
/// stdio `icm serve` process onto the network.
fn resolve_memory(
    mem: &Memory,
    hosts: &BTreeMap<String, HostEntry>,
) -> Result<ResolvedMcp, ResolveError> {
    let entry = hosts
        .get(&mem.server_host)
        .ok_or_else(|| ResolveError::MemoryUnknownServerHost(mem.server_host.clone()))?;
    Ok(ResolvedMcp {
        name: MEMORY_MCP_NAME.to_string(),
        kind: ResolvedKind::Remote {
            url: format!("http://{}:{}/mcp", entry.addr, mem.port),
            transport: McpTransport::Http,
        },
    })
}

fn stdio_kind(m: &McpServer) -> Result<ResolvedKind, ResolveError> {
    let command = m
        .command
        .clone()
        .ok_or_else(|| ResolveError::StdioMissingCommand {
            name: m.name.clone(),
        })?;
    Ok(ResolvedKind::Stdio {
        command,
        args: m.args.clone(),
        env: m.env.clone(),
    })
}

fn remote_kind(m: &McpServer, transport: McpTransport) -> Result<ResolvedKind, ResolveError> {
    let url = m
        .url
        .clone()
        .ok_or_else(|| ResolveError::RemoteMissingUrl {
            name: m.name.clone(),
            transport: format!("{transport:?}").to_lowercase(),
        })?;
    Ok(ResolvedKind::Remote { url, transport })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::{Features, McpServer, Memory};

    fn tags(ts: &[&str]) -> BTreeSet<String> {
        ts.iter().map(|s| (*s).to_string()).collect()
    }

    fn base_config() -> Config {
        Config {
            host: BTreeMap::from([(
                "still".to_string(),
                HostEntry {
                    addr: "still.local".to_string(),
                },
            )]),
            ..Config::default()
        }
    }

    fn stdio_server(name: &str, tags: &[&str], command: &str) -> McpServer {
        McpServer {
            name: name.into(),
            tags: tags.iter().map(|s| (*s).into()).collect(),
            transport: McpTransport::Stdio,
            command: Some(command.into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
        }
    }

    fn memory() -> Memory {
        Memory {
            server_host: "still".into(),
            port: 7878,
            tags: vec!["network-home".into()],
            default_topics: vec![],
        }
    }

    #[test]
    fn selects_only_servers_with_intersecting_tags() {
        let mut cfg = base_config();
        cfg.mcp = vec![
            stdio_server("playwright", &["user-ranger"], "npx"),
            stdio_server("tolaria", &["host-still"], "tolaria-mcp"),
        ];
        let resolved = resolve_mcps(&cfg, &tags(&["user-ranger"])).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "playwright");
    }

    #[test]
    fn stdio_server_resolves_to_command() {
        let mut cfg = base_config();
        let mut s = stdio_server("playwright", &["t"], "npx");
        s.args = vec!["-y".into(), "@playwright/mcp@latest".into()];
        s.env = BTreeMap::from([("HEADLESS".to_string(), "1".to_string())]);
        cfg.mcp = vec![s];
        let resolved = resolve_mcps(&cfg, &tags(&["t"])).unwrap();
        match &resolved[0].kind {
            ResolvedKind::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y", "@playwright/mcp@latest"]);
                assert_eq!(env.get("HEADLESS").map(String::as_str), Some("1"));
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn memory_always_resolves_to_network_client() {
        let mut cfg = base_config();
        cfg.features = Some(Features {
            memory: Some(memory()),
        });
        // Same result whether or not this host is the server host: the agent
        // always talks to the network proxy.
        for active_tags in [tags(&["network-home"]), tags(&["network-home"])] {
            let resolved = resolve_mcps(&cfg, &active_tags).unwrap();
            assert_eq!(resolved[0].name, MEMORY_MCP_NAME);
            match &resolved[0].kind {
                ResolvedKind::Remote { url, transport } => {
                    assert_eq!(url, "http://still.local:7878/mcp");
                    assert_eq!(*transport, McpTransport::Http);
                }
                other => panic!("expected remote client, got {other:?}"),
            }
        }
    }

    #[test]
    fn memory_not_selected_when_tags_inactive() {
        let mut cfg = base_config();
        cfg.features = Some(Features {
            memory: Some(memory()),
        });
        let resolved = resolve_mcps(&cfg, &tags(&["unrelated"])).unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn memory_with_unknown_host_errors() {
        let mut cfg = base_config();
        cfg.host.clear();
        cfg.features = Some(Features {
            memory: Some(memory()),
        });
        let err = resolve_mcps(&cfg, &tags(&["network-home"])).unwrap_err();
        assert_eq!(err, ResolveError::MemoryUnknownServerHost("still".into()));
    }

    #[test]
    fn stdio_without_command_errors() {
        let mut cfg = base_config();
        let mut s = stdio_server("broken", &["t"], "x");
        s.command = None;
        cfg.mcp = vec![s];
        let err = resolve_mcps(&cfg, &tags(&["t"])).unwrap_err();
        assert_eq!(
            err,
            ResolveError::StdioMissingCommand {
                name: "broken".into()
            }
        );
    }

    #[test]
    fn http_server_resolves_to_remote() {
        let mut cfg = base_config();
        let mut s = stdio_server("ctx7", &["t"], "x");
        s.transport = McpTransport::Http;
        s.command = None;
        s.url = Some("https://ctx7.example/mcp".into());
        cfg.mcp = vec![s];
        let resolved = resolve_mcps(&cfg, &tags(&["t"])).unwrap();
        match &resolved[0].kind {
            ResolvedKind::Remote { url, transport } => {
                assert_eq!(url, "https://ctx7.example/mcp");
                assert_eq!(*transport, McpTransport::Http);
            }
            other => panic!("expected remote, got {other:?}"),
        }
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        // A resolvable stdio server with a fixed command, parameterised on a
        // unique name and a tag set, so resolution never fails on missing
        // fields and we can reason purely about selection.
        fn arb_server(idx: usize) -> impl Strategy<Value = McpServer> {
            prop::collection::vec("[a-z]{1,4}", 0..4).prop_map(move |ts| McpServer {
                name: format!("srv-{idx}"),
                tags: ts,
                transport: McpTransport::Stdio,
                command: Some("echo".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            })
        }

        fn arb_config_and_tags() -> impl Strategy<Value = (Config, BTreeSet<String>)> {
            let servers = (0usize..5).prop_flat_map(|n| (0..n).map(arb_server).collect::<Vec<_>>());
            let active = prop::collection::btree_set("[a-z]{1,4}", 0..6);
            (servers, active).prop_map(|(mcp, active)| {
                let mut cfg = base_config();
                cfg.mcp = mcp;
                (cfg, active)
            })
        }

        proptest! {
            // resolve_mcps never invents entries: the output never exceeds the
            // declared servers plus the (here unconfigured) memory backend.
            #[test]
            fn output_count_bounded_by_inputs((cfg, active) in arb_config_and_tags()) {
                let resolved = resolve_mcps(&cfg, &active).expect("valid servers resolve");
                prop_assert!(resolved.len() <= cfg.mcp.len());
            }

            // Every resolved static server corresponds to a declared server
            // whose tag set intersects the active tags.
            #[test]
            fn every_selected_server_has_active_tag((cfg, active) in arb_config_and_tags()) {
                let resolved = resolve_mcps(&cfg, &active).expect("valid servers resolve");
                for r in &resolved {
                    let src = cfg
                        .mcp
                        .iter()
                        .find(|m| m.name == r.name)
                        .expect("resolved name maps to a declared server");
                    prop_assert!(src.tags.iter().any(|t| active.contains(t)));
                }
            }

            // Resolution is a pure function of (config, tags).
            #[test]
            fn resolution_is_deterministic((cfg, active) in arb_config_and_tags()) {
                let a = resolve_mcps(&cfg, &active).expect("resolve");
                let b = resolve_mcps(&cfg, &active).expect("resolve");
                prop_assert_eq!(a, b);
            }
        }
    }

    // #329: resolve_bundle_mcps — tagless entries always active; tagged entries
    // filtered by active_tags.
    mod bundle_mcps {
        use super::*;

        #[test]
        fn tagless_entry_always_active() {
            let server = stdio_server("ctx", &[], "ctx-mcp");
            let resolved = resolve_bundle_mcps(&[server], &tags(&[])).unwrap();
            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].name, "ctx");
        }

        #[test]
        fn tagged_entry_active_when_tag_matches() {
            let server = stdio_server("playwright", &["user-ranger"], "npx");
            let resolved = resolve_bundle_mcps(&[server], &tags(&["user-ranger"])).unwrap();
            assert_eq!(resolved.len(), 1);
        }

        #[test]
        fn tagged_entry_inactive_when_no_tag_matches() {
            let server = stdio_server("playwright", &["user-ranger"], "npx");
            let resolved = resolve_bundle_mcps(&[server], &tags(&["network-office"])).unwrap();
            assert!(resolved.is_empty());
        }

        #[test]
        fn mix_of_tagless_and_tagged() {
            let always = stdio_server("always", &[], "always-mcp");
            let sometimes = stdio_server("sometimes", &["home"], "sometimes-mcp");
            let never = stdio_server("never", &["work"], "never-mcp");

            let resolved =
                resolve_bundle_mcps(&[always, sometimes, never], &tags(&["home"])).unwrap();
            assert_eq!(resolved.len(), 2);
            assert!(resolved.iter().any(|m| m.name == "always"));
            assert!(resolved.iter().any(|m| m.name == "sometimes"));
        }

        #[test]
        fn empty_input_yields_empty_output() {
            let resolved = resolve_bundle_mcps(&[], &tags(&["any"])).unwrap();
            assert!(resolved.is_empty());
        }

        #[test]
        fn stdio_missing_command_errors() {
            let mut s = stdio_server("broken", &[], "x");
            s.command = None;
            let err = resolve_bundle_mcps(&[s], &tags(&[])).unwrap_err();
            assert_eq!(
                err,
                ResolveError::StdioMissingCommand {
                    name: "broken".into()
                }
            );
        }

        #[test]
        fn reserved_icm_name_errors() {
            let s = stdio_server(MEMORY_MCP_NAME, &[], "attacker-binary");
            let err = resolve_bundle_mcps(&[s], &tags(&[])).unwrap_err();
            assert_eq!(
                err,
                ResolveError::BundleMcpReservedName(MEMORY_MCP_NAME.into())
            );
        }

        mod props {
            use super::*;
            use proptest::prelude::*;

            // A resolvable stdio server at index idx, with arbitrary tags.
            fn arb_bundle_server(idx: usize) -> impl Strategy<Value = McpServer> {
                prop::collection::vec("[a-z]{1,4}", 0..4).prop_map(move |ts| McpServer {
                    name: format!("bsrv-{idx}"),
                    tags: ts,
                    transport: McpTransport::Stdio,
                    command: Some("echo".into()),
                    args: vec![],
                    env: BTreeMap::new(),
                    url: None,
                })
            }

            fn arb_servers_and_tags() -> impl Strategy<Value = (Vec<McpServer>, BTreeSet<String>)> {
                let servers = (0usize..5)
                    .prop_flat_map(|n| (0..n).map(arb_bundle_server).collect::<Vec<_>>());
                let active = prop::collection::btree_set("[a-z]{1,4}", 0..6);
                (servers, active)
            }

            proptest! {
                // Every tagless entry appears in the output.
                #[test]
                fn tagless_entries_always_resolved(
                    (servers, active) in arb_servers_and_tags()
                ) {
                    let resolved = resolve_bundle_mcps(&servers, &active).expect("resolve");
                    let tagless_count = servers.iter().filter(|s| s.tags.is_empty()).count();
                    let tagless_in_output = resolved
                        .iter()
                        .filter(|r| servers.iter().any(|s| s.name == r.name && s.tags.is_empty()))
                        .count();
                    prop_assert_eq!(tagless_count, tagless_in_output);
                }

                // Tagged entries with no active-tag match are absent.
                #[test]
                fn tagged_entries_absent_when_no_match(
                    (servers, active) in arb_servers_and_tags()
                ) {
                    let resolved = resolve_bundle_mcps(&servers, &active).expect("resolve");
                    let resolved_names: BTreeSet<&str> =
                        resolved.iter().map(|r| r.name.as_str()).collect();
                    for s in &servers {
                        if !s.tags.is_empty() && !s.tags.iter().any(|t| active.contains(t)) {
                            prop_assert!(
                                !resolved_names.contains(s.name.as_str()),
                                "tagged server {} with no matching tag must be absent",
                                s.name
                            );
                        }
                    }
                }

                // Output count = tagless count + matched-tagged count.
                #[test]
                fn output_count_equals_tagless_plus_matched(
                    (servers, active) in arb_servers_and_tags()
                ) {
                    let resolved = resolve_bundle_mcps(&servers, &active).expect("resolve");
                    let expected = servers
                        .iter()
                        .filter(|s| {
                            s.tags.is_empty() || s.tags.iter().any(|t| active.contains(t))
                        })
                        .count();
                    prop_assert_eq!(resolved.len(), expected);
                }

                // Resolution is deterministic.
                #[test]
                fn resolve_bundle_mcps_is_deterministic(
                    (servers, active) in arb_servers_and_tags()
                ) {
                    let a = resolve_bundle_mcps(&servers, &active).expect("resolve");
                    let b = resolve_bundle_mcps(&servers, &active).expect("resolve");
                    prop_assert_eq!(a, b);
                }
            }
        }
    }
}
