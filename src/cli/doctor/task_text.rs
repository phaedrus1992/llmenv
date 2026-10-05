//! `llmenv doctor`: instruction text that contradicts the task tracker (#2457).
//!
//! The tracker injects its own rules. An instruction file that says the engine task tools are
//! blocked, or that forbids `llmenv task`, makes the agent follow the wrong rule.
//! Design: docs/design/issue-2438-task-tracking-nudges.md

use super::CheckLevel;
use crate::merge::MergedManifest;

const ENGINE_TOOLS: [&str; 3] = ["taskcreate", "tasklist", "taskupdate"];

/// Phrases that say a tool must not be used.
const PROHIBITIONS: [&str; 10] = [
    "blocked",
    "forbidden",
    "prohibited",
    "disabled",
    "not allowed",
    "do not use",
    "do not call",
    "don't use",
    "don't call",
    "never use",
];
/// Also a prohibition, with a trailing space, so `no use` does not match inside another word.
const NO_USE: &str = "no use ";

/// How far apart, in characters, a prohibition and the tool it targets may be.
const PROXIMITY: usize = 60;

/// The char positions where `phrase` occurs in `text` as a whole word or phrase.
fn word_hits(text: &[char], phrase: &str) -> Vec<usize> {
    let needle: Vec<char> = phrase.chars().collect();
    let boundary = |c: Option<&char>| c.is_none_or(|c| !c.is_alphanumeric() && *c != '_');
    (0..text.len().saturating_sub(needle.len().saturating_sub(1)))
        .filter(|&i| text[i..].starts_with(&needle))
        .filter(|&i| {
            let before = i.checked_sub(1).and_then(|j| text.get(j));
            boundary(before) && boundary(text.get(i + needle.len()))
        })
        .collect()
}

/// Whether a paragraph contradicts the tracker: a prohibition sits next to an engine task tool or
/// next to `llmenv task`. A paragraph that says the tools are redirected states the right fact.
/// This is a heuristic on words.
fn contradicts(paragraph: &str) -> bool {
    let lower = paragraph.to_lowercase().replace('\u{2019}', "'");
    if lower.contains("redirect") {
        return false;
    }
    let text: Vec<char> = lower.chars().collect();
    let mut prohibitions: Vec<usize> = PROHIBITIONS
        .iter()
        .flat_map(|p| word_hits(&text, p))
        // `blocked on the user` and `blocked by a hook` describe a state, not a ban.
        .filter(|&i| {
            let rest: String = text[i..].iter().take(14).collect();
            !["blocked on ", "blocked by ", "blocked until "]
                .iter()
                .any(|state| rest.starts_with(state))
        })
        .collect();
    prohibitions.extend(word_hits(&text, NO_USE.trim_end()));
    if prohibitions.is_empty() {
        return false;
    }
    let targets: Vec<usize> = ENGINE_TOOLS
        .iter()
        .copied()
        .chain(["llmenv task"])
        .flat_map(|t| word_hits(&text, t))
        .collect();
    prohibitions
        .iter()
        .any(|p| targets.iter().any(|t| p.abs_diff(*t) <= PROXIMITY))
}

/// Where a hit is, and its first 100 characters.
fn describe(place: &str, line: usize, paragraph: &str) -> String {
    let first = paragraph.lines().next().unwrap_or_default();
    let shown: String = first.trim().chars().take(100).collect();
    format!("{place}:{line}: {shown}")
}

/// The place and the text of each contradicting paragraph in the instruction text. A paragraph is
/// a run of lines up to a blank line, so a sentence that wraps across lines is still one unit.
fn find(manifest: &MergedManifest) -> Vec<String> {
    let mut found = Vec::new();
    let mut scan = |place: &dyn Fn(usize) -> String, text: &str| {
        let mut paragraph = String::new();
        let mut start = 1;
        for (n, line) in text.lines().chain(std::iter::once("")).enumerate() {
            if line.trim().is_empty() {
                if contradicts(&paragraph) {
                    found.push(describe(&place(start), start, &paragraph));
                }
                paragraph.clear();
                start = n + 2;
            } else {
                paragraph.push_str(line);
                paragraph.push('\n');
            }
        }
    };
    // The merged CLAUDE.md names each bundle in a separator comment, so a hit names its bundle.
    let bundle_at: Vec<String> = {
        let mut current = "unattributed".to_string();
        manifest
            .agents_md
            .lines()
            .map(|l| {
                if let Some(b) = super::instruction_size::separator_bundle(l) {
                    current = b;
                }
                current.clone()
            })
            .collect()
    };
    scan(
        &|line| {
            let bundle = bundle_at
                .get(line.saturating_sub(1))
                .map_or("", String::as_str);
            format!("CLAUDE.md (bundle {bundle})")
        },
        &manifest.agents_md,
    );
    for rule in &manifest.rules {
        let place = format!("{}/{}", rule.bundle, rule.rel.display());
        scan(&|_| place.clone(), &rule.body);
    }
    found
}

/// The checks for one manifest. `enabled` is `features.task_tracker.enabled`.
fn checks(manifest: &MergedManifest, enabled: bool, redirect: bool) -> Vec<(CheckLevel, String)> {
    if !enabled {
        return Vec::new();
    }
    let found = find(manifest);
    if found.is_empty() {
        return vec![(
            CheckLevel::Pass,
            "task tracker: no instruction text says the task tools are blocked".to_string(),
        )];
    }
    let mut out = vec![(
        CheckLevel::Warn,
        format!(
            "task tracker: {} instruction paragraph(s) say the task tools are blocked or forbid \
             `llmenv task`{}. Reword them in the source bundle, then run llmenv regenerate.",
            found.len(),
            if redirect {
                ", and the tracker redirects the engine tools instead"
            } else {
                ", but the tracker is on"
            }
        ),
    )];
    out.extend(found.into_iter().map(|f| (CheckLevel::Info, f)));
    out
}

/// Print the section.
pub(super) fn run_doctor_task_text(use_color: bool, manifest: &MergedManifest) {
    let tracker = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref());
    let enabled = tracker.is_some_and(|t| t.enabled);
    let redirect = tracker.is_none_or(|t| t.block_engine_task_tools);
    let list = checks(manifest, enabled, redirect);
    if list.is_empty() {
        return;
    }
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    eprintln!();
    eprintln!("Task tracker instructions:");
    for check in list {
        super::print_check(check, &pass, &warn, &info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::rules::RuleFile;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn manifest(agents_md: &str, rules: Vec<RuleFile>) -> MergedManifest {
        MergedManifest {
            agents_md: agents_md.into(),
            rules,
            ..Default::default()
        }
    }

    #[test]
    fn a_paragraph_that_blocks_the_engine_tools_or_forbids_llmenv_task_contradicts() {
        for (text, expected) in [
            (
                "TaskCreate, TaskList and TaskUpdate are \"blocked at the tool-call level\".",
                true,
            ),
            (
                "No use Claude's TaskCreate/TaskList/TaskUpdate (ephemeral).",
                true,
            ),
            ("Never use `llmenv task` here.", true),
            ("Do not use llmenv task for planning.", true),
            ("Don\u{2019}t use taskcreate.", true),
            ("The task tools are\nblocked: TaskCreate\nincluded.", true),
            ("TaskCreate calls are redirected to llmenv task.", false),
            ("TaskCreate is redirected, not blocked.", false),
            ("The commit is blocked by a hook.", false),
            ("Use llmenv task for each step.", false),
            ("Use llmenv task when a step is blocked on the user.", false),
            ("TaskUpdate sets blockedBy and unblocked items.", false),
            (
                "Do not use GitHub issues for steps. Elsewhere in this very long paragraph, which keeps going for more than sixty characters, use llmenv task.",
                false,
            ),
            ("", false),
        ] {
            assert_eq!(contradicts(text), expected, "{text:?}");
        }
    }

    #[test]
    fn the_places_and_line_numbers_name_each_hit() {
        let rule = RuleFile {
            bundle: "base".into(),
            rel: PathBuf::from("rules/tasks.md"),
            frontmatter: None,
            body: "ok\n\nNever use TaskCreate.".into(),
            raw: "ok\n\nNever use TaskCreate.".into(),
        };
        let m = manifest(
            "<!-- # from bundle: base -->\nfine\n\n<!-- # from bundle: extra -->\nDo not use llmenv task.\n",
            vec![rule],
        );
        let found = find(&m);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found[0].starts_with("CLAUDE.md (bundle extra):4:"),
            "{found:?}"
        );
        assert!(
            found[1].starts_with("base/rules/tasks.md:3: Never use TaskCreate"),
            "{found:?}"
        );
    }

    #[test]
    fn nothing_is_checked_while_the_tracker_is_off_and_a_clean_manifest_passes() {
        let bad = manifest("Never use llmenv task.", vec![]);
        assert!(checks(&bad, false, true).is_empty());
        let clean = manifest("Use llmenv task.", vec![]);
        let list = checks(&clean, true, true);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, CheckLevel::Pass);
        let flagged = checks(&bad, true, true);
        assert_eq!(flagged[0].0, CheckLevel::Warn);
        assert!(
            flagged[0].1.contains("1 instruction paragraph(s)"),
            "{flagged:?}"
        );
        assert!(
            flagged[0].1.contains("redirects the engine tools"),
            "{flagged:?}"
        );
        assert_eq!(flagged.len(), 2);
        let no_redirect = checks(&bad, true, false);
        assert!(
            no_redirect[0].1.contains("but the tracker is on"),
            "{no_redirect:?}"
        );
    }

    proptest! {
        #[test]
        fn a_line_with_no_prohibition_word_never_contradicts(line in "[A-Za-z0-9 `/.,]{0,80}") {
            let lower = line.to_lowercase();
            prop_assume!(!PROHIBITIONS.iter().any(|p| lower.contains(p)));
            prop_assert!(!contradicts(&line));
        }
    }
}
