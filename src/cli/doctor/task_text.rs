//! `llmenv doctor`: instruction text that contradicts the task tracker (#2457).
//!
//! The tracker injects its own rules. An instruction file that says the engine task tools are
//! blocked, or that forbids `llmenv task`, makes the agent follow the wrong rule.
//! Design: docs/design/issue-2438-task-tracking-nudges.md

use super::CheckLevel;
use crate::merge::MergedManifest;

const ENGINE_TOOLS: [&str; 3] = ["TaskCreate", "TaskList", "TaskUpdate"];

/// Words that say a tool must not be used.
const PROHIBITIONS: [&str; 7] = [
    "blocked",
    "forbidden",
    "do not use",
    "don't use",
    "never use",
    "no use ",
    "must not use",
];

/// Whether a line contradicts the tracker: it forbids the engine task tools in the words of
/// [`PROHIBITIONS`], or it forbids `llmenv task`. This is a heuristic on words.
fn contradicts(line: &str) -> bool {
    let lower = line.to_lowercase();
    let prohibits = PROHIBITIONS.iter().any(|p| lower.contains(p));
    if !prohibits {
        return false;
    }
    ENGINE_TOOLS.iter().any(|t| line.contains(t)) || lower.contains("llmenv task")
}

/// The place and the text of each contradicting line in the always-read instruction text.
fn find(manifest: &MergedManifest) -> Vec<String> {
    let mut found = Vec::new();
    let mut scan = |place: &str, text: &str| {
        for (n, line) in text.lines().enumerate() {
            if contradicts(line) {
                let shown: String = line.trim().chars().take(100).collect();
                found.push(format!("{place}:{}: {shown}", n + 1));
            }
        }
    };
    scan("CLAUDE.md", &manifest.agents_md);
    for rule in &manifest.rules {
        scan(
            &format!("{}/{}", rule.bundle, rule.rel.display()),
            &rule.body,
        );
    }
    found
}

/// The checks for one manifest. `enabled` is `features.task_tracker.enabled`.
fn checks(manifest: &MergedManifest, enabled: bool) -> Vec<(CheckLevel, String)> {
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
            "task tracker: {} instruction line(s) say the task tools are blocked or forbid \
             `llmenv task`, and the tracker redirects them instead. Reword them in the source \
             bundle, then run llmenv regenerate.",
            found.len()
        ),
    )];
    out.extend(found.into_iter().map(|f| (CheckLevel::Info, f)));
    out
}

/// Print the section.
pub(super) fn run_doctor_task_text(use_color: bool, manifest: &MergedManifest) {
    let enabled = manifest
        .capabilities
        .features
        .as_ref()
        .and_then(|f| f.task_tracker.as_ref())
        .is_some_and(|t| t.enabled);
    let list = checks(manifest, enabled);
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
    fn a_line_that_blocks_the_engine_tools_or_forbids_llmenv_task_contradicts() {
        for (line, expected) in [
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
            ("TaskCreate calls are redirected to llmenv task.", false),
            ("The commit is blocked by a hook.", false),
            ("Use llmenv task for each step.", false),
            ("", false),
        ] {
            assert_eq!(contradicts(line), expected, "{line:?}");
        }
    }

    #[test]
    fn the_places_and_line_numbers_name_each_hit() {
        let rule = RuleFile {
            bundle: "base".into(),
            rel: PathBuf::from("rules/tasks.md"),
            frontmatter: None,
            body: "ok\nNever use TaskCreate.".into(),
            raw: "ok\nNever use TaskCreate.".into(),
        };
        let m = manifest("fine\nDo not use llmenv task.\n", vec![rule]);
        let found = find(&m);
        assert_eq!(found.len(), 2);
        assert!(
            found[0].starts_with("CLAUDE.md:2: Do not use llmenv task"),
            "{found:?}"
        );
        assert!(
            found[1].starts_with("base/rules/tasks.md:2: Never use TaskCreate"),
            "{found:?}"
        );
    }

    #[test]
    fn nothing_is_checked_while_the_tracker_is_off_and_a_clean_manifest_passes() {
        let bad = manifest("Never use llmenv task.", vec![]);
        assert!(checks(&bad, false).is_empty());
        let clean = manifest("Use llmenv task.", vec![]);
        let list = checks(&clean, true);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, CheckLevel::Pass);
        let flagged = checks(&bad, true);
        assert_eq!(flagged[0].0, CheckLevel::Warn);
        assert!(
            flagged[0].1.contains("1 instruction line(s)"),
            "{flagged:?}"
        );
        assert_eq!(flagged.len(), 2);
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
