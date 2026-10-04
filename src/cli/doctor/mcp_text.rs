//! `llmenv doctor`: MCP text that Claude Code cuts (#2148).
//!
//! Design: docs/design/issue-2148-mcp-description-cap.md

use super::CheckLevel;
use crate::mcp::resolve::{ResolvedKind, ResolvedMcp};
use llmenv_mcp::probe::{McpTextReport, effective_limit, parse_limit, probe};

/// What doctor learned about one server.
#[derive(Debug)]
pub(super) enum Measured {
    Report(McpTextReport),
    /// The probe failed. A server can be down while doctor runs, so this is information.
    Failed(String, String),
    /// A stdio server that doctor did not start.
    NotStarted(String),
    /// A server of a transport that doctor does not probe.
    Unsupported(String, String),
}

/// The doctor lines for the measured servers, in the given order.
fn checks(measured: &[Measured], limit: usize) -> Vec<(CheckLevel, String)> {
    let mut out = Vec::new();
    for item in measured {
        match item {
            Measured::Failed(server, error) => out.push((
                CheckLevel::Info,
                format!("{server}: not measured ({error})"),
            )),
            Measured::NotStarted(server) => out.push((
                CheckLevel::Info,
                format!(
                    "{server}: stdio server not started; run llmenv doctor --probe-mcp to \
                     measure it"
                ),
            )),
            Measured::Unsupported(server, transport) => out.push((
                CheckLevel::Info,
                format!("{server}: {transport} servers are not probed"),
            )),
            Measured::Report(report) => out.extend(report_checks(report, limit)),
        }
    }
    out
}

fn report_checks(report: &McpTextReport, limit: usize) -> Vec<(CheckLevel, String)> {
    let server = &report.server;
    let mut out = Vec::new();
    if let Some(chars) = report.instructions_chars.filter(|n| *n > limit) {
        out.push((
            CheckLevel::Warn,
            format!(
                "{server}: instructions are {chars} characters; Claude Code sends the first {limit}"
            ),
        ));
    }
    for (tool, chars) in report.tools.iter().filter(|(_, n)| *n > limit) {
        out.push((
            CheckLevel::Warn,
            format!(
                "{server}/{tool}: description is {chars} characters; Claude Code sends the \
                 first {limit}"
            ),
        ));
    }
    if out.is_empty() {
        let instructions = report
            .instructions_chars
            .map_or_else(|| "none".to_string(), |n| n.to_string());
        let longest = report.tools.iter().map(|(_, n)| *n).max().unwrap_or(0);
        out.push((
            CheckLevel::Pass,
            format!(
                "{server}: instructions {instructions}, {} tools, longest description {longest} \
                 characters",
                report.tools.len()
            ),
        ));
    }
    out
}

/// Probe every server at once and return the results in manifest order.
async fn measure_all(servers: &[ResolvedMcp], probe_stdio: bool) -> Vec<Measured> {
    let mut tasks = tokio::task::JoinSet::new();
    let mut slots: Vec<Option<Measured>> = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        slots.push(None);
        match &server.kind {
            ResolvedKind::Stdio { .. } if !probe_stdio => {
                slots[index] = Some(Measured::NotStarted(server.name.clone()));
            }
            ResolvedKind::Remote { transport, .. }
                if *transport != crate::config::McpTransport::Http =>
            {
                slots[index] = Some(Measured::Unsupported(
                    server.name.clone(),
                    format!("{transport:?}").to_uppercase(),
                ));
            }
            _ => {
                let server = server.clone();
                tasks.spawn(async move {
                    let outcome = match probe(&server).await {
                        Ok(report) => Measured::Report(report),
                        Err(e) => Measured::Failed(
                            server.name.clone(),
                            llmenv_mcp::stdio_rpc::tidy_reason(&format!("{e:#}")),
                        ),
                    };
                    (index, outcome)
                });
            }
        }
    }
    let mut crashes = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((index, outcome)) => slots[index] = Some(outcome),
            Err(e) => crashes.push(e.to_string()),
        }
    }
    slots
        .into_iter()
        .zip(servers)
        .map(|(slot, server)| {
            slot.unwrap_or_else(|| {
                Measured::Failed(
                    server.name.clone(),
                    format!("the probe crashed: {}", crashes.join("; ")),
                )
            })
        })
        .collect()
}

/// Print the section. `limit_env` is `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` as Claude Code
/// sees it.
pub(super) fn run_doctor_mcp_text(
    use_color: bool,
    servers: &[ResolvedMcp],
    limit_env: Option<&str>,
    probe_stdio: bool,
) {
    if servers.is_empty() {
        return;
    }
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    let limit = effective_limit(limit_env);
    eprintln!();
    eprintln!("MCP text limits (Claude Code keeps {limit} characters):");
    if let Some(value) = limit_env.filter(|v| parse_limit(v).is_none()) {
        eprintln!(
            "{info} CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH={value:?} is not 1 to 9 digits and not \
             zero, so Claude Code ignores it"
        );
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("{warn} MCP text check skipped: cannot start a runtime: {e}");
            return;
        }
    };
    let measured = runtime.block_on(measure_all(servers, probe_stdio));
    for check in checks(&measured, limit) {
        super::print_check(check, &pass, &warn, &info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(server: &str, instructions: Option<usize>, tools: &[(&str, usize)]) -> Measured {
        Measured::Report(McpTextReport {
            server: server.into(),
            instructions_chars: instructions,
            tools: tools.iter().map(|(n, c)| ((*n).to_string(), *c)).collect(),
        })
    }

    #[test]
    fn a_server_under_the_limit_passes_with_its_counts() {
        let lines = checks(&[report("icm", Some(100), &[("a", 10), ("b", 30)])], 2048);
        assert_eq!(
            lines,
            [(
                CheckLevel::Pass,
                "icm: instructions 100, 2 tools, longest description 30 characters".to_string()
            )]
        );
        let none = checks(&[report("s", None, &[])], 2048);
        assert!(
            none[0]
                .1
                .contains("instructions none, 0 tools, longest description 0")
        );
    }

    #[test]
    fn text_over_the_limit_warns_per_item_and_the_limit_itself_is_fine() {
        let lines = checks(
            &[report("s", Some(2049), &[("big", 3000), ("edge", 2048)])],
            2048,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|(l, _)| *l == CheckLevel::Warn));
        assert!(lines[0].1.contains("s: instructions are 2049 characters"));
        assert!(lines[1].1.contains("s/big: description is 3000 characters"));
        assert!(lines[1].1.contains("first 2048"));
    }

    #[test]
    fn a_smaller_limit_changes_what_warns() {
        let lines = checks(&[report("s", Some(10), &[("t", 5)])], 8);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].1.contains("instructions are 10 characters"));
    }

    #[test]
    fn failures_and_skips_are_information() {
        let lines = checks(
            &[
                Measured::Failed("down".into(), "refused".into()),
                Measured::NotStarted("cbm".into()),
                Measured::Unsupported("old".into(), "SSE".into()),
            ],
            2048,
        );
        assert!(lines.iter().all(|(l, _)| *l == CheckLevel::Info));
        assert_eq!(lines[0].1, "down: not measured (refused)");
        assert!(lines[1].1.contains("--probe-mcp"));
        assert_eq!(lines[2].1, "old: SSE servers are not probed");
    }

    fn stdio(name: &str) -> ResolvedMcp {
        ResolvedMcp {
            always_load: None,
            name: name.into(),
            kind: ResolvedKind::Stdio {
                command: "llmenv-no-such-binary".into(),
                args: vec![],
                env: Default::default(),
            },
            headers: Default::default(),
            timeout: None,
            disabled_tools: vec![],
            mcp_permissions: None,
            memory_hook: None,
        }
    }

    #[tokio::test]
    async fn stdio_servers_are_not_started_without_the_flag() {
        let measured = measure_all(&[stdio("cbm")], false).await;
        assert!(matches!(&measured[0], Measured::NotStarted(n) if n == "cbm"));
    }

    #[tokio::test]
    async fn results_keep_the_manifest_order_and_name_a_failed_server() {
        let measured = measure_all(&[stdio("a"), stdio("b")], true).await;
        let names: Vec<Option<&str>> = measured
            .iter()
            .map(|m| match m {
                Measured::Failed(name, _) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, [Some("a"), Some("b")]);
    }

    #[tokio::test]
    async fn an_sse_server_is_reported_not_probed() {
        let mut sse = stdio("s");
        sse.kind = ResolvedKind::Remote {
            url: "http://127.0.0.1:1/sse".into(),
            transport: crate::config::McpTransport::Sse,
        };
        let measured = measure_all(&[sse], true).await;
        assert!(matches!(&measured[0], Measured::Unsupported(n, t) if n == "s" && t == "SSE"));
    }
}
