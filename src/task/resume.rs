//! Resume context for task sessions (#2339): what a fresh agent needs to pick a session back
//! up after `/clear`. Every field is optional, so a state file from before this type loads.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The largest issue number `detect_issue` accepts, as a digit count. No repo reaches eight
/// digits, and the bound stops a long digit run from becoming a wrong number.
const MAX_ISSUE_DIGITS: usize = 7;

/// Shown when a session starts with nothing recorded.
pub(crate) const MISSING_CONTEXT_NUDGE: &str = "No resume context recorded. Pass --context, \
    --issue, --doc, or --memory-topic to `llmenv task session start`, or run \
    `llmenv task session edit`.";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResumeContext {
    /// Free text, multi-line. Appended to by `session note`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<String>,
    /// GitHub issue numbers this session works on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) issues: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    /// The branch the work merges into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base: Option<String>,
    /// ICM topics to recall when resuming.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) memory_topics: Vec<String>,
    /// Plan and spec files, as paths relative to the repo root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) docs: Vec<String>,
}

impl ResumeContext {
    /// True when nothing is recorded.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// True when a cold reader could not tell what the work is: no notes, issue, doc, or memory
    /// topic. A branch alone does not say that, so it does not count.
    #[must_use]
    pub(crate) fn needs_nudge(&self) -> bool {
        self.context.is_none()
            && self.issues.is_empty()
            && self.docs.is_empty()
            && self.memory_topics.is_empty()
    }

    /// A context holding only what llmenv can detect in `cwd`.
    #[must_use]
    pub(crate) fn detected(cwd: &Path) -> Self {
        let mut detected = Self::default();
        detected.fill_detected(git_branch(cwd).as_deref());
        detected
    }

    /// Fold `update` in. A set value replaces the old one. A list adds the entries it lacks.
    /// A field that `update` leaves empty keeps its old value.
    pub(crate) fn apply(&mut self, update: &Self) {
        if update.context.is_some() {
            self.context.clone_from(&update.context);
        }
        if update.branch.is_some() {
            self.branch.clone_from(&update.branch);
        }
        if update.base.is_some() {
            self.base.clone_from(&update.base);
        }
        extend_unique(&mut self.issues, &update.issues);
        extend_unique(&mut self.memory_topics, &update.memory_topics);
        extend_unique(&mut self.docs, &update.docs);
    }

    /// Append `text` to the free-text context on a new line.
    pub(crate) fn append_note(&mut self, text: &str) {
        match &mut self.context {
            Some(existing) if !existing.is_empty() => {
                existing.push('\n');
                existing.push_str(text);
            }
            slot => *slot = Some(text.to_string()),
        }
    }

    /// Fill `branch` from `git_branch`, and `issues` from the branch name, only where the
    /// user set nothing. A value the user set is never overwritten.
    pub(crate) fn fill_detected(&mut self, git_branch: Option<&str>) {
        if self.branch.is_none() {
            self.branch = git_branch.map(str::to_string);
        }
        if self.issues.is_empty()
            && let Some(number) = self.branch.as_deref().and_then(detect_issue)
        {
            self.issues.push(number);
        }
    }

    /// A short form for a human or an agent. Names the next command to run for each ref.
    #[must_use]
    pub(crate) fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut lines = vec!["Resume context:".to_string()];
        if let Some(context) = &self.context {
            lines.extend(context.lines().map(|line| format!("  {}", clean(line))));
        }
        let (branch, base) = (
            self.branch.as_deref().map(clean),
            self.base.as_deref().map(clean),
        );
        match (&branch, &base) {
            (Some(branch), Some(base)) => lines.push(format!("  Branch: {branch} (base: {base})")),
            (Some(branch), None) => lines.push(format!("  Branch: {branch}")),
            (None, Some(base)) => lines.push(format!("  Base: {base}")),
            (None, None) => {}
        }
        lines.extend(
            self.issues
                .iter()
                .map(|n| format!("  Issue #{n}: gh issue view {n}")),
        );
        lines.extend(self.memory_topics.iter().map(|topic| {
            let topic = clean(topic);
            format!("  Memory topic {topic}: icm_memory_recall with topic \"{topic}\"")
        }));
        lines.extend(self.docs.iter().map(|doc| format!("  Doc: {}", clean(doc))));
        lines.join("\n")
    }
}

/// The issue number in a branch name such as `feat/2337-foo`, `fix/2358`, or
/// `ranger/fix/2337-foo`. `None` when the last path segment does not start with 1 to 7 ASCII
/// digits followed by `-` or the end of the name.
#[must_use]
fn detect_issue(branch: &str) -> Option<u32> {
    let (_, last) = branch.rsplit_once('/')?;
    let digit_count = last.bytes().take_while(u8::is_ascii_digit).count();
    // ASCII digits are one byte each, so `digit_count` is always a char boundary.
    let (digits, rest) = last.split_at(digit_count);
    if digits.is_empty() || digits.len() > MAX_ISSUE_DIGITS {
        return None;
    }
    if !rest.is_empty() && !rest.starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

/// The checked-out branch in `cwd`. `None` outside a git repo and on a detached HEAD.
#[must_use]
pub(crate) fn git_branch(cwd: &Path) -> Option<String> {
    let output = llmenv_git::secure_git()
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Drop control characters, so agent-written text cannot spoof terminal output.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn extend_unique<T: PartialEq + Clone>(into: &mut Vec<T>, from: &[T]) {
    for item in from {
        if !into.contains(item) {
            into.push(item.clone());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn ctx() -> ResumeContext {
        ResumeContext {
            context: Some("first".to_string()),
            issues: vec![1],
            branch: Some("feat/1-a".to_string()),
            base: Some("main".to_string()),
            memory_topics: vec!["t1".to_string()],
            docs: vec!["a.md".to_string()],
        }
    }

    #[test]
    fn apply_replaces_set_values_and_keeps_unset_ones() {
        let mut old = ctx();
        old.apply(&ResumeContext {
            branch: Some("feat/2-b".to_string()),
            ..ResumeContext::default()
        });
        assert_eq!(old.branch.as_deref(), Some("feat/2-b"));
        assert_eq!(old.base.as_deref(), Some("main"));
        assert_eq!(old.context.as_deref(), Some("first"));
    }

    #[test]
    fn apply_adds_list_entries_it_lacks_without_duplicates() {
        let mut old = ctx();
        old.apply(&ResumeContext {
            issues: vec![1, 2],
            memory_topics: vec!["t2".to_string()],
            docs: vec!["a.md".to_string(), "b.md".to_string()],
            ..ResumeContext::default()
        });
        assert_eq!(old.issues, [1, 2]);
        assert_eq!(old.memory_topics, ["t1", "t2"]);
        assert_eq!(old.docs, ["a.md", "b.md"]);
    }

    #[test]
    fn append_note_starts_the_context_or_adds_a_line() {
        let mut empty = ResumeContext::default();
        empty.append_note("one");
        assert_eq!(empty.context.as_deref(), Some("one"));
        empty.append_note("two");
        assert_eq!(empty.context.as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn detect_issue_reads_the_number_after_the_type_prefix() {
        assert_eq!(detect_issue("feat/2337-foo"), Some(2337));
        assert_eq!(detect_issue("fix/2358"), Some(2358));
        assert_eq!(detect_issue("ranger/fix/2337-foo-bar"), Some(2337));
    }

    #[test]
    fn detect_issue_rejects_names_that_only_look_numeric() {
        for name in [
            "main",
            "release/3.x",
            "feat/abc-12",
            "feat/-12",
            "feat/12345678-too-long",
            "feat/12x-foo",
            "feat/\u{0662}\u{0663}-arabic-indic-digits",
            "2337-no-prefix",
            "",
        ] {
            assert_eq!(detect_issue(name), None, "{name:?}");
        }
    }

    #[test]
    fn fill_detected_records_branch_and_issue_from_git() {
        let mut c = ResumeContext::default();
        c.fill_detected(Some("feat/2337-foo"));
        assert_eq!(c.branch.as_deref(), Some("feat/2337-foo"));
        assert_eq!(c.issues, [2337]);
    }

    #[test]
    fn fill_detected_never_overwrites_what_the_user_set() {
        let mut c = ResumeContext {
            branch: Some("my-branch".to_string()),
            issues: vec![9],
            ..ResumeContext::default()
        };
        c.fill_detected(Some("feat/2337-foo"));
        assert_eq!(c.branch.as_deref(), Some("my-branch"));
        assert_eq!(c.issues, [9]);
    }

    #[test]
    fn fill_detected_reads_the_issue_from_a_user_set_branch() {
        let mut c = ResumeContext {
            branch: Some("fix/2358-x".to_string()),
            ..ResumeContext::default()
        };
        c.fill_detected(Some("main"));
        assert_eq!(c.branch.as_deref(), Some("fix/2358-x"));
        assert_eq!(c.issues, [2358]);
    }

    #[test]
    fn fill_detected_without_git_records_nothing() {
        let mut c = ResumeContext::default();
        c.fill_detected(None);
        assert!(c.is_empty());
    }

    #[test]
    fn render_names_the_next_command_for_each_ref() {
        let text = ResumeContext {
            issues: vec![2337],
            memory_topics: vec!["decisions-llmenv".to_string()],
            docs: vec!["docs/design/x.md".to_string()],
            context: Some("pick up at step 4".to_string()),
            branch: Some("feat/2337-foo".to_string()),
            base: Some("release/3.x".to_string()),
        }
        .render();
        for needle in [
            "pick up at step 4",
            "gh issue view 2337",
            "icm_memory_recall",
            "decisions-llmenv",
            "docs/design/x.md",
            "feat/2337-foo",
            "release/3.x",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn needs_nudge_ignores_a_branch_but_not_a_note_issue_doc_or_topic() {
        assert!(ResumeContext::default().needs_nudge());
        let branch_only = ResumeContext {
            branch: Some("main".to_string()),
            base: Some("main".to_string()),
            ..ResumeContext::default()
        };
        assert!(branch_only.needs_nudge());
        for informative in [
            ResumeContext {
                context: Some("x".into()),
                ..ResumeContext::default()
            },
            ResumeContext {
                issues: vec![1],
                ..ResumeContext::default()
            },
            ResumeContext {
                docs: vec!["a.md".into()],
                ..ResumeContext::default()
            },
            ResumeContext {
                memory_topics: vec!["t".into()],
                ..ResumeContext::default()
            },
        ] {
            assert!(!informative.needs_nudge(), "{informative:?}");
        }
    }

    fn git_repo_on(branch: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = llmenv_git::secure_git()
            .args(["init", "-q", "-b", branch])
            .current_dir(dir.path())
            .status()
            .expect("run git");
        assert!(status.success());
        dir
    }

    #[test]
    fn git_branch_reads_the_checked_out_branch_even_before_the_first_commit() {
        let repo = git_repo_on("feat/2337-foo");
        assert_eq!(git_branch(repo.path()).as_deref(), Some("feat/2337-foo"));
    }

    #[test]
    fn detected_fills_branch_and_issue_from_the_repo() {
        let repo = git_repo_on("fix/2358-bar");
        let detected = ResumeContext::detected(repo.path());
        assert_eq!(detected.branch.as_deref(), Some("fix/2358-bar"));
        assert_eq!(detected.issues, [2358]);
    }

    #[test]
    fn git_branch_is_none_for_a_missing_directory() {
        assert_eq!(git_branch(Path::new("/no/such/dir/for/llmenv")), None);
    }

    #[test]
    fn render_strips_terminal_control_sequences_from_every_field() {
        let text = ResumeContext {
            context: Some("ok\u{1b}]0;pwned\u{7}\nsecond".to_string()),
            branch: Some("feat/\u{1b}[31mred".to_string()),
            docs: vec!["a\u{0}.md".to_string()],
            ..ResumeContext::default()
        }
        .render();
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "{text:?}"
        );
        assert!(text.contains("second"), "{text}");
    }

    #[test]
    fn render_of_an_empty_context_is_empty() {
        assert_eq!(ResumeContext::default().render(), "");
    }

    mod props {
        use super::super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn detect_issue_finds_any_valid_number(n in 1u32..=9_999_999, slug in "[a-z][a-z0-9-]{0,20}") {
                prop_assert_eq!(detect_issue(&format!("feat/{n}-{slug}")), Some(n));
            }

            #[test]
            fn detect_issue_never_panics(name in ".*") {
                let _ = detect_issue(&name);
            }

            #[test]
            fn apply_is_idempotent(
                issues in proptest::collection::vec(1u32..100, 0..5),
                topics in proptest::collection::vec("[a-z]{1,6}", 0..5),
                branch in proptest::option::of("[a-z/0-9-]{1,12}"),
            ) {
                let update = ResumeContext { issues, memory_topics: topics, branch, ..ResumeContext::default() };
                let mut once = ResumeContext::default();
                once.apply(&update);
                let mut twice = once.clone();
                twice.apply(&update);
                prop_assert_eq!(once, twice);
            }
        }
    }
}
