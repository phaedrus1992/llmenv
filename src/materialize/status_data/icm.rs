//! The ICM stats collector for the status line: a best-effort live read of the memory counts.

use super::*;

/// This collector runs on hot paths (materialize, `llmenv export`, session
/// start) — not an interactive CLI invocation where a user is watching and
/// waiting is fine. A slow/unreachable ICM backend must not stall one of
/// those paths, so this uses the same short timeout the hook-run MCP calls
/// use rather than the 10s interactive-CLI timeout `memory::stats_json` uses.
const ICM_STATS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Best-effort live ICM query. Returns `None` when no memory backend is
/// active for the current scope, the MCP call fails or times out, or the
/// response can't be parsed — every one of those is an expected, non-error
/// condition (most sessions have no ICM backend configured at all).
pub(super) fn collect_icm() -> Option<IcmData> {
    icm_data_from(crate::memory::stats_json_with_timeout(ICM_STATS_TIMEOUT))
}

/// The status data for the result of the stats call. A scope with no backend is normal and logs at
/// debug. Any other failure, and a reply with no readable count, log at warn, so an empty field
/// has a visible cause (#3062).
fn icm_data_from(reply: anyhow::Result<String>) -> Option<IcmData> {
    let raw = match reply {
        Ok(raw) => raw,
        // Most scopes have no ICM backend, so that case is not a problem to report.
        Err(e) if crate::memory::is_no_backend(&e) => {
            tracing::debug!("icm stat collection skipped, no memory backend for this scope: {e}");
            return None;
        }
        Err(e) => {
            tracing::warn!(
                "icm stat collection failed, so the ICM field is empty: {e:#}. Fix: check that \
                 the ICM backend is running and reachable."
            );
            return None;
        }
    };
    let parsed = parse_icm_stats(&raw);
    if parsed.is_none() {
        // The status field is empty without this line, so a format change in ICM's reply is
        // visible instead of silent. The reply is cut to keep the log short.
        let head: String = raw.chars().take(200).collect();
        tracing::warn!(
            "icm stat reply has no readable Memories line, so the ICM field is empty. Reply \
             starts: {head:?}. Fix: check that the ICM backend returns the stats text."
        );
    }
    parsed
}

/// Pure text-extraction half of `collect_icm`, split out so it's testable
/// without a live ICM MCP backend.
///
/// The `icm_memory_stats` MCP tool has no structured-output mode — it always
/// returns a human-formatted text block (`"Memories: 2880\nTopics: 155\n..."`),
/// never JSON. `memories` is required. `concepts` (sourced from the tool's
/// "Topics" line — its closest analogous count) is `None` when the line is
/// absent, so an older or trimmed stats response shows an unknown count, not zero.
pub(super) fn parse_icm_stats(raw: &str) -> Option<IcmData> {
    Some(IcmData {
        memories: extract_stat_line(raw, "Memories")?,
        concepts: extract_stat_line(raw, "Topics"),
    })
}

/// Extract the `u64` value from a `"<label>: <n>"` line, e.g. `"Memories:
/// 2880"` for `label = "Memories"`. Whitespace-tolerant around the colon;
/// `None` if no line matches or the value doesn't parse.
pub(super) fn extract_stat_line(raw: &str, label: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        line.trim()
            .strip_prefix(label)?
            .trim_start()
            .strip_prefix(':')?
            .trim()
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::icm_data_from;
    use crate::test_log_capture::capture_logs;

    #[test]
    fn a_failed_call_warns_with_the_cause_and_a_reply_with_no_count_warns_with_its_head() {
        let mut failed = None;
        let logs = capture_logs(|| {
            failed = icm_data_from(Err(anyhow::anyhow!("connection refused by backend")));
        });
        assert!(failed.is_none());
        assert!(
            logs.contains("WARN") && logs.contains("connection refused by backend"),
            "{logs}"
        );
        assert!(logs.contains("Fix:"), "{logs}");

        let mut empty = None;
        let logs = capture_logs(|| {
            empty = icm_data_from(Ok("Topics: 3\n".to_owned()));
        });
        assert!(empty.is_none());
        assert!(
            logs.contains("WARN") && logs.contains("Memories line"),
            "{logs}"
        );
        assert!(logs.contains("Topics: 3"), "{logs}");
    }

    #[test]
    fn a_good_reply_logs_no_warning() {
        let mut data = None;
        let logs = capture_logs(|| {
            // The control line proves the capture works, so a missing WARN is not a lost log.
            tracing::info!("capture-control");
            data = icm_data_from(Ok("Memories: 12\nTopics: 3\n".to_owned()));
        });
        assert!(logs.contains("capture-control"), "{logs}");
        assert_eq!(data.map(|d| d.memories), Some(12));
        assert!(!logs.contains("WARN"), "{logs}");
    }
}
