use super::*;
use std::os::fd::AsFd;

#[test]
fn opens_a_real_directory_and_lists_its_entries() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("child")).unwrap();
    std::fs::write(tmp.path().join("file"), b"x").unwrap();

    let dir = open_dir_nofollow(tmp.path()).unwrap();
    let mut names: Vec<_> = read_dir_entries(dir.as_fd())
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, ["child", "file"]);
}

#[test]
fn dot_entries_are_not_returned() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        read_dir_entries(open_dir_nofollow(tmp.path()).unwrap().as_fd())
            .unwrap()
            .is_empty(),
        "an empty directory has no entries once . and .. are dropped"
    );
}

// The whole point: a symlink to a directory must not open as that
// directory, however ordinary it looks from a path-based stat.
#[test]
fn refuses_to_open_a_symlink_to_a_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    // The errno differs by platform — Linux reports ELOOP, macOS ENOTDIR
    // — so the assertion is on the property that matters (the open fails)
    // rather than on a number that would make this a Linux-only test.
    assert!(
        open_dir_nofollow(&link).is_err(),
        "a symlink must not open as the directory it points at"
    );
}

#[test]
fn refuses_to_open_a_file_as_a_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(open_dir_nofollow(&file).is_err());
}

#[test]
fn descends_relative_to_an_open_directory() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();

    let root = open_dir_nofollow(tmp.path()).unwrap();
    let a = open_dir_at(root.as_fd(), OsStr::new("a")).unwrap();
    let b = read_dir_entries(a.as_fd()).unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].name, "b");
    assert!(b[0].is_dir());
}

// A relative open must stay inside the directory it was handed. Accepting
// `..` or an embedded slash would resolve multiple components again and
// give back the race this module removes.
#[test]
fn relative_open_rejects_anything_but_a_single_component() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
    let root = open_dir_nofollow(tmp.path()).unwrap();

    for bad in ["..", ".", "", "a/b", "/etc"] {
        let err = open_dir_at(root.as_fd(), OsStr::new(bad)).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "{bad:?} should be rejected before it reaches openat"
        );
    }
}

#[test]
fn entry_types_describe_the_link_not_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("real")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();

    let dir = open_dir_nofollow(tmp.path()).unwrap();
    let entries = read_dir_entries(dir.as_fd()).unwrap();
    let link = entries.iter().find(|e| e.name == "link").unwrap();
    assert!(link.is_symlink(), "a symlink to a dir is a symlink entry");
    assert!(!link.is_dir(), "and must not read as a directory");
}
#[test]
fn reads_a_file_relative_to_an_open_directory() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("data"), b"hello").unwrap();
    let dir = open_dir_nofollow(tmp.path()).unwrap();
    assert_eq!(
        read_file_at(dir.as_fd(), OsStr::new("data"), 64).unwrap(),
        b"hello"
    );
}

#[test]
fn refuses_to_read_through_a_symlinked_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("secret"), b"s").unwrap();
    std::os::unix::fs::symlink(tmp.path().join("secret"), tmp.path().join("link")).unwrap();
    let dir = open_dir_nofollow(tmp.path()).unwrap();
    assert!(
        read_file_at(dir.as_fd(), OsStr::new("link"), 64).is_err(),
        "reading must not follow a symlink to its target"
    );
}

#[test]
fn a_fifo_or_a_file_over_the_cap_fails_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(tmp.path().join("fifo"))
        .status()
        .unwrap();
    assert!(made.success());
    std::fs::write(tmp.path().join("big"), [b'a'; 65]).unwrap();
    let dir = open_dir_nofollow(tmp.path()).unwrap();
    let err = read_file_at(dir.as_fd(), OsStr::new("fifo"), 64).unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");
    let err = read_file_at(dir.as_fd(), OsStr::new("big"), 64).unwrap_err();
    assert!(err.to_string().contains("more than 64 bytes"), "{err}");
    assert_eq!(
        read_file_at(dir.as_fd(), OsStr::new("big"), 65)
            .unwrap()
            .len(),
        65
    );
}

#[test]
fn copy_streams_a_regular_file_and_refuses_a_fifo() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("t.jsonl"), b"line\n").unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(src.path().join("fifo"))
        .status()
        .unwrap();
    assert!(made.success());
    let (s, d) = (
        open_dir_nofollow(src.path()).unwrap(),
        open_dir_nofollow(dst.path()).unwrap(),
    );
    let mode = Mode::from(0o600);
    assert_eq!(
        copy_file_at(s.as_fd(), d.as_fd(), OsStr::new("t.jsonl"), mode).unwrap(),
        5
    );
    assert_eq!(
        std::fs::read(dst.path().join("t.jsonl")).unwrap(),
        b"line\n"
    );
    let err = copy_file_at(s.as_fd(), d.as_fd(), OsStr::new("fifo"), mode).unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");
    assert!(
        !dst.path().join("fifo").exists(),
        "a refused copy creates nothing"
    );
}

#[test]
fn file_reads_reject_anything_but_a_single_component() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = open_dir_nofollow(tmp.path()).unwrap();
    for bad in ["../etc/passwd", "a/b", ".."] {
        assert_eq!(
            read_file_at(dir.as_fd(), OsStr::new(bad), 64)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
#[test]
fn renames_between_open_directories() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("a")).unwrap();
    std::fs::create_dir(tmp.path().join("b")).unwrap();
    std::fs::write(tmp.path().join("a/f"), b"v").unwrap();

    let a = open_dir_nofollow(&tmp.path().join("a")).unwrap();
    let b = open_dir_nofollow(&tmp.path().join("b")).unwrap();
    rename_at(a.as_fd(), OsStr::new("f"), b.as_fd(), OsStr::new("f")).unwrap();

    assert!(!tmp.path().join("a/f").exists());
    assert_eq!(std::fs::read(tmp.path().join("b/f")).unwrap(), b"v");
}

// Replacing must remove a symlink, not write through it — the #1065
// behaviour, preserved now that the write goes through an fd.
#[test]
fn writing_replaces_a_symlink_instead_of_following_it() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::write(&outside, b"original").unwrap();
    std::fs::create_dir(tmp.path().join("d")).unwrap();
    std::os::unix::fs::symlink(&outside, tmp.path().join("d/link")).unwrap();

    std::fs::create_dir(tmp.path().join("s")).unwrap();
    std::fs::write(tmp.path().join("s/link"), b"new").unwrap();
    let s = open_dir_nofollow(&tmp.path().join("s")).unwrap();
    let d = open_dir_nofollow(&tmp.path().join("d")).unwrap();
    copy_file_at(s.as_fd(), d.as_fd(), OsStr::new("link"), Mode::from(0o600)).unwrap();

    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"original",
        "the symlink's target must be untouched"
    );
    assert_eq!(std::fs::read(tmp.path().join("d/link")).unwrap(), b"new");
    assert!(
        !std::fs::symlink_metadata(tmp.path().join("d/link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn mtime_is_none_for_a_missing_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = open_dir_nofollow(tmp.path()).unwrap();
    assert!(mtime_at(dir.as_fd(), OsStr::new("nope")).unwrap().is_none());
    std::fs::write(tmp.path().join("here"), b"x").unwrap();
    assert!(mtime_at(dir.as_fd(), OsStr::new("here")).unwrap().is_some());
    assert!(mtime_at(dir.as_fd(), OsStr::new("../x")).is_err());
}
#[test]
fn removes_an_empty_directory_but_not_a_symlink_to_one() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("empty")).unwrap();
    let target = tmp.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, tmp.path().join("link")).unwrap();

    let dir = open_dir_nofollow(tmp.path()).unwrap();
    remove_dir_at(dir.as_fd(), OsStr::new("empty")).unwrap();
    assert!(!tmp.path().join("empty").exists());

    assert!(
        remove_dir_at(dir.as_fd(), OsStr::new("link")).is_err(),
        "a symlink must not be removed as if it were the directory it points at"
    );
    assert!(target.exists(), "and its target must survive");
}

proptest::proptest! {
    /// A name is accepted exactly when it is one non-empty component that is not `.` or `..`.
    #[test]
    fn reject_traversal_accepts_only_single_components(s in "[a-z./]{0,8}") {
        let accepted = reject_traversal(OsStr::new(&s)).is_ok();
        let single = !s.is_empty() && s != "." && s != ".." && !s.contains('/');
        proptest::prop_assert_eq!(accepted, single, "name {:?}", s);
    }
}

/// Any one of the three execute bits makes a file executable; a missing entry is not.
#[test]
fn any_execute_bit_makes_an_entry_executable() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let dir = open_dir_nofollow(tmp.path()).unwrap();
    for (mode, expected) in [(0o700, true), (0o610, true), (0o601, true), (0o644, false)] {
        let name = format!("f{mode:o}");
        let path = tmp.path().join(&name);
        std::fs::write(&path, "x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            is_executable_at(dir.as_fd(), OsStr::new(&name)).unwrap(),
            expected,
            "{mode:o}"
        );
    }
    assert!(!is_executable_at(dir.as_fd(), OsStr::new("missing")).unwrap());
}

// A pre-1970 time or a nanosecond part outside one second is unknown, so it cannot overflow.
#[test]
fn a_stat_time_out_of_range_is_unknown() {
    let at =
        |secs: u64, nanos: u32| Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos));
    assert_eq!(stat_time(10i64, 999_999_999i64), at(10, 999_999_999));
    assert_eq!(stat_time(10i64, 0i64), at(10, 0));
    assert_eq!(stat_time(10i64, 1_000_000_000i64), None);
    assert_eq!(stat_time(-1i64, 0i64), None);
    assert_eq!(stat_time(10i64, -1i64), None);
    assert_eq!(stat_time(i64::MAX, 1_000_000_000i64), None);
}
