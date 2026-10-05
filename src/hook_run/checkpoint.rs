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
use super::session_state::{lock_state_file, prune_stale_json_files, unix_now};

/// Runs of one job before it stays for doctor. The first run counts as attempt 1.
const MAX_ATTEMPTS: u32 = 3;
/// Checkpoints older than this are deleted, finished or not.
const PRUNE_DAYS: u64 = 7;
/// Above this, a payload is not checkpointed. The job still runs, as before.
const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Seconds added to a job's longest timeout before its checkpoint counts as stale.
const DEADLINE_MARGIN_SECS: u64 = 60;
/// Jobs one `SessionStart` runs again. A long outage leaves one checkpoint per event, and an
/// unbounded resume would start one process for each. The rest wait for the next session.
const MAX_RESUMED_PER_START: usize = 20;

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
    fn as_str(self) -> &'static str {
        match self {
            Self::Consolidation => "consolidation",
            Self::IcmStore => "icm-store",
            Self::SessionLogRecord => "session-log-record",
            Self::CbmIndex => "cbm-index",
        }
    }

    /// Whether a stale checkpoint of this kind is run again from a later `SessionStart`. The index
    /// job is not: every `SessionStart` starts the indexer itself and rewrites its checkpoint.
    fn resumable(self) -> bool {
        self != Self::CbmIndex
    }

    /// Seconds a healthy job can run: its longest timeout plus a margin. The cbm indexer has no
    /// timeout, so it gets half an hour; the upstream benchmark for a very large repository is
    /// about three minutes.
    fn deadline_secs(self) -> u64 {
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
    key: String,
    /// Where the job stopped. Consolidation uses `started`, `recalled`, `summarized`; the other
    /// jobs are one call each and stay at `started`.
    pub(crate) phase: String,
    pub(crate) inputs: serde_json::Value,
    pub(crate) started_at: u64,
    pub(crate) attempts: u32,
    session_id: Option<String>,
    /// The working directory of the first run. The memory backend and the project come from it,
    /// so a resumed job starts there and never in the directory of the session that resumes it.
    #[serde(default)]
    pub(crate) cwd: Option<String>,
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
            cwd: std::env::current_dir()
                .ok()
                .map(|d| d.display().to_string()),
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

fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("checkpoints")
}

pub(crate) fn now_secs() -> u64 {
    u64::try_from(unix_now()).unwrap_or(0)
}

/// The first 16 hex characters of the SHA-256 over the kind and the inputs.
#[must_use]
fn key_for(kind: JobKind, inputs: &serde_json::Value) -> String {
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
///
/// A directory or an entry that cannot be read is an [`Entry::Unreadable`], so doctor reports it
/// instead of showing no work.
pub(crate) fn list(state_dir: &Path) -> Vec<Entry> {
    let directory = dir(state_dir);
    let read = match std::fs::read_dir(&directory) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            return vec![Entry::Unreadable(
                directory.clone(),
                format!(
                    "cannot read the checkpoint folder {}: {e}",
                    directory.display()
                ),
            )];
        }
    };
    let mut unreadable = Vec::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in read {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    paths.push(path);
                }
            }
            Err(e) => unreadable.push(Entry::Unreadable(
                directory.clone(),
                format!("cannot read an entry of the checkpoint folder: {e}"),
            )),
        }
    }
    paths.sort();
    let mut entries: Vec<Entry> = paths
        .into_iter()
        .map(|path| match load(&path) {
            Ok(checkpoint) => Entry::Ready(path, checkpoint),
            Err(e) => Entry::Unreadable(path, format!("{e:#}")),
        })
        .collect();
    entries.extend(unreadable);
    entries
}

/// Run every stale checkpoint again: bump its attempt count, restart its clock, and hand it to
/// `spawn`. A checkpoint at [`MAX_ATTEMPTS`] stays for doctor, a fresh one is left alone because
/// its child may still be running, and a kind that is not resumable is skipped. At most
/// [`MAX_RESUMED_PER_START`] jobs start. A lock keeps two sessions that start together from
/// running the same job twice. If `spawn` fails, the checkpoint goes back to its earlier state, so
/// a failure to start does not use up an attempt. Old files are pruned first. Returns the number
/// of jobs handed to `spawn`.
pub(crate) fn resume_pending(
    state_dir: &Path,
    now: u64,
    mut spawn: impl FnMut(&Checkpoint, &Path) -> anyhow::Result<()>,
) -> usize {
    prune_stale_json_files(&dir(state_dir), PRUNE_DAYS);
    let Some(_lock) = lock_state_file(&dir(state_dir), "resume", "checkpoint resume") else {
        return 0;
    };
    let mut resumed = 0;
    for entry in list(state_dir) {
        let Entry::Ready(path, checkpoint) = entry else {
            continue;
        };
        if !checkpoint.kind.resumable() || !checkpoint.is_stale(now) {
            continue;
        }
        if checkpoint.attempts >= MAX_ATTEMPTS {
            tracing::error!(
                "background job {} gave up after {MAX_ATTEMPTS} attempts; see `llmenv doctor`",
                path.display()
            );
            continue;
        }
        if resumed >= MAX_RESUMED_PER_START {
            tracing::error!(
                "more than {MAX_RESUMED_PER_START} unfinished background jobs; the rest wait for the next session"
            );
            break;
        }
        if restart(&path, &checkpoint, now, &mut spawn) {
            resumed += 1;
        }
    }
    resumed
}

/// Save the bumped checkpoint, start the job, and restore the old file when the start fails.
fn restart(
    path: &Path,
    checkpoint: &Checkpoint,
    now: u64,
    spawn: &mut impl FnMut(&Checkpoint, &Path) -> anyhow::Result<()>,
) -> bool {
    let mut bumped = checkpoint.clone();
    bumped.attempts += 1;
    bumped.started_at = now;
    if let Err(e) = save(path, &bumped) {
        tracing::error!("background job {} not resumed: {e:#}", path.display());
        return false;
    }
    if let Err(e) = spawn(&bumped, path) {
        tracing::error!("background job {} not resumed: {e:#}", path.display());
        if let Err(e) = save(path, checkpoint) {
            tracing::error!("{e:#}");
        }
        return false;
    }
    true
}

/// A value no two jobs share, for the part of a job's id that must tell two runs of the same
/// inputs apart: nanoseconds since the epoch plus a counter for the same nanosecond.
#[must_use]
pub(crate) fn run_tag() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Accept a `--checkpoint` argument only when it is a regular file, not a symlink, directly inside
/// `state_dir/checkpoints`, named like a checkpoint. A hidden subcommand that deletes and trusts
/// the file it is given must not act on an arbitrary path.
///
/// # Errors
/// The path is outside the checkpoint directory, is a symlink, or is not a checkpoint file name.
pub(crate) fn validated_path(state_dir: &Path, path: &Path) -> anyhow::Result<PathBuf> {
    let dir = dir(state_dir);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| n.ends_with(".json"))
        .ok_or_else(|| anyhow::anyhow!("--checkpoint {} is not a .json file", path.display()))?;
    anyhow::ensure!(
        path.parent() == Some(dir.as_path()),
        "--checkpoint {} is not inside {}",
        path.display(),
        dir.display()
    );
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("--checkpoint {} cannot be read", path.display()))?;
    anyhow::ensure!(
        meta.file_type().is_file(),
        "--checkpoint {name} is not a regular file"
    );
    Ok(path.to_path_buf())
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
/// checkpoint, as it did before #2396. A failed write is logged at error level, because it turns
/// the durability of the job off. The index job keeps counting its attempts across the checkpoints
/// that every session start rewrites, so an indexer that always fails shows as exhausted.
pub(crate) fn begin(
    state_dir: Option<&Path>,
    kind: JobKind,
    inputs: &impl Serialize,
    session_id: Option<&str>,
) -> Option<PathBuf> {
    let state_dir = state_dir?;
    let inputs = serde_json::to_value(inputs)
        .inspect_err(|e| tracing::error!("{} checkpoint not written: {e}", kind.as_str()))
        .ok()?;
    let mut checkpoint = Checkpoint::new(kind, inputs, session_id);
    if kind == JobKind::CbmIndex {
        let earlier_path = dir(state_dir).join(checkpoint.file_name());
        match load(&earlier_path) {
            Ok(earlier) => {
                checkpoint.attempts = earlier.attempts.saturating_add(1).min(MAX_ATTEMPTS);
            }
            Err(_) if !earlier_path.exists() => {}
            // A count that restarts at 1 hides an indexer that always fails.
            Err(e) => {
                tracing::error!("earlier index checkpoint unreadable, attempts restart: {e:#}")
            }
        }
    }
    write(state_dir, &checkpoint)
        .inspect_err(|e| tracing::error!("{} checkpoint not written: {e:#}", kind.as_str()))
        .ok()
        .flatten()
}

/// The inputs of a detached job. `stdin_text` is what the parent piped to the child. When that
/// text does not parse as `T`, for example because the parent failed to write all of it, the inputs
/// that the parent recorded in the checkpoint at `path` are used. The job then runs, and a
/// truncated pipe does not lose it.
///
/// # Errors
/// The text does not parse, and there is no readable checkpoint whose inputs parse.
pub(crate) fn inputs_or_checkpoint<T: serde::de::DeserializeOwned>(
    stdin_text: &str,
    path: Option<&Path>,
) -> anyhow::Result<T> {
    let stdin_error = match serde_json::from_str::<T>(stdin_text) {
        Ok(inputs) => return Ok(inputs),
        Err(e) => e,
    };
    let recorded = path
        .and_then(|p| load(p).ok())
        .and_then(|cp| serde_json::from_value::<T>(cp.inputs).ok());
    match recorded {
        Some(inputs) => {
            tracing::error!("job input on stdin is unusable ({stdin_error}); using the checkpoint");
            Ok(inputs)
        }
        None => Err(stdin_error.into()),
    }
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
    humantime::format_duration(std::time::Duration::from_secs(secs)).to_string()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::panic, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cp(kind: JobKind, n: u32) -> Checkpoint {
        Checkpoint::new(kind, serde_json::json!({ "n": n }), Some("sess"))
    }

    #[test]
    fn job_inputs_come_from_stdin_when_it_parses() {
        let got: serde_json::Value = inputs_or_checkpoint(r#"{"a":1}"#, None).unwrap();
        assert_eq!(got, serde_json::json!({"a": 1}));
    }

    #[test]
    fn a_truncated_stdin_falls_back_to_the_checkpoint_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint::new(JobKind::IcmStore, serde_json::json!({"a": 1}), Some("s"));
        let path = write(dir.path(), &cp).unwrap().unwrap();
        let got: serde_json::Value = inputs_or_checkpoint(r#"{"a":"#, Some(&path)).unwrap();
        assert_eq!(got, serde_json::json!({"a": 1}));
        let got: serde_json::Value = inputs_or_checkpoint("", Some(&path)).unwrap();
        assert_eq!(got, serde_json::json!({"a": 1}));
    }

    #[test]
    fn unusable_stdin_without_a_usable_checkpoint_is_an_error() {
        assert!(inputs_or_checkpoint::<serde_json::Value>("{", None).is_err());
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("x.json");
        std::fs::write(&bad, "not a checkpoint").unwrap();
        assert!(inputs_or_checkpoint::<serde_json::Value>("{", Some(&bad)).is_err());
        assert!(
            inputs_or_checkpoint::<serde_json::Value>("{", Some(&dir.path().join("none.json")))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_checkpoint_folder_that_cannot_be_read_is_listed_as_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("checkpoints");
        std::fs::create_dir(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o000)).unwrap();
        let entries = list(dir.path());
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
        if std::fs::read_dir(&folder).is_ok() && entries.is_empty() {
            return; // running as a user that ignores permissions
        }
        assert!(
            matches!(entries.as_slice(), [Entry::Unreadable(_, why)] if why.contains("cannot read"))
        );
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
    fn inputs_up_to_the_cap_are_checkpointed_and_one_byte_more_is_not() {
        let dir = tempfile::tempdir().unwrap();
        // The JSON text of a string is the string plus two quotes.
        let at_cap = serde_json::json!("x".repeat(MAX_INPUT_BYTES - 2));
        let over = serde_json::json!("x".repeat(MAX_INPUT_BYTES - 1));
        assert_eq!(MAX_INPUT_BYTES, 65_536);
        assert!(
            write(
                dir.path(),
                &Checkpoint::new(JobKind::IcmStore, at_cap, None)
            )
            .unwrap()
            .is_some()
        );
        assert!(
            write(dir.path(), &Checkpoint::new(JobKind::IcmStore, over, None))
                .unwrap()
                .is_none()
        );
        let modest = serde_json::json!({ "content": "y".repeat(10_000) });
        assert!(
            write(
                dir.path(),
                &Checkpoint::new(JobKind::SessionLogRecord, modest, None)
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn deadlines_are_the_longest_timeout_plus_the_margin() {
        assert_eq!(JobKind::Consolidation.deadline_secs(), 150 + 60);
        assert_eq!(JobKind::IcmStore.deadline_secs(), 5 + 60);
        assert_eq!(JobKind::SessionLogRecord.deadline_secs(), 5 + 60);
        assert_eq!(JobKind::CbmIndex.deadline_secs(), 1800 + 60);
    }

    #[test]
    fn now_secs_is_the_current_time() {
        assert!(now_secs() > 1_700_000_000, "{}", now_secs());
        assert!(now_secs() < 4_000_000_000);
    }

    #[test]
    fn completing_a_path_that_cannot_be_removed_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_file = dir.path().join("adir");
        std::fs::create_dir_all(not_a_file.join("inner")).unwrap();
        assert!(complete(&not_a_file).is_err());
        assert!(complete(&dir.path().join("missing.json")).is_ok());
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
    fn a_failed_start_restores_the_checkpoint_so_no_attempt_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        let mut stale = cp(JobKind::IcmStore, 1);
        stale.started_at = 1000;
        let path = write(dir.path(), &stale).unwrap().unwrap();
        let now = 1000 + JobKind::IcmStore.deadline_secs() + 1;
        let resumed = resume_pending(dir.path(), now, |_, _| anyhow::bail!("no such directory"));
        assert_eq!(resumed, 0);
        assert_eq!(load(&path).unwrap(), stale, "the file is as it was");
    }

    #[test]
    fn at_most_the_cap_of_jobs_start_and_the_rest_wait() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..(MAX_RESUMED_PER_START as u32 + 5) {
            let mut c = cp(JobKind::SessionLogRecord, n);
            c.started_at = 1;
            write(dir.path(), &c).unwrap();
        }
        let now = 1 + JobKind::SessionLogRecord.deadline_secs() + 1;
        let resumed = resume_pending(dir.path(), now, |_, _| Ok(()));
        assert_eq!(resumed, MAX_RESUMED_PER_START);
        let waiting = list(dir.path())
            .iter()
            .filter(|e| matches!(e, Entry::Ready(_, c) if c.attempts == 1))
            .count();
        assert_eq!(
            waiting, 5,
            "the rest keep their attempt count for the next session"
        );
    }

    #[test]
    fn a_kind_that_is_not_resumable_is_never_handed_to_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cp(JobKind::CbmIndex, 1);
        c.started_at = 1;
        let path = write(dir.path(), &c).unwrap().unwrap();
        assert_eq!(
            resume_pending(dir.path(), 1_000_000, |_, _| panic!("spawned")),
            0
        );
        assert_eq!(load(&path).unwrap().attempts, 1);
    }

    #[test]
    fn a_second_resume_in_progress_blocks_this_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut stale = cp(JobKind::IcmStore, 1);
        stale.started_at = 1;
        write(dir.path(), &stale).unwrap();
        let held = lock_state_file(&super::dir(dir.path()), "resume", "test").unwrap();
        assert_eq!(
            resume_pending(dir.path(), 1_000_000, |_, _| panic!(
                "spawned under a held lock"
            )),
            0
        );
        drop(held);
        assert_eq!(resume_pending(dir.path(), 1_000_000, |_, _| Ok(())), 1);
    }

    #[test]
    fn the_index_job_counts_attempts_across_the_checkpoints_each_session_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = serde_json::json!({ "project_root": "/r", "index_path": null });
        let path = begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();
        assert_eq!(load(&path).unwrap().attempts, 1);
        begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();
        begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();
        begin(Some(dir.path()), JobKind::CbmIndex, &inputs, None).unwrap();
        assert_eq!(
            load(&path).unwrap().attempts,
            MAX_ATTEMPTS,
            "capped at the limit"
        );
    }

    #[test]
    fn run_tags_differ_and_the_first_attempt_records_the_working_directory() {
        assert_ne!(run_tag(), run_tag());
        let c = cp(JobKind::IcmStore, 1);
        let cwd = std::env::current_dir().unwrap().display().to_string();
        assert_eq!(c.cwd.as_deref(), Some(cwd.as_str()));
    }

    #[test]
    fn only_a_regular_checkpoint_file_inside_the_checkpoint_dir_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let good = write(dir.path(), &cp(JobKind::IcmStore, 1))
            .unwrap()
            .unwrap();
        assert_eq!(validated_path(dir.path(), &good).unwrap(), good);
        let outside = dir.path().join("elsewhere.json");
        std::fs::write(&outside, "{}").unwrap();
        assert!(
            validated_path(dir.path(), &outside).is_err(),
            "outside the directory"
        );
        let not_json = super::dir(dir.path()).join("x.txt");
        std::fs::write(&not_json, "{}").unwrap();
        assert!(
            validated_path(dir.path(), &not_json).is_err(),
            "not a .json name"
        );
        let link = super::dir(dir.path()).join("icm-store-link.json");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(
            validated_path(dir.path(), &link).is_err(),
            "a symlink is refused"
        );
        let traversal = super::dir(dir.path()).join("../elsewhere.json");
        assert!(
            validated_path(dir.path(), &traversal).is_err(),
            "a .. path is refused"
        );
    }

    #[test]
    fn describe_names_kind_age_attempts_phase_and_log() {
        let mut c = cp(JobKind::Consolidation, 1);
        c.started_at = 100;
        c.attempts = 3;
        let text = describe(&c, 100 + 7300, Path::new("/s/detached-hook.log"));
        assert_eq!(
            text,
            "consolidation started 2h 1m 40s ago, 3/3 attempts, phase started; log: /s/detached-hook.log"
        );
    }

    proptest! {
        #[test]
        fn format_age_is_never_empty_and_ends_in_a_unit(secs in 0u64..10_000_000) {
            let text = format_age(secs);
            prop_assert!(!text.is_empty());
            prop_assert!(text.ends_with(['s', 'm', 'h', 'd']), "{text}");
        }

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
