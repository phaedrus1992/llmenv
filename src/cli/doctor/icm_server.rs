//! Doctor's verdict on the version of the ICM server (#2261).

use super::CheckLevel;
use super::version::parse_semver_triple;

/// ICM server releases that fix recall behavior llmenv depends on (#2261): 0.10.60 applies the
/// keyword/topic filter before the limit (adaptive recall fanout needs it), and 0.10.64 changes
/// the full-text query from AND to OR and fixes topic starvation. 0.11.3 answers a recall in
/// JSON with keywords, which the failure recall gate reads (#2921). 0.11.5 gives each JSON record
/// its id and links, takes several `queries` in one recall, and adds `icm_memory_related`; the
/// hook dedup, consolidation, and prune read those records (#3371).
const ICM_SERVER_FILTER_FIX: (u64, u64, u64) = (0, 10, 60);
const ICM_SERVER_RANKING_FIX: (u64, u64, u64) = (0, 10, 64);
const ICM_SERVER_JSON_RECALL: (u64, u64, u64) = (0, 11, 3);
const ICM_SERVER_JSON_RECORDS: (u64, u64, u64) = (0, 11, 5);

/// The oldest ICM server that llmenv fully supports, as text for messages.
pub(super) const ICM_SERVER_MIN: &str = "0.11.5";

/// Doctor's verdict on the local `icm` version, or on the reason that it is unknown (#3371).
pub(super) fn icm_version_verdict(version: Result<String, String>) -> (CheckLevel, String) {
    match version {
        Ok(version) => icm_server_check(&version),
        Err(reason) => (
            CheckLevel::Warn,
            format!(
                "{reason}. Fix: install icm {ICM_SERVER_MIN} or later, then run `llmenv doctor` \
                 again."
            ),
        ),
    }
}

/// Doctor's verdict on the version of the `icm` binary that serves memory (#2261).
fn icm_server_check(version: &str) -> (CheckLevel, String) {
    let upgrade = "Run `icm upgrade --apply`.";
    match parse_semver_triple(version) {
        None => (
            CheckLevel::Info,
            format!("ICM server reports version {version:?}, which is not semver"),
        ),
        Some(v) if v < ICM_SERVER_FILTER_FIX => (
            CheckLevel::Warn,
            format!(
                "ICM server {version} is older than 0.10.60: it filters recall by keyword and \
                 topic after the limit, so adaptive recall returns little or nothing. {upgrade}"
            ),
        ),
        Some(v) if v < ICM_SERVER_RANKING_FIX => (
            CheckLevel::Warn,
            format!(
                "ICM server {version} is older than 0.10.64: recall ranking is weaker (AND \
                 full-text query, topic starvation). {upgrade}"
            ),
        ),
        Some(v) if v < ICM_SERVER_JSON_RECALL => (
            CheckLevel::Warn,
            format!(
                "ICM server {version} is older than 0.11.3: it cannot answer a recall in JSON, so \
                 the tool-failure recall matches the memory summary only, not its keywords. \
                 {upgrade}"
            ),
        ),
        Some(v) if v < ICM_SERVER_JSON_RECORDS => (
            CheckLevel::Warn,
            format!(
                "ICM server {version} is older than {ICM_SERVER_MIN}: its JSON recall has no \
                 record ids, it takes one query per recall, and it has no icm_memory_related, so \
                 memory dedup and consolidation cannot read its records. Fix: upgrade icm to \
                 {ICM_SERVER_MIN} or later (`icm upgrade --apply`)."
            ),
        ),
        Some(_) => (CheckLevel::Pass, format!("ICM server {version}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The verdict depends only on where the version falls against the newest floor.
        #[test]
        fn icm_server_check_is_monotonic(a in 0u64..3, b in 0u64..20, c in 0u64..100) {
            let (level, _) = icm_server_check(&format!("{a}.{b}.{c}"));
            let expected = if (a, b, c) >= ICM_SERVER_JSON_RECORDS {
                CheckLevel::Pass
            } else {
                CheckLevel::Warn
            };
            prop_assert_eq!(level, expected);
        }
    }

    #[test]
    fn icm_server_check_boundaries() {
        let level = |v| icm_server_check(v).0;
        assert_eq!(level("0.10.59"), CheckLevel::Warn);
        assert_eq!(level("0.10.60"), CheckLevel::Warn);
        assert_eq!(level("0.10.63"), CheckLevel::Warn);
        assert_eq!(level("0.10.64"), CheckLevel::Warn);
        assert_eq!(level("0.11.2"), CheckLevel::Warn);
        assert_eq!(level("0.11.3"), CheckLevel::Warn);
        assert_eq!(level("0.11.4"), CheckLevel::Warn);
        assert_eq!(level("0.11.5"), CheckLevel::Pass);
        assert_eq!(level("v1.0.0"), CheckLevel::Pass);
        assert_eq!(level("dev"), CheckLevel::Info);
    }

    #[test]
    fn icm_server_check_requires_0_11_5_for_json_records_and_related() {
        let (level, text) = icm_server_check("0.11.4");
        assert_eq!(level, CheckLevel::Warn);
        assert!(text.contains("older than 0.11.5"), "{text}");
        assert!(
            text.contains("Fix: upgrade icm to 0.11.5 or later"),
            "{text}"
        );
        assert_eq!(icm_server_check("0.11.5").0, CheckLevel::Pass);
    }

    #[test]
    fn icm_version_verdict_names_the_fix_for_a_missing_binary() {
        let (level, text) = icm_version_verdict(Err("icm is not on PATH".to_string()));
        assert_eq!(level, CheckLevel::Warn);
        assert_eq!(
            text,
            "icm is not on PATH. Fix: install icm 0.11.5 or later, then run `llmenv doctor` again."
        );
        assert_eq!(
            icm_version_verdict(Ok("0.11.4".to_string())).0,
            CheckLevel::Warn
        );
        assert_eq!(
            icm_version_verdict(Ok("0.11.5".to_string())).0,
            CheckLevel::Pass
        );
    }

    #[test]
    fn icm_server_check_names_the_fix_the_server_lacks() {
        let (_, old) = icm_server_check("0.10.59");
        assert!(old.contains("older than 0.10.60"), "{old}");
        assert!(old.contains("icm upgrade --apply"), "{old}");
        let (_, mid) = icm_server_check("0.10.62");
        assert!(mid.contains("older than 0.10.64"), "{mid}");
        let (_, json) = icm_server_check("0.11.0");
        assert!(json.contains("older than 0.11.3"), "{json}");
    }
}
