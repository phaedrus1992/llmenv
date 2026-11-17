//! The journal of an `--other-session` audit note (#3762).
//!
//! A command that acts on a task of another session saves its change first, and then writes the
//! note in a second write. A crash between the two writes would leave a change with no record.
//! The guard writes the note to a journal file before the change. The command removes the file
//! once the note is written. A file whose process is gone is the trace of a crash, and the next
//! `llmenv task` command writes its note, marked as recovered.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use super::TaskTarget;

/// The prefix of a recovered note. The change may not have saved before the crash, so the note
/// says so.
const RECOVERED: &str = "recovered after a crash, the change may not have completed: ";

/// What a journal entry holds the note for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Target {
    /// The slug of a task.
    Task(String),
    /// The id of a session.
    Session(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    target: Target,
    text: String,
}

/// A journal file that exists until the note is written, or until the command ends without one.
/// The command holds an exclusive lock on the file while it lives. The system releases the lock
/// when the process dies, so a file that can be locked belongs to a command that is gone. This
/// needs no process id, and a reused id cannot fool it.
#[derive(Debug)]
pub(crate) struct Pending {
    path: PathBuf,
    /// The open file that holds the lock. It closes when the value drops.
    _lock: Option<File>,
}

impl Pending {
    /// Journal the note of a task. Run it before the change.
    ///
    /// # Errors
    /// See [`Pending::begin`].
    pub(crate) fn for_task(state_dir: &Path, slug: &str, text: &str) -> anyhow::Result<Self> {
        Self::begin(state_dir, Target::Task(slug.to_owned()), text)
    }

    /// Journal the note of a session. Run it before the change.
    ///
    /// # Errors
    /// See [`Pending::begin`].
    pub(crate) fn for_session(state_dir: &Path, id: &str, text: &str) -> anyhow::Result<Self> {
        Self::begin(state_dir, Target::Session(id.to_owned()), text)
    }

    /// Write the journal entry of the note. The entry is written under a hidden name and then
    /// renamed, so a reader never sees a half-written entry.
    ///
    /// # Errors
    /// The journal folder or file cannot be written. The command must stop, because its change
    /// would have no safe record.
    fn begin(state_dir: &Path, target: Target, text: &str) -> anyhow::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = journal_dir(state_dir);
        crate::paths::create_dir_owner_only(&dir)
            .with_context(|| format!("cannot make the note journal folder {}", dir.display()))?;
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!(
            "{pid}-{nanos:x}-{}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(format!("{name}.json"));
        let hidden = dir.join(format!(".{name}.tmp"));
        let entry = Entry {
            target,
            text: text.to_owned(),
        };
        let bytes = serde_json::to_vec(&entry).context("cannot encode the note journal entry")?;
        let written = write_locked(&hidden, &bytes).and_then(|file| {
            std::fs::rename(&hidden, &path)?;
            Ok(file)
        });
        let file = match written {
            Ok(file) => file,
            Err(e) => {
                let _ = std::fs::remove_file(&hidden);
                return Err(e).with_context(|| {
                    format!(
                        "cannot write the note journal {}. Fix: make the folder writable, then \
                         run the command again",
                        path.display()
                    )
                });
            }
        };
        Ok(Self {
            path,
            _lock: Some(file),
        })
    }

    /// Remove the journal file. The note is written, or the command ended without it. Calling it
    /// again does nothing.
    pub(crate) fn finish(&self) {
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), "cannot remove a note journal: {e}");
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // A panic after the change leaves the entry, so the next command writes the note.
        if std::thread::panicking() {
            return;
        }
        self.finish();
    }
}

fn journal_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("pending-notes")
}

/// Write a new file and keep it locked. The lock is taken before the entry has its final name,
/// so a reader never sees an unlocked entry of a live command.
fn write_locked(path: &Path, bytes: &[u8]) -> std::io::Result<File> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.try_lock().map_err(std::io::Error::other)?;
    file.write_all(bytes)?;
    Ok(file)
}

/// The entries of the journal folder, in name order. A hidden file and a claimed file are not
/// entries. A folder that is missing holds none.
fn entry_paths(state_dir: &Path) -> Vec<PathBuf> {
    let dir = journal_dir(state_dir);
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(path = %dir.display(), "cannot read the note journal folder: {e}");
            return Vec::new();
        }
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(|item| match item {
            Ok(item) => Some(item.path()),
            Err(e) => {
                tracing::warn!(path = %dir.display(), "cannot read a note journal entry: {e}");
                None
            }
        })
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".json") && !n.starts_with('.'))
        })
        .collect();
    paths.sort();
    paths
}

/// Read and parse one entry. A file that is not a plain file, or that cannot be read, stays where
/// it is, with a warning. A file that does not parse moves aside as `.corrupt`.
fn read_entry(path: &Path) -> Option<Entry> {
    let plain = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
    if !plain {
        tracing::warn!(path = %path.display(), "a note journal entry is not a plain file; skipped");
        return None;
    }
    let bytes = match crate::paths::bounded::state_bytes_io(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(path = %path.display(), "cannot read a note journal entry: {e}");
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(entry) => Some(entry),
        Err(e) => {
            let aside = path.with_extension("corrupt");
            tracing::warn!(
                path = %path.display(),
                "a note journal entry does not parse ({e}); moved to {}",
                aside.display()
            );
            let _ = std::fs::rename(path, aside);
            None
        }
    }
}

/// Lock an entry for a replay. `None` when a live command holds it, when another command replays
/// it, or when the entry is gone, which are all reasons to skip it. A lock on a file that was
/// removed after the open is no lock, so the path must still name the locked file.
fn lock_entry(path: &Path) -> Option<File> {
    // A pipe or a link named like an entry must not block the open or be followed. The open does
    // not wait for a writer, and `read_entry` checks the type again.
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
        tracing::warn!(path = %path.display(), "a note journal entry is not a plain file; skipped");
        return None;
    }
    let file = open_entry(path).ok()?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return None,
        Err(std::fs::TryLockError::Error(e)) => {
            tracing::warn!(path = %path.display(), "cannot lock a note journal entry: {e}");
            return None;
        }
    }
    let still_there = match (std::fs::metadata(path), file.metadata()) {
        (Ok(named), Ok(opened)) => same_file(&named, &opened),
        _ => false,
    };
    still_there.then_some(file)
}

#[cfg(unix)]
fn open_entry(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits().cast_signed())
        .open(path)
}

#[cfg(not(unix))]
fn open_entry(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    true
}

/// Write the note of every journal entry whose command is gone, and remove the entry. A note that
/// cannot be written is kept as `.failed` beside the journal, and the result names it with the
/// text. An entry that a live command holds stays: that command is still running.
pub(crate) fn recover(state_dir: &Path) -> Vec<String> {
    let mut lost = Vec::new();
    for path in entry_paths(state_dir) {
        // The lock is held to the end of the replay, so two commands replay one entry once, and a
        // command that dies in the replay leaves the entry for the next command.
        let Some(_lock) = lock_entry(&path) else {
            continue;
        };
        let Some(entry) = read_entry(&path) else {
            continue;
        };
        let text = format!("{RECOVERED}{}", entry.text);
        let written = match &entry.target {
            Target::Task(slug) => {
                super::note_task(state_dir, TaskTarget::Resolved(slug), &text).map(drop)
            }
            Target::Session(id) => super::session::append_override_note(state_dir, id, Some(&text)),
        };
        let kept = path.with_extension("failed");
        let ended = match written {
            Ok(()) => std::fs::remove_file(&path).map_err(|e| {
                format!(
                    "the entry {} could not be removed ({e}), so the note may be written again",
                    path.display()
                )
            }),
            Err(e) => Err(match std::fs::rename(&path, &kept) {
                Ok(()) => format!("{e:#}. The entry is kept as {}", kept.display()),
                Err(rename) => format!("{e:#}. The entry stays at {} ({rename})", path.display()),
            }),
        };
        if let Err(why) = ended {
            lost.push(format!(
                "an --other-session note from a crashed command: {why}. The note text was: {}",
                entry.text
            ));
        }
    }
    lost
}

#[cfg(test)]
impl Pending {
    /// Simulate a crash: the lock is released and the entry stays.
    pub(in crate::task) fn crash(self) {
        let mut me = std::mem::ManuallyDrop::new(self);
        drop(me._lock.take());
    }
}

#[cfg(test)]
mod tests;
