//! Limits and cleanup for tags from untrusted sources: the charset, the length, and the count.

use std::collections::BTreeSet;

/// Charset every tag must satisfy, regardless of source: non-empty,
/// alphanumeric plus `-`/`_`. Matches `hook_run::validate_tag`'s recall-query
/// charset — a tag outside it trips that validation at ICM-recall time, and
/// the resulting error silently disables memory recall/store *and*
/// session-log for the whole session (#1035), so untrusted sources are
/// filtered here, at creation, rather than left to fail downstream.
pub fn is_valid_tag_charset(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Max bytes for a single tag from an untrusted source. Charset alone
/// doesn't bound length, and an oversized tag still bloats
/// `LLMENV_ACTIVE_TAGS` and every ICM recall keyword built from it (#1035).
pub(crate) const MAX_TAG_LEN: usize = 64;

/// Max tags accepted from a single untrusted source in one ingest pass. A
/// hand-written `.llmenv.yaml` never approaches this; a generated/scripted
/// `$LLMENV_EXTRA_TAGS` trivially could, and every active tag becomes one
/// `Action::RecallTag` per turn (#1035).
pub(crate) const MAX_TAGS_PER_SOURCE: usize = 64;

/// Drop tags that fail [`is_valid_tag_charset`] or exceed [`MAX_TAG_LEN`],
/// then cap the remainder to [`MAX_TAGS_PER_SOURCE`] — logging a warning
/// naming each rejected tag and any overflow. `source` labels the origin in
/// the warning (e.g. `.llmenv.yaml` or `$LLMENV_EXTRA_TAGS`). Also used for
/// bundle names (`enable_bundles`/`disable_bundles`), which share the same
/// charset rule and the same downstream `hook_run::validate_bundle` failure
/// mode as tags.
pub(crate) fn sanitize_tags(raw: Vec<String>, source: &str) -> Vec<String> {
    // A dropped tag never takes effect, and the user must hear about it. The agent sees a notice
    // only when it is recorded, and the file layer logs only at error level, so a warning would
    // reach no one (#1345, #3217).
    let mut valid = Vec::with_capacity(raw.len());
    for tag in raw {
        if !is_valid_tag_charset(&tag) {
            llmenv_util::agent_notice::record_logged(format!(
                "llmenv: {source} tag {tag:?} is dropped, because only letters, digits, - and _ \
                 are allowed. Fix: rename the tag to use only those characters."
            ));
        } else if tag.len() > MAX_TAG_LEN {
            llmenv_util::agent_notice::record_logged(format!(
                "llmenv: {source} tag {tag:?} is dropped, because it is over {MAX_TAG_LEN} bytes. \
                 Fix: shorten the tag."
            ));
        } else {
            valid.push(tag);
        }
    }
    if valid.len() > MAX_TAGS_PER_SOURCE {
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: {source} has {} tags, over the cap of {MAX_TAGS_PER_SOURCE}. Only the first \
             {MAX_TAGS_PER_SOURCE} are kept. Fix: remove the extra tags from {source}.",
            valid.len()
        ));
        valid.truncate(MAX_TAGS_PER_SOURCE);
    }
    valid
}

/// Cap the *union* of tags across every source (`config.yaml`'s network/
/// host/user/content scopes, `.llmenv.yaml`, `$LLMENV_EXTRA_TAGS`, `env.os`)
/// to [`MAX_TAGS_PER_SOURCE`] — [`sanitize_tags`] already bounds each source
/// individually, but several active scopes plus a large `.llmenv.yaml` plus
/// `$LLMENV_EXTRA_TAGS` can still combine into several hundred tags, each
/// becoming one `Action::RecallTag` per turn (#1041). Reuses
/// `MAX_TAGS_PER_SOURCE` as the aggregate bound too, rather than a second
/// magic number: the same "one source's worth" ceiling that's already
/// accepted as tolerable for a single source is exactly the ceiling worth
/// enforcing on the combination.
///
/// Keeps the alphabetically-first entries (`BTreeSet`'s natural iteration
/// order) — the same "keep the first N, warn about the rest" policy
/// `sanitize_tags` uses per-source, just applied to the union so no one
/// source is treated as more important than another when something has to
/// be dropped.
pub(crate) fn cap_aggregate_tags(tags: BTreeSet<String>) -> BTreeSet<String> {
    let total = tags.len();
    if total <= MAX_TAGS_PER_SOURCE {
        return tags;
    }
    llmenv_util::agent_notice::record_logged(format!(
        "llmenv: {total} tags across all sources are over the cap of {MAX_TAGS_PER_SOURCE}. Only \
         the alphabetically-first {MAX_TAGS_PER_SOURCE} are kept. Fix: remove some tags or scopes."
    ));
    tags.into_iter().take(MAX_TAGS_PER_SOURCE).collect()
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    /// A tag outside the charset is named, with its source and a fix (#3217).
    #[test]
    fn a_dropped_charset_tag_is_recorded_for_the_agent() {
        let kept = sanitize_tags(vec!["bad tag-3217-charset".into()], "src-3217-charset");
        assert!(kept.is_empty());
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("bad tag-3217-charset"), "{notices}");
        assert!(notices.contains("src-3217-charset"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    /// A tag over the length limit is named, with a fix (#3217).
    #[test]
    fn an_over_length_tag_is_recorded_for_the_agent() {
        let long = format!("long-3217-{}", "a".repeat(MAX_TAG_LEN));
        let kept = sanitize_tags(vec![long], "src-3217-long");
        assert!(kept.is_empty());
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("long-3217-"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    /// A source whose tag list is cut is named, with a fix (#3217).
    #[test]
    fn a_truncated_tag_list_is_recorded_for_the_agent() {
        let tags: Vec<String> = (0..=MAX_TAGS_PER_SOURCE).map(|i| format!("t{i}")).collect();
        let kept = sanitize_tags(tags, "src-3217-list");
        assert_eq!(kept.len(), MAX_TAGS_PER_SOURCE);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("src-3217-list"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }

    /// The aggregate cap is recorded for the agent, with a fix (#3217).
    #[test]
    fn an_aggregate_cap_overflow_is_recorded_for_the_agent() {
        let tags: BTreeSet<String> = (0..=MAX_TAGS_PER_SOURCE)
            .map(|i| format!("agg3217-{i:03}"))
            .collect();
        let kept = cap_aggregate_tags(tags);
        assert_eq!(kept.len(), MAX_TAGS_PER_SOURCE);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        assert!(notices.contains("across all sources"), "{notices}");
        assert!(notices.contains("Fix:"), "{notices}");
    }
}
