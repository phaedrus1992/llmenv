//! The claim of a journal entry that a command replays (#3783).
//!
//! A command claims an entry by renaming it to `<name>.claim-<pid>` before it writes the note.
//! A command that dies between the rename and the write leaves the claim. Another command puts
//! the claim back under the entry name once its process is gone, so the note is replayed.

use std::path::{Path, PathBuf};

/// The name that a command with process id `pid` gives to the claim of `entry`.
pub(super) fn claim_path(entry: &Path, pid: u32) -> PathBuf {
    entry.with_extension(format!("claim-{pid}"))
}

/// The process id in the extension of a claim file, or `None` for another file.
fn claim_owner(path: &Path) -> Option<u32> {
    path.extension()?
        .to_str()?
        .strip_prefix("claim-")?
        .parse()
        .ok()
}

/// Put each claim whose process is gone back under its entry name. Two commands that restore the
/// same claim at once are safe: the second rename finds no file.
pub(super) fn restore_abandoned(folder: &Path, is_gone: impl Fn(u32) -> bool) {
    let Ok(read) = std::fs::read_dir(folder) else {
        return;
    };
    for path in read.flatten().map(|item| item.path()) {
        let Some(owner) = claim_owner(&path) else {
            continue;
        };
        if !is_gone(owner) {
            continue;
        }
        let entry = path.with_extension("json");
        if std::fs::rename(&path, &entry).is_ok() {
            tracing::warn!(
                path = %entry.display(),
                "a note journal entry was claimed by a command that is gone; replaying it"
            );
        }
    }
}
