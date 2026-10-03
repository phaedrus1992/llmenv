//! Doctor's `Background work:` section (#2396): the detached jobs that left a checkpoint.
//! Design: docs/design/issue-2396-detached-checkpoints.md

use std::path::Path;

use super::{CheckLevel, print_check};
use crate::hook_run::checkpoint::{self, Entry};

/// One doctor line per checkpoint, or a single pass line when there is none. A checkpoint that is
/// not stale is `info`, because its child may still be running. A stale one is `warn`, and so is
/// a file that cannot be read.
pub(super) fn checkpoint_lines(
    entries: &[Entry],
    now: u64,
    log: &Path,
) -> Vec<(CheckLevel, String)> {
    if entries.is_empty() {
        return vec![(
            CheckLevel::Pass,
            "no unfinished background work".to_string(),
        )];
    }
    entries
        .iter()
        .map(|entry| match entry {
            Entry::Ready(_, cp) => {
                let level = if cp.is_stale(now) {
                    CheckLevel::Warn
                } else {
                    CheckLevel::Info
                };
                (level, checkpoint::describe(cp, now, log))
            }
            Entry::Unreadable(path, reason) => (
                CheckLevel::Warn,
                format!("unreadable checkpoint {}: {reason}", path.display()),
            ),
        })
        .collect()
}

pub(super) fn run_doctor_checkpoints(use_color: bool, state_dir: &Path) {
    let pass = super::super::doctor_pass(use_color);
    let warn = super::super::doctor_warning(use_color);
    let info = super::super::doctor_info(use_color);
    eprintln!();
    eprintln!("Background work:");
    let now = checkpoint::now_secs();
    let log = state_dir.join("detached-hook.log");
    for line in checkpoint_lines(&checkpoint::list(state_dir), now, &log) {
        print_check(line, &pass, &warn, &info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_run::checkpoint::{Checkpoint, JobKind};
    use std::path::PathBuf;

    fn ready(kind: JobKind, started_at: u64, attempts: u32) -> Entry {
        let mut cp = Checkpoint::new(kind, serde_json::json!({}), None);
        cp.started_at = started_at;
        cp.attempts = attempts;
        Entry::Ready(PathBuf::from("/s/checkpoints/x.json"), cp)
    }

    #[test]
    fn no_checkpoints_is_a_pass() {
        let lines = checkpoint_lines(&[], 100, Path::new("/s/log"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, CheckLevel::Pass);
        assert!(lines[0].1.contains("no unfinished"));
    }

    #[test]
    fn stale_is_a_warning_fresh_is_info_and_corrupt_is_a_warning() {
        let now = 100_000;
        let entries = [
            ready(JobKind::IcmStore, 1000, 3),
            ready(JobKind::IcmStore, now - 1, 1),
            Entry::Unreadable(PathBuf::from("/s/checkpoints/bad.json"), "parse".into()),
        ];
        let lines = checkpoint_lines(&entries, now, Path::new("/s/log"));
        assert_eq!(lines[0].0, CheckLevel::Warn);
        assert!(lines[0].1.contains("3/3 attempts"), "{}", lines[0].1);
        assert!(lines[0].1.contains("/s/log"), "{}", lines[0].1);
        assert_eq!(lines[1].0, CheckLevel::Info);
        assert_eq!(lines[2].0, CheckLevel::Warn);
        assert!(
            lines[2]
                .1
                .contains("unreadable checkpoint /s/checkpoints/bad.json")
        );
    }
}
