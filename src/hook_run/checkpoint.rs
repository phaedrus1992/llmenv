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
/// The largest checkpoint file that a load reads: the input cap plus room for the other fields.
/// A larger file is not a checkpoint that llmenv wrote.
const MAX_CHECKPOINT_BYTES: u64 = 2 * MAX_INPUT_BYTES as u64;
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
                .inspect_err(|e| {
                    tracing::error!("{} checkpoint has no working directory: {e}", kind.as_str());
                })
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
    let text = llmenv_paths::read_text_bounded(path, MAX_CHECKPOINT_BYTES)
        .and_then(|text| text.ok_or_else(|| std::io::ErrorKind::NotFound.into()))
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
/// its child may still be running, a kind that is not resumable is skipped, and one that can never
/// run again is deleted with a notice. At most
/// [`MAX_RESUMED_PER_START`] jobs start. A lock keeps two sessions that start together from
/// running the same job twice. A failure to start uses up an attempt. Old files are pruned first. Returns the number
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
        if let Some(reason) = unrunnable_reason(&checkpoint) {
            drop_unrunnable(&path, &checkpoint, &reason);
            continue;
        }
        if checkpoint.attempts >= MAX_ATTEMPTS {
            crate::agent_notice::record_logged(format!(
                "llmenv: background job {} gave up after {MAX_ATTEMPTS} attempts. Fix: run `llmenv doctor`, then start the job again.",
                path.display()
            ));
            continue;
        }
        if resumed >= MAX_RESUMED_PER_START {
            crate::agent_notice::record_logged(format!(
                "llmenv: more than {MAX_RESUMED_PER_START} unfinished background jobs, so the rest wait for the next session. Fix: finish or remove the older jobs with `llmenv doctor`."
            ));
            break;
        }
        if restart(&path, &checkpoint, now, &mut spawn) {
            resumed += 1;
        }
    }
    resumed
}

/// Save the bumped checkpoint and start the job. A failed start keeps the bumped count: a restore
/// would also reset the age of the file, so a start that always fails would run at every session
/// start, never reach [`MAX_ATTEMPTS`], and never be pruned (#2912, #2720).
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
        crate::agent_notice::record_logged(format!(
            "llmenv: background job {} not resumed, its checkpoint cannot be saved ({e:#}). Fix: check the permissions of the checkpoint folder.",
            path.display()
        ));
        return false;
    }
    if let Err(e) = spawn(&bumped, path) {
        crate::agent_notice::record_logged(format!(
            "llmenv: background job {} not resumed (attempt {}/{MAX_ATTEMPTS}): {e:#}. Fix: fix the cause in the error, then start the job again.",
            path.display(),
            bumped.attempts
        ));
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
    crate::paths::state_dir()
        .inspect_err(|e| {
            crate::agent_notice::no_state_folder("a detached job runs with no checkpoint", e);
        })
        .ok()
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
        .inspect_err(|e| crate::agent_notice::record_logged(format!(
            "llmenv: the {} checkpoint is not written ({e}), so the job runs without resume. Fix: check the permissions of the checkpoint folder.",
            kind.as_str()
        )))
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
            Err(e) => crate::agent_notice::record_logged(format!(
                "llmenv: the earlier code index checkpoint {} cannot be read, so its attempt count \
                 restarts at 1: {e:#}. Fix: remove the file.",
                earlier_path.display()
            )),
        }
    }
    write(state_dir, &checkpoint)
        .inspect_err(|e| crate::agent_notice::record_logged(format!(
            "llmenv: the {} checkpoint is not written ({e:#}), so the job runs without resume. Fix: check the permissions of the checkpoint folder.",
            kind.as_str()
        )))
        .ok()
        .flatten()
}

mod describe;
pub(crate) use describe::describe;
#[cfg(test)]
use describe::format_age;
mod inputs;
mod unrunnable;
pub(crate) use inputs::{InputsError, inputs_or_checkpoint};
use unrunnable::{drop_unrunnable, unrunnable_reason};

/// Delete the checkpoint of a job that finished, or that cannot succeed on a retry. A job that
/// failed in a way a later run may fix leaves its file. Fail-soft.
///
/// A checkpoint that stays makes the next session run the finished job again, and later report
/// that it gave up, so a failed removal is a notice (#3465).
pub(crate) fn finish(path: Option<&Path>) {
    if let Some(path) = path
        && let Err(e) = complete(path)
    {
        crate::agent_notice::record_logged(format!(
            "llmenv: a background job finished, but its checkpoint stays, so a later session can \
             run it again: {e:#}. Fix: remove {}.",
            path.display()
        ));
    }
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::checkpoint_entries_for_test;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::panic, reason = "test code")]
mod resume_tests;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod notice_tests;
