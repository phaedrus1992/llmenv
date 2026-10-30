//! The outcome of recent adaptive recall calls, for `llmenv doctor` (#2919).
//!
//! Each hook adds its recall calls to a ring file under the state folder. Doctor reads that
//! file with no network call, so it can report a memory server that fails often or always.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Calls kept in the ring. With up to six calls a turn, this covers the last 30 or more turns,
/// which is enough for a rate and small enough to rewrite on every hook.
const RING_CAP: usize = 200;
/// Doctor warns when more than this percent of the kept calls failed.
pub(crate) const WARN_FAILED_PERCENT: usize = 20;
/// Doctor reports the server as stuck when this many of the newest calls all failed.
pub(crate) const STUCK_RUN: usize = 10;
const FILE_KEY: &str = "recall_health";

/// One recall call: its Unix time in seconds, and whether it answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Call {
    at: i64,
    ok: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ring {
    calls: VecDeque<Call>,
}

/// What doctor reports for the kept calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Health {
    pub(crate) calls: usize,
    pub(crate) failed: usize,
    /// The Unix time in seconds of the newest call.
    pub(crate) newest_at: i64,
    /// The newest [`STUCK_RUN`] calls all failed.
    pub(crate) stuck: bool,
}

impl Health {
    /// More than [`WARN_FAILED_PERCENT`] of the kept calls failed.
    pub(crate) fn failing_often(&self) -> bool {
        self.failed.saturating_mul(100) > self.calls.saturating_mul(WARN_FAILED_PERCENT)
    }
}

/// The ring file under `state_dir`.
pub(crate) fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(format!("{FILE_KEY}.json"))
}

/// Adds `ok` answered calls and `failed` failed calls at `now` to the ring under `state_dir`.
/// The failures go first, so a hook in which any call answered does not end the ring with a
/// failure. A lock, read, or write that fails is logged and told to the agent, and the calls
/// are not kept.
pub(crate) fn record(state_dir: &Path, ok: usize, failed: usize, now: i64) {
    if ok == 0 && failed == 0 {
        return;
    }
    let path = path(state_dir);
    let _lock =
        match super::session_state::lock_state_file_why(state_dir, FILE_KEY, "recall health") {
            Ok(lock) => lock,
            Err(why) => {
                note_unsaved(&path, &why);
                return;
            }
        };
    let result = read_ring(&path).and_then(|mut ring| {
        let new = std::iter::repeat_n(false, failed).chain(std::iter::repeat_n(true, ok));
        ring.calls.extend(new.map(|ok| Call { at: now, ok }));
        let excess = ring.calls.len().saturating_sub(RING_CAP);
        ring.calls.drain(..excess);
        let bytes = serde_json::to_vec(&ring).context("encoding the ring")?;
        crate::paths::write_owner_only_atomic_unsynced(&path, &bytes)
            .with_context(|| format!("writing {}", path.display()))
    });
    if let Err(e) = result {
        note_unsaved(&path, &format!("{e:#}"));
    }
}

/// The health of the calls kept under `state_dir`, or `None` when no hook kept a call yet.
///
/// # Errors
/// The file exists but cannot be read, or it is not a valid ring.
pub(crate) fn health(state_dir: &Path) -> anyhow::Result<Option<Health>> {
    let path = path(state_dir);
    let Some(text) = read_text(&path).with_context(|| format!("reading {}", path.display()))?
    else {
        return Ok(None);
    };
    let ring: Ring = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a valid recall health file", path.display()))?;
    Ok(assess(&ring.calls))
}

/// The text of the ring file, or `None` when there is no file.
fn read_text(path: &Path) -> std::io::Result<Option<String>> {
    match crate::paths::bounded::state_text_io(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The stored ring. A missing file is an empty ring. A file that is not a valid ring is
/// replaced and the agent is told, because the rate of new calls is worth more than a ring
/// that nobody can read. `InvalidData` covers bytes that are not UTF-8 and an oversized file.
fn read_ring(path: &Path) -> anyhow::Result<Ring> {
    let text = match read_text(path) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(Ring::default()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            note_reset(path, &e.to_string());
            return Ok(Ring::default());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(serde_json::from_str(&text).unwrap_or_else(|e| {
        note_reset(path, &e.to_string());
        Ring::default()
    }))
}

/// The health of `calls`, oldest first, or `None` for an empty ring.
fn assess(calls: &VecDeque<Call>) -> Option<Health> {
    let newest_at = calls.back()?.at;
    let failed = calls.iter().filter(|c| !c.ok).count();
    let newest_failed = calls.iter().rev().take_while(|c| !c.ok).count();
    Some(Health {
        calls: calls.len(),
        failed,
        newest_at,
        stuck: newest_failed >= STUCK_RUN,
    })
}

fn note_reset(path: &Path, why: &str) {
    crate::agent_notice::record_logged(format!(
        "llmenv: the recall health file {} was not a valid ring ({why}), so a new one was \
         started. `llmenv doctor` now counts only the calls after this point.",
        path.display()
    ));
}

fn note_unsaved(path: &Path, why: &str) {
    crate::agent_notice::record_logged(format!(
        "llmenv: the recall health file {} was not updated ({why}), so `llmenv doctor` can show \
         an old failure rate. Fix: run `llmenv doctor` to check the state directory.",
        path.display()
    ));
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn no_file_is_no_health() {
        let dir = dir();
        let log = dir.path();
        assert_eq!(health(log).unwrap(), None);
    }

    #[test]
    fn a_recorded_hook_is_counted() {
        let dir = dir();
        let log = dir.path();
        record(log, 4, 1, 100);
        let h = health(log).unwrap().unwrap();
        assert_eq!((h.calls, h.failed), (5, 1));
        assert!(!h.stuck);
        assert!(!h.failing_often(), "1 of 5 is 20%, not more");
        record(log, 0, 0, 200);
        assert_eq!(health(log).unwrap().unwrap().calls, 5, "no call, no entry");
    }

    #[test]
    fn the_ring_keeps_the_newest_calls() {
        let dir = dir();
        let log = dir.path();
        record(log, RING_CAP, 0, 1);
        record(log, 0, STUCK_RUN, 2);
        let h = health(log).unwrap().unwrap();
        assert_eq!((h.calls, h.failed), (RING_CAP, STUCK_RUN));
        assert!(h.stuck);
    }

    #[test]
    fn an_answer_in_the_newest_hook_is_not_stuck() {
        let dir = dir();
        let log = dir.path();
        record(log, 1, STUCK_RUN, 1);
        assert!(!health(log).unwrap().unwrap().stuck);
    }

    #[test]
    fn a_corrupt_file_is_an_error_for_doctor_and_replaced_by_a_hook() {
        let dir = dir();
        let log = dir.path();
        std::fs::write(path(log), "not json").unwrap();
        let err = health(log).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a valid recall health file"),
            "{err:#}"
        );
        record(log, 1, 0, 5);
        assert_eq!(health(log).unwrap().unwrap().calls, 1);
    }

    // The ring is saved without an fsync, so a power loss can leave an empty or cut-off file.
    // Every strict prefix of a saved ring must be replaced by the next hook, not block it.
    #[test]
    fn an_empty_or_cut_off_file_is_replaced_by_the_next_hook() {
        let dir = dir();
        let log = dir.path();
        record(log, 2, 1, 3);
        let saved = std::fs::read_to_string(path(log)).unwrap();
        for end in 0..saved.len() {
            std::fs::write(path(log), &saved.as_bytes()[..end]).unwrap();
            record(log, 1, 0, 9);
            assert_eq!(health(log).unwrap().unwrap().calls, 1, "cut at {end}");
        }
    }

    #[test]
    fn an_unwritable_folder_tells_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-folder");
        std::fs::write(&file, "").unwrap();
        record(&file, 1, 0, 1);
        let notice = crate::agent_notice::current().unwrap_or_default();
        assert!(notice.contains("recall health file"), "{notice}");
    }

    #[test]
    fn a_file_that_is_not_utf8_is_replaced_and_the_agent_is_told() {
        let dir = dir();
        let log = dir.path();
        std::fs::write(path(log), [0xff, 0xfe]).unwrap();
        assert!(health(log).is_err(), "doctor reports the bad file");
        record(log, 1, 0, 7);
        let h = health(log).unwrap().unwrap();
        assert_eq!((h.calls, h.newest_at), (1, 7));
        let notice = crate::agent_notice::current().unwrap_or_default();
        assert!(notice.contains("was not a valid ring"), "{notice}");
    }

    #[test]
    fn the_newest_time_is_the_last_hook() {
        let dir = dir();
        let log = dir.path();
        record(log, 1, 0, 10);
        record(log, 0, 1, 20);
        assert_eq!(health(log).unwrap().unwrap().newest_at, 20);
    }

    /// A file that exists but cannot be opened is not a bad ring: it is kept, and the agent
    /// hears that the update failed.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_kept_and_the_update_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir();
        let log = dir.path();
        record(log, 3, 0, 1);
        let file = path(log);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&file).is_ok() {
            // A root user reads the file anyway, so the case cannot happen here.
            return;
        }
        record(log, 1, 0, 2);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            health(log).unwrap().unwrap().calls,
            3,
            "the file was not replaced"
        );
        let notice = crate::agent_notice::current().unwrap_or_default();
        assert!(notice.contains("was not updated"), "{notice}");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// Recording hooks one by one keeps the newest calls of a plain model: failures first
        /// within a hook, capped at the ring size, with the stuck flag from its tail.
        #[test]
        fn record_then_health_matches_a_model(
            hooks in prop::collection::vec((0usize..30, 0usize..30), 1..20),
        ) {
            let dir = dir();
            let mut model: Vec<bool> = Vec::new();
            for (i, &(ok, failed)) in hooks.iter().enumerate() {
                record(dir.path(), ok, failed, i64::try_from(i).unwrap());
                model.extend(std::iter::repeat_n(false, failed));
                model.extend(std::iter::repeat_n(true, ok));
            }
            let kept = &model[model.len().saturating_sub(200)..];
            let got = health(dir.path()).unwrap();
            if kept.is_empty() {
                prop_assert!(got.is_none());
            } else {
                let h = got.unwrap();
                prop_assert_eq!(h.calls, kept.len());
                prop_assert_eq!(h.failed, kept.iter().filter(|ok| !**ok).count());
                let tail = kept.iter().rev().take_while(|ok| !**ok).count();
                prop_assert_eq!(h.stuck, tail >= 10);
            }
        }

        /// The warning and the stuck flag follow their definitions for any ring.
        #[test]
        fn assess_matches_the_definitions(oks in prop::collection::vec(any::<bool>(), 0..60)) {
            let calls: VecDeque<Call> = oks.iter().map(|&ok| Call { at: 1, ok }).collect();
            let Some(h) = assess(&calls) else {
                prop_assert!(oks.is_empty());
                return Ok(());
            };
            let failed = oks.iter().filter(|ok| !**ok).count();
            prop_assert_eq!(h.failed, failed);
            prop_assert_eq!(h.failing_often(), failed * 5 > oks.len());
            let tail_failed = oks.len() >= STUCK_RUN
                && oks[oks.len() - STUCK_RUN..].iter().all(|ok| !ok);
            prop_assert_eq!(h.stuck, tail_failed);
        }
    }
}
