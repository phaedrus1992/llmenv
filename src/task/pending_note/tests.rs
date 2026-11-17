#![expect(clippy::unwrap_used, clippy::panic, reason = "test scaffolding")]
use super::*;
use crate::task::{ParentSpec, add_task_for_session, list_tasks};
use proptest::prelude::*;

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

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(journal_dir(dir))
        .map(|r| {
            r.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
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
    assert!(
        files_in(dir.path()).is_empty(),
        "{:?}",
        files_in(dir.path())
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
fn a_panic_leaves_the_entry_for_recovery() {
    let (dir, slug) = state_with_task();
    let path = dir.path().to_path_buf();
    let task = slug.clone();
    let result = std::panic::catch_unwind(move || {
        let _pending = Pending::for_task(&path, &task, "acted on").unwrap();
        std::panic::panic_any("a failure after the change");
    });
    assert!(result.is_err());
    assert_eq!(files_in(dir.path()).len(), 1, "{:?}", files_in(dir.path()));
    assert!(recover(dir.path(), |_| true).is_empty());
    assert_eq!(notes_of(dir.path(), &slug).len(), 1);
}

#[test]
fn a_note_that_cannot_be_written_is_kept_and_reported_with_its_text() {
    let (dir, _) = state_with_task();
    std::mem::forget(Pending::for_task(dir.path(), "gone", "text-1").unwrap());
    let lost = recover(dir.path(), |_| true);
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(
        lost[0].contains("text-1") && lost[0].contains(".failed"),
        "{lost:?}"
    );
    assert!(
        recover(dir.path(), |_| true).is_empty(),
        "a lost note is reported once"
    );
    let files = files_in(dir.path());
    assert!(files.iter().any(|f| f.ends_with(".failed")), "{files:?}");
}

#[test]
fn a_slug_in_an_entry_is_not_matched_by_prefix() {
    let (dir, slug) = state_with_task();
    let prefix = &slug[..slug.len() - 1];
    std::mem::forget(Pending::for_task(dir.path(), prefix, "text-2").unwrap());
    let lost = recover(dir.path(), |_| true);
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(notes_of(dir.path(), &slug).is_empty());
}

#[test]
fn a_cut_short_entry_moves_aside_and_is_not_deleted() {
    let (dir, slug) = state_with_task();
    let folder = journal_dir(dir.path());
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("1-0.json"), b"{\"pid\":").unwrap();
    assert!(recover(dir.path(), |_| true).is_empty());
    assert!(notes_of(dir.path(), &slug).is_empty());
    assert_eq!(files_in(dir.path()), ["1-0.corrupt"]);
}

#[cfg(unix)]
#[test]
fn a_link_in_the_journal_is_not_followed() {
    let (dir, slug) = state_with_task();
    let secret = dir.path().join("secret.json");
    let entry = Entry {
        pid: 1,
        target: Target::Task(slug.clone()),
        text: "stolen".into(),
    };
    std::fs::write(&secret, serde_json::to_vec(&entry).unwrap()).unwrap();
    let folder = journal_dir(dir.path());
    std::fs::create_dir_all(&folder).unwrap();
    std::os::unix::fs::symlink(&secret, folder.join("1-0.json")).unwrap();
    assert!(recover(dir.path(), |_| true).is_empty());
    assert!(notes_of(dir.path(), &slug).is_empty());
}

#[test]
fn this_process_is_not_gone() {
    assert!(!process_is_gone(std::process::id()));
}

proptest! {
    #[test]
    fn an_entry_survives_a_round_trip(
        pid in any::<u32>(),
        id in ".{0,40}",
        text in ".{0,200}",
        session in any::<bool>(),
    ) {
        let target = if session { Target::Session(id) } else { Target::Task(id) };
        let entry = Entry { pid, target, text };
        let bytes = serde_json::to_vec(&entry).unwrap();
        prop_assert_eq!(serde_json::from_slice::<Entry>(&bytes).unwrap(), entry);
    }

    #[test]
    fn recovery_keeps_the_entries_of_live_processes(
        pids in proptest::collection::vec(1u32..6, 0..6),
        garbage in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..40), 0..3),
    ) {
        let dir = tempfile::TempDir::new().unwrap();
        let folder = journal_dir(dir.path());
        std::fs::create_dir_all(&folder).unwrap();
        for (n, pid) in pids.iter().enumerate() {
            let entry = Entry { pid: *pid, target: Target::Session("none".into()), text: "t".into() };
            std::fs::write(folder.join(format!("{pid}-{n}.json")), serde_json::to_vec(&entry).unwrap()).unwrap();
        }
        for (n, bytes) in garbage.iter().enumerate() {
            std::fs::write(folder.join(format!("g-{n}.json")), bytes).unwrap();
        }
        // Even pids are gone. The sessions do not exist, so a replay fails and is kept.
        let _ = recover(dir.path(), |pid| pid % 2 == 0);
        let live = pids.iter().filter(|p| *p % 2 == 1).count();
        let left = files_in(dir.path());
        let json = left.iter().filter(|f| f.ends_with(".json") && !f.starts_with("g-")).count();
        prop_assert_eq!(json, live, "{:?}", left);
    }
}
