//! The journal of an `--other-session` audit note (#3762).
//!
//! A command that acts on a task of another session saves its change first, and then writes the
//! note in a second write. A crash between the two writes would leave a change with no record.
//! The guard writes the note to a journal file before the change. The command removes the file
//! once the note is written. A file whose process is gone is the trace of a crash, and the next
//! `llmenv task` command writes its note, marked as recovered.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    pid: u32,
    target: Target,
    text: String,
}

/// A journal file that exists until the note is written, or until the command ends without one.
/// A journal has no identity, so any two compare equal: the note that holds it is compared by
/// its text.
#[derive(Debug)]
pub(crate) struct Pending {
    path: PathBuf,
}

impl PartialEq for Pending {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for Pending {}

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

    /// Write the journal entry of the note.
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
        let name = format!("{pid}-{}.json", COUNTER.fetch_add(1, Ordering::Relaxed));
        let path = dir.join(name);
        let entry = Entry {
            pid,
            target,
            text: text.to_owned(),
        };
        write_new(&path, &serde_json::to_vec(&entry)?).with_context(|| {
            format!(
                "cannot write the note journal {}. Fix: make the folder writable, then run the \
                 command again",
                path.display()
            )
        })?;
        Ok(Self { path })
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
        self.finish();
    }
}

fn journal_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("pending-notes")
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// [`recover`] for the processes that the system reports as gone. A process that cannot be asked
/// about counts as alive, so its entry stays. Each result is a note that could not be written.
pub(crate) fn recover_crashed(state_dir: &Path) -> Vec<String> {
    recover(state_dir, |pid| {
        crate::mcp::proxy::is_alive(pid) == Some(false)
    })
}

/// Write the note of every journal entry whose process is gone, and remove the entry. The result
/// lists a note that could not be written, each with its text, so the caller can report it. A
/// live process keeps its entry: its command is still running.
///
/// `is_gone` answers whether a process id has no process.
pub(in crate::task) fn recover(state_dir: &Path, is_gone: impl Fn(u32) -> bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(journal_dir(state_dir)) else {
        return Vec::new();
    };
    let mut lost = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        // A file that does not parse was cut short by the crash, and the change never started.
        let Ok(entry) = serde_json::from_slice::<Entry>(&bytes) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if !is_gone(entry.pid) {
            continue;
        }
        let text = format!("{RECOVERED}{}", entry.text);
        let written = match &entry.target {
            Target::Task(slug) => super::note_task(state_dir, slug.as_str(), &text).map(drop),
            Target::Session(id) => super::session::append_override_note(state_dir, id, Some(&text)),
        };
        // The entry is removed in either case, so a note that cannot be written is reported once.
        let _ = std::fs::remove_file(&path);
        if let Err(e) = written {
            lost.push(format!(
                "an --other-session note from a crashed command could not be written: {e:#}. The \
                 note text was: {}",
                entry.text
            ));
        }
    }
    lost
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{ParentSpec, add_task_for_session, list_tasks};

    fn state_with_task() -> (tempfile::TempDir, String) {
        let dir = tempfile::TempDir::new().unwrap_or_else(|e| unreachable!("{e}"));
        let task = add_task_for_session(dir.path(), "Step", ParentSpec::Detached, "owner")
            .unwrap_or_else(|e| unreachable!("{e}"));
        (dir, task.slug)
    }

    fn notes_of(dir: &Path, slug: &str) -> Vec<String> {
        list_tasks(dir)
            .into_iter()
            .filter(|t| t.slug == slug)
            .flat_map(|t| t.notes.into_iter().map(|n| n.text))
            .collect()
    }

    #[test]
    fn a_crashed_command_leaves_its_note_for_the_next_command() {
        let (dir, slug) = state_with_task();
        let pending = Pending::for_task(dir.path(), &slug, "acted on").unwrap();
        // A crash runs no destructor, so the entry stays.
        std::mem::forget(pending);
        assert!(recover(dir.path(), |_| true).is_empty());
        let notes = notes_of(dir.path(), &slug);
        assert_eq!(notes, [format!("{RECOVERED}acted on")]);
        assert!(recover(dir.path(), |_| true).is_empty());
        assert_eq!(
            notes_of(dir.path(), &slug).len(),
            1,
            "an entry is used once"
        );
    }

    #[test]
    fn a_command_that_is_still_running_keeps_its_entry() {
        let (dir, slug) = state_with_task();
        let pending = Pending::for_task(dir.path(), &slug, "acted on").unwrap();
        assert!(recover(dir.path(), |_| false).is_empty());
        assert!(notes_of(dir.path(), &slug).is_empty());
        drop(pending);
        assert!(recover(dir.path(), |_| true).is_empty());
        assert!(
            notes_of(dir.path(), &slug).is_empty(),
            "a finished command leaves no entry"
        );
    }

    #[test]
    fn a_note_that_cannot_be_written_is_reported_with_its_text() {
        let (dir, _) = state_with_task();
        let pending = Pending::for_task(dir.path(), "gone", "text-1").unwrap();
        std::mem::forget(pending);
        let lost = recover(dir.path(), |_| true);
        assert_eq!(lost.len(), 1, "{lost:?}");
        assert!(lost[0].contains("text-1"), "{lost:?}");
        assert!(
            recover(dir.path(), |_| true).is_empty(),
            "a lost note is reported once"
        );
    }

    #[test]
    fn a_cut_short_entry_is_dropped() {
        let (dir, slug) = state_with_task();
        let folder = journal_dir(dir.path());
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("1-0.json"), b"{\"pid\":").unwrap();
        assert!(recover(dir.path(), |_| true).is_empty());
        assert!(notes_of(dir.path(), &slug).is_empty());
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
    }
}
