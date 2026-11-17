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
    Pending::for_task(dir.path(), &slug, "acted on")
        .unwrap()
        .crash();
    assert!(recover(dir.path()).is_empty());
    let notes = notes_of(dir.path(), &slug);
    assert_eq!(notes, [format!("{RECOVERED}acted on")]);
    assert!(recover(dir.path()).is_empty());
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
    assert!(recover(dir.path()).is_empty());
    assert!(notes_of(dir.path(), &slug).is_empty());
    assert_eq!(files_in(dir.path()).len(), 1, "the live entry stays");
    drop(pending);
    assert!(recover(dir.path()).is_empty());
    assert!(
        notes_of(dir.path(), &slug).is_empty(),
        "a finished command leaves no entry"
    );
    assert!(files_in(dir.path()).is_empty());
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
    assert!(recover(dir.path()).is_empty());
    assert_eq!(notes_of(dir.path(), &slug).len(), 1);
}

#[test]
fn a_note_that_cannot_be_written_is_kept_and_reported_with_its_text() {
    let (dir, _) = state_with_task();
    Pending::for_task(dir.path(), "gone", "text-1")
        .unwrap()
        .crash();
    let lost = recover(dir.path());
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(
        lost[0].contains("text-1") && lost[0].contains(".failed"),
        "{lost:?}"
    );
    assert!(
        recover(dir.path()).is_empty(),
        "a lost note is reported once"
    );
    let files = files_in(dir.path());
    assert!(files.iter().any(|f| f.ends_with(".failed")), "{files:?}");
}

#[test]
fn a_slug_in_an_entry_is_not_matched_by_prefix() {
    let (dir, slug) = state_with_task();
    let prefix = &slug[..slug.len() - 1];
    Pending::for_task(dir.path(), prefix, "text-2")
        .unwrap()
        .crash();
    let lost = recover(dir.path());
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(notes_of(dir.path(), &slug).is_empty());
}

#[test]
fn a_cut_short_entry_moves_aside_and_is_not_deleted() {
    let (dir, slug) = state_with_task();
    let folder = journal_dir(dir.path());
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("1-0.json"), b"{\"target\":").unwrap();
    assert!(recover(dir.path()).is_empty());
    assert!(notes_of(dir.path(), &slug).is_empty());
    assert_eq!(files_in(dir.path()), ["1-0.corrupt"]);
}

#[cfg(unix)]
#[test]
fn a_link_in_the_journal_is_not_followed() {
    let (dir, slug) = state_with_task();
    let secret = dir.path().join("secret.json");
    let entry = Entry {
        target: Target::Task(slug.clone()),
        text: "stolen".into(),
    };
    std::fs::write(&secret, serde_json::to_vec(&entry).unwrap()).unwrap();
    let folder = journal_dir(dir.path());
    std::fs::create_dir_all(&folder).unwrap();
    std::os::unix::fs::symlink(&secret, folder.join("1-0.json")).unwrap();
    assert!(recover(dir.path()).is_empty());
    assert!(notes_of(dir.path(), &slug).is_empty());
}

#[test]
fn an_entry_that_a_replay_holds_is_not_replayed_again() {
    let (dir, slug) = state_with_task();
    Pending::for_task(dir.path(), &slug, "acted on")
        .unwrap()
        .crash();
    let entry = entry_paths(dir.path()).pop().unwrap();
    let _replay = lock_entry(&entry).expect("the entry of a gone command can be locked");
    assert!(recover(dir.path()).is_empty());
    assert!(
        notes_of(dir.path(), &slug).is_empty(),
        "the held entry waits for its replay"
    );
}

proptest! {
    #[test]
    fn an_entry_survives_a_round_trip(
        id in ".{0,40}",
        text in ".{0,200}",
        session in any::<bool>(),
    ) {
        let target = if session { Target::Session(id) } else { Target::Task(id) };
        let entry = Entry { target, text };
        let bytes = serde_json::to_vec(&entry).unwrap();
        prop_assert_eq!(serde_json::from_slice::<Entry>(&bytes).unwrap(), entry);
    }

    #[test]
    fn recovery_ends_every_entry_it_can_lock(
        texts in proptest::collection::vec(".{0,30}", 0..4),
        garbage in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..40), 0..3),
    ) {
        let dir = tempfile::TempDir::new().unwrap();
        let folder = journal_dir(dir.path());
        std::fs::create_dir_all(&folder).unwrap();
        for (n, text) in texts.iter().enumerate() {
            let entry = Entry { target: Target::Session("none".into()), text: text.clone() };
            std::fs::write(folder.join(format!("e-{n}.json")), serde_json::to_vec(&entry).unwrap()).unwrap();
        }
        for (n, bytes) in garbage.iter().enumerate() {
            std::fs::write(folder.join(format!("g-{n}.json")), bytes).unwrap();
        }
        // The sessions do not exist, so a replay fails and the entry is kept as .failed. An
        // entry that does not parse moves aside. No entry stays under its .json name.
        let _ = recover(dir.path());
        let left = files_in(dir.path());
        prop_assert!(left.iter().all(|f| !f.ends_with(".json")), "{:?}", left);
    }
}
