//! Checkpoints for detached background work (#2396).
//! Design: docs/design/issue-2396-detached-checkpoints.md
//!
//! A checkpoint is a file the parent writes before it spawns a detached job. The job deletes it on
//! success. A file that is still there after its job's deadline is unfinished work: the next
//! `SessionStart` runs the job again, up to [`MAX_ATTEMPTS`] times, and `llmenv doctor` lists what
//! is left. This module only reads and writes the files. The spawners stay where they are.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use super::session_ledger::hash_prefix;
use super::session_state::{prune_stale_json_files, unix_now};

/// Runs of one job before it stays for doctor. The first run counts as attempt 1.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// Checkpoints older than this are deleted, finished or not.
const PRUNE_DAYS: u64 = 7;
/// Above this, a payload is not checkpointed. The job still runs, as before.
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Seconds added to a job's longest timeout before its checkpoint counts as stale.
const DEADLINE_MARGIN_SECS: u64 = 60;

/// The detached jobs that keep a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobKind {
    Consolidation,
    IcmStore,
    SessionLogRecord,
    CbmIndex,
}

impl JobKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Consolidation => "consolidation",
            Self::IcmStore => "icm-store",
            Self::SessionLogRecord => "session-log-record",
            Self::CbmIndex => "cbm-index",
        }
    }

    /// Seconds a healthy job can run: its longest timeout plus a margin. The cbm indexer has no
    /// timeout, so it gets half an hour; the upstream benchmark for a very large repository is
    /// about three minutes.
    pub(crate) fn deadline_secs(self) -> u64 {
        let longest = match self {
            // The LLM call allows 120 s and each ICM call 30 s.
            Self::Consolidation => 120 + 30,
            Self::IcmStore | Self::SessionLogRecord => 5,
            Self::CbmIndex => 1800,
        };
        longest + DEADLINE_MARGIN_SECS
    }
}

/// Everything a job needs to run again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub(crate) kind: JobKind,
    pub(crate) key: String,
    /// Where the job stopped. Consolidation uses `started`, `recalled`, `summarized`; the other
    /// jobs are one call each and stay at `started`.
    pub(crate) phase: String,
    pub(crate) inputs: serde_json::Value,
    pub(crate) started_at: u64,
    pub(crate) attempts: u32,
    pub(crate) session_id: Option<String>,
}

impl Checkpoint {
    /// A new checkpoint for `kind` with `inputs`, at attempt 1. The key is a hash of the inputs,
    /// so the same job maps to the same file.
    pub(crate) fn new(kind: JobKind, inputs: serde_json::Value, session_id: Option<&str>) -> Self {
        Self {
            kind,
            key: key_for(kind, &inputs),
            phase: "started".to_string(),
            inputs,
            started_at: now_secs(),
            attempts: 1,
            session_id: session_id.map(str::to_string),
        }
    }

    /// Whether the job has run past its deadline, so its child is not still working.
    pub(crate) fn is_stale(&self, now: u64) -> bool {
        now.saturating_sub(self.started_at) > self.kind.deadline_secs()
    }

    fn file_name(&self) -> String {
        format!("{}-{}.json", self.kind.as_str(), self.key)
    }
}

/// One file in the checkpoint directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    Ready(PathBuf, Checkpoint),
    /// A file that cannot be read or parsed, with the reason. Doctor lists it.
    Unreadable(PathBuf, String),
}

pub(crate) fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("checkpoints")
}

pub(crate) fn now_secs() -> u64 {
    u64::try_from(unix_now()).unwrap_or(0)
}

/// The first 16 hex characters of the SHA-256 over the kind and the inputs.
#[must_use]
pub(crate) fn key_for(kind: JobKind, inputs: &serde_json::Value) -> String {
    hash_prefix(format!("{}\n{inputs}", kind.as_str()).as_bytes())
}

/// Write `checkpoint` and return its path. `None` when the inputs exceed [`MAX_INPUT_BYTES`]; the
/// job then runs without a checkpoint.
///
/// # Errors
/// The directory cannot be created or the file cannot be written.
pub(crate) fn write(state_dir: &Path, checkpoint: &Checkpoint) -> anyhow::Result<Option<PathBuf>> {
    if checkpoint.inputs.to_string().len() > MAX_INPUT_BYTES {
        tracing::debug!(
            "checkpoint for {} skipped: inputs above {MAX_INPUT_BYTES} bytes",
            checkpoint.kind.as_str()
        );
        return Ok(None);
    }
    let dir = dir(state_dir);
    crate::paths::create_dir_owner_only(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(checkpoint.file_name());
    save(&path, checkpoint)?;
    Ok(Some(path))
}

fn save(path: &Path, checkpoint: &Checkpoint) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(checkpoint)?;
    crate::paths::write_owner_only_atomic(path, &bytes)
        .with_context(|| format!("writing checkpoint {}", path.display()))
}

/// Read one checkpoint file.
///
/// # Errors
/// The file cannot be read or is not a checkpoint.
pub(crate) fn load(path: &Path) -> anyhow::Result<Checkpoint> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading checkpoint {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing checkpoint {}", path.display()))
}

/// Change a checkpoint in place, for a job that reached a new phase.
///
/// # Errors
/// The file cannot be read, parsed, or written.
pub(crate) fn update(path: &Path, change: impl FnOnce(&mut Checkpoint)) -> anyhow::Result<()> {
    let mut checkpoint = load(path)?;
    change(&mut checkpoint);
    save(path, &checkpoint)
}

/// Delete a checkpoint. A missing file is not an error: the job may have finished twice.
///
/// # Errors
/// The file exists and cannot be removed.
pub(crate) fn complete(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing checkpoint {}", path.display())),
    }
}

/// Every checkpoint under `state_dir`, oldest name first. A missing directory is empty.
pub(crate) fn list(state_dir: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir(state_dir)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| match load(&path) {
            Ok(checkpoint) => Entry::Ready(path, checkpoint),
            Err(e) => Entry::Unreadable(path, format!("{e:#}")),
        })
        .collect()
}

/// Run every stale checkpoint again: bump its attempt count, restart its clock, and hand it to
/// `spawn`. A checkpoint at [`MAX_ATTEMPTS`] stays for doctor, and a fresh one is left alone
/// because its child may still be running. Old files are pruned first. Returns the number of jobs
/// handed to `spawn`.
pub(crate) fn resume_pending(
    state_dir: &Path,
    now: u64,
    mut spawn: impl FnMut(&Checkpoint, &Path) -> anyhow::Result<()>,
) -> usize {
    prune_stale_json_files(&dir(state_dir), PRUNE_DAYS);
    let mut resumed = 0;
    for entry in list(state_dir) {
        let Entry::Ready(path, mut checkpoint) = entry else {
            continue;
        };
        if !checkpoint.is_stale(now) || checkpoint.attempts >= MAX_ATTEMPTS {
            continue;
        }
        checkpoint.attempts += 1;
        checkpoint.started_at = now;
        if let Err(e) = save(&path, &checkpoint) {
            tracing::debug!("checkpoint {} not resumed: {e:#}", path.display());
            continue;
        }
        match spawn(&checkpoint, &path) {
            Ok(()) => resumed += 1,
            Err(e) => tracing::debug!("checkpoint {} spawn failed: {e:#}", path.display()),
        }
    }
    resumed
}

/// The state dir the spawners write checkpoints into. Unit tests spawn real children, and those
/// must not write into the user's state dir, so a test build has none; the tests of the spawners
/// pass a temporary directory to the `_in` variants instead.
pub(crate) fn spawner_state_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::paths::state_dir().ok()
}

/// Write the checkpoint of a job that is about to spawn, and return its path. `None` when there
/// is no state dir, the inputs are too large, or the write failed: the job then runs without a
/// checkpoint, as it did before #2396.
pub(crate) fn begin(
    state_dir: Option<&Path>,
    kind: JobKind,
    inputs: serde_json::Value,
    session_id: Option<&str>,
) -> Option<PathBuf> {
    let checkpoint = Checkpoint::new(kind, inputs, session_id);
    write(state_dir?, &checkpoint)
        .inspect_err(|e| tracing::debug!("{} checkpoint not written: {e:#}", kind.as_str()))
        .ok()
        .flatten()
}

/// Delete the checkpoint of a job that finished, or that cannot succeed on a retry. A job that
/// failed in a way a later run may fix leaves its file. Fail-soft.
pub(crate) fn finish(path: Option<&Path>) {
    if let Some(path) = path
        && let Err(e) = complete(path)
    {
        tracing::error!("{e:#}");
    }
}

/// The kind, inputs, and session of each readable checkpoint, for the tests of the spawners.
#[cfg(test)]
pub(crate) fn checkpoint_entries_for_test(
    state_dir: &Path,
) -> Vec<(JobKind, serde_json::Value, Option<String>)> {
    list(state_dir)
        .into_iter()
        .filter_map(|e| match e {
            Entry::Ready(_, c) => Some((c.kind, c.inputs, c.session_id)),
            Entry::Unreadable(..) => None,
        })
        .collect()
}

/// Doctor's description of one checkpoint: kind, age, attempts, phase, and what to read.
#[must_use]
pub(crate) fn describe(checkpoint: &Checkpoint, now: u64, log: &Path) -> String {
    let age = now.saturating_sub(checkpoint.started_at);
    format!(
        "{} started {} ago, {}/{MAX_ATTEMPTS} attempts, phase {}; log: {}",
        checkpoint.kind.as_str(),
        format_age(age),
        checkpoint.attempts,
        checkpoint.phase,
        log.display()
    )
}

fn format_age(secs: u64) -> String {
    match secs {
        0..=119 => format!("{secs}s"),
        120..=7199 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cp(kind: JobKind, n: u32) -> Checkpoint {
        Checkpoint::new(kind, serde_json::json!({ "n": n }), Some("sess"))
    }

    #[test]
    fn key_is_stable_for_equal_inputs_and_differs_otherwise() {
        let a = serde_json::json!({ "x": 1 });
        assert_eq!(
            key_for(JobKind::IcmStore, &a),
            key_for(JobKind::IcmStore, &a)
        );
        assert_ne!(
            key_for(JobKind::IcmStore, &a),
            key_for(JobKind::SessionLogRecord, &a)
        );
        assert_ne!(
            key_for(JobKind::IcmStore, &a),
            key_for(JobKind::IcmStore, &serde_json::json!({ "x": 2 }))
        );
    }

    #[test]
    fn write_list_complete_round_trip_and_double_complete_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let checkpoint = cp(JobKind::IcmStore, 1);
        let path = write(dir.path(), &checkpoint).unwrap().unwrap();
        assert_eq!(
            list(dir.path()),
            vec![Entry::Ready(path.clone(), checkpoint)]
        );
        complete(&path).unwrap();
        complete(&path).unwrap();
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn the_same_job_maps_to_one_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &cp(JobKind::IcmStore, 1)).unwrap();
        write(dir.path(), &cp(JobKind::IcmStore, 1)).unwrap();
        assert_eq!(list(dir.path()).len(), 1);
    }

    #[test]
    fn inputs_above_the_cap_are_not_checkpointed() {
        let dir = tempfile::tempdir().unwrap();
        let big = Checkpoint::new(
            JobKind::IcmStore,
            serde_json::json!({ "blob": "x".repeat(MAX_INPUT_BYTES + 1) }),
            None,
        );
        assert_eq!(write(dir.path(), &big).unwrap(), None);
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn a_corrupt_file_is_listed_as_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(super::dir(dir.path())).unwrap();
        std::fs::write(super::dir(dir.path()).join("icm-store-bad.json"), "{ nope").unwrap();
        assert!(matches!(&list(dir.path())[..], [Entry::Unreadable(..)]));
    }

    #[test]
    fn update_changes_the_phase_and_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &cp(JobKind::Consolidation, 1))
            .unwrap()
            .unwrap();
        update(&path, |c| {
            c.phase = "summarized".into();
            c.inputs["summary"] = serde_json::json!("rules");
        })
        .unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.phase, "summarized");
        assert_eq!(loaded.inputs["summary"], "rules");
    }

    #[test]
    fn staleness_follows_the_deadline_of_the_kind() {
        let c = cp(JobKind::IcmStore, 1);
        let deadline = JobKind::IcmStore.deadline_secs();
        assert!(!c.is_stale(c.started_at + deadline));
        assert!(c.is_stale(c.started_at + deadline + 1));
        assert!(JobKind::CbmIndex.deadline_secs() > JobKind::Consolidation.deadline_secs());
    }

    #[test]
    fn resume_respawns_a_stale_checkpoint_once_with_its_inputs_and_bumps_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let mut stale = cp(JobKind::IcmStore, 1);
        stale.started_at = 1000;
        let path = write(dir.path(), &stale).unwrap().unwrap();
        let now = 1000 + JobKind::IcmStore.deadline_secs() + 1;
        let mut seen = Vec::new();
        let resumed = resume_pending(dir.path(), now, |c, p| {
            seen.push((c.inputs.clone(), c.attempts, p.to_path_buf()));
            Ok(())
        });
        assert_eq!(resumed, 1);
        assert_eq!(seen, vec![(stale.inputs.clone(), 2, path.clone())]);
        let stored = load(&path).unwrap();
        assert_eq!((stored.attempts, stored.started_at), (2, now));
        // The clock restarted, so an immediate second resume leaves it alone.
        assert_eq!(resume_pending(dir.path(), now + 1, |_, _| Ok(())), 0);
    }

    #[test]
    fn resume_skips_a_fresh_checkpoint_and_one_at_the_attempt_cap() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = cp(JobKind::IcmStore, 1);
        write(dir.path(), &fresh).unwrap();
        let mut capped = cp(JobKind::SessionLogRecord, 2);
        capped.started_at = 1;
        capped.attempts = MAX_ATTEMPTS;
        write(dir.path(), &capped).unwrap();
        let far_future = fresh.started_at + 100_000;
        let resumed = resume_pending(dir.path(), far_future, |c, _| {
            assert_eq!(
                c.kind,
                JobKind::IcmStore,
                "only the fresh-turned-stale one runs"
            );
            Ok(())
        });
        assert_eq!(resumed, 1, "the capped checkpoint must not run");
    }

    #[test]
    fn describe_names_kind_age_attempts_phase_and_log() {
        let mut c = cp(JobKind::Consolidation, 1);
        c.started_at = 100;
        c.attempts = 3;
        let text = describe(&c, 100 + 7300, Path::new("/s/detached-hook.log"));
        assert_eq!(
            text,
            "consolidation started 2h ago, 3/3 attempts, phase started; log: /s/detached-hook.log"
        );
    }

    proptest! {
        #[test]
        fn a_checkpoint_survives_serialization(
            n in 0u32..1000, attempts in 0u32..10, started in 0u64..u64::MAX / 2,
            phase in "[a-z]{1,10}", session in proptest::option::of("[a-z0-9-]{1,12}"),
        ) {
            let mut c = cp(JobKind::SessionLogRecord, n);
            c.attempts = attempts;
            c.started_at = started;
            c.phase = phase;
            c.session_id = session;
            let back: Checkpoint = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
            prop_assert_eq!(back, c);
        }

        #[test]
        fn attempts_never_decrease_through_resume(attempts in 1u32..6, age in 0u64..10_000) {
            let dir = tempfile::tempdir().unwrap();
            let mut c = cp(JobKind::IcmStore, 1);
            c.started_at = 1000;
            c.attempts = attempts;
            let path = write(dir.path(), &c).unwrap().unwrap();
            resume_pending(dir.path(), 1000 + age, |_, _| Ok(()));
            prop_assert!(load(&path).unwrap().attempts >= attempts);
        }
    }
}
