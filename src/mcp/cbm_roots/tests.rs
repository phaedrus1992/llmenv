use super::guard::{decide, decide_with_reads, path_outside_roots};
use super::*;
use proptest::prelude::*;

fn cm(index_path: Option<&str>) -> CodebaseMemory {
    CodebaseMemory {
        when: vec!["p".into()],
        index_path: index_path.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn the_listing_is_read_after_its_header() {
    let listing = "allowed roots:\n/Users/u/git\n/Users/u/.cache/nbl-diag/repos\n";
    assert_eq!(
        parse_roots(listing).unwrap(),
        [
            PathBuf::from("/Users/u/git"),
            PathBuf::from("/Users/u/.cache/nbl-diag/repos")
        ]
    );
    assert!(
        parse_roots("no allowed roots recorded \n")
            .unwrap()
            .is_empty()
    );
    assert!(parse_roots("").unwrap().is_empty());
}

#[test]
fn a_listing_in_a_new_format_is_an_error_not_an_empty_record() {
    // An empty record means unconfined, so a format change must not read as one (#2916).
    assert!(parse_roots("- /Users/u/git\n").is_err());
    assert!(parse_roots("allowed roots:\n  /Users/u/git\nsomething new\n").is_err());
}

#[test]
fn only_existing_unrecorded_roots_are_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (have, lack, gone) = (
        dir.path().join("have"),
        dir.path().join("lack"),
        dir.path().join("gone"),
    );
    std::fs::create_dir(&have).unwrap();
    std::fs::create_dir(&lack).unwrap();
    let wanted = vec![have.clone(), lack.clone(), gone];
    assert_eq!(missing_roots(&wanted, std::slice::from_ref(&have)), [lack]);
}

#[test]
fn an_empty_record_is_unconfined_so_nothing_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let lack = dir.path().join("lack");
    std::fs::create_dir(&lack).unwrap();
    assert!(missing_roots(&[lack], &[]).is_empty());
}

#[test]
fn a_folder_under_a_recorded_root_is_covered() {
    let dir = tempfile::tempdir().unwrap();
    let child = dir.path().join("parent/child");
    std::fs::create_dir_all(&child).unwrap();
    let parent = dir.path().join("parent");
    assert!(missing_roots(std::slice::from_ref(&child), &[parent]).is_empty());
}

#[test]
fn a_recorded_root_counts_through_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(missing_roots(&[link], &[real]).is_empty());
}

#[test]
fn a_recorded_root_that_does_not_resolve_is_left_out_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("gone-root-3462");
    assert!(undeclared_roots(std::slice::from_ref(&gone), &[]).is_empty());
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains(&format!("{} does not resolve", gone.display())),
        "{notices}"
    );
}

#[cfg(unix)]
#[test]
fn a_declared_root_that_cannot_be_read_is_left_out_with_a_notice() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked-3462");
    let root = locked.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable_anyway = std::fs::metadata(&root).is_ok();
    // An empty record means an unconfined server, so one recorded root makes the check run.
    let missing = missing_roots(std::slice::from_ref(&root), &[dir.path().join("other")]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable_anyway {
        return; // root ignores permissions, so the stat error cannot happen here
    }
    assert!(missing.is_empty(), "{missing:?}");
    let notices = crate::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains(&format!("{} cannot be read", root.display())),
        "{notices}"
    );
}

#[test]
fn a_recorded_root_that_no_declared_root_covers_is_undeclared() {
    let dir = tempfile::tempdir().unwrap();
    let declared = dir.path().join("declared");
    let wider = dir.path().join("wider");
    std::fs::create_dir_all(declared.join("sub")).unwrap();
    std::fs::create_dir(&wider).unwrap();
    let undeclared = undeclared_roots(
        &[declared.clone(), wider.clone()],
        std::slice::from_ref(&declared),
    );
    assert_eq!(undeclared, [wider]);
    assert!(undeclared_roots(&[declared.join("sub")], &[declared]).is_empty());
}

#[test]
fn a_path_with_spaces_is_quoted_for_the_shell() {
    assert_eq!(
        crate::util::shell_word(Path::new("/plain/path")),
        "/plain/path"
    );
    assert_eq!(crate::util::shell_word(Path::new("/a b")), "'/a b'");
    assert_eq!(crate::util::shell_word(Path::new("/it's")), "'/it'\\''s'");
    assert_eq!(crate::util::shell_word(Path::new("")), "''");
}

#[test]
fn a_failed_read_with_an_empty_record_denies_the_call() {
    let reason = decide_with_reads("/usr", vec![], &["timed out".to_string()]).unwrap();
    assert!(reason.contains("could not be read"), "{reason}");
    assert!(reason.contains("timed out"), "{reason}");
    assert_eq!(decide_with_reads("/usr", vec![], &[]), None);
}

#[test]
fn a_relative_repo_path_is_denied_not_resolved_against_the_hook_folder() {
    let listed = vec![std::env::current_dir().unwrap()];
    let reason = decide(".", listed).unwrap();
    assert!(reason.contains("not an absolute path"), "{reason}");
}

#[test]
fn a_record_that_resolves_to_nothing_denies_the_call() {
    let reason = decide("/usr", vec![PathBuf::from("/no/such/root/2916")]).unwrap();
    assert!(reason.contains("none of the 1 recorded roots"), "{reason}");
}

#[test]
fn the_cache_dir_is_the_index_path_or_the_default() {
    let mut c = cm(None);
    assert_eq!(
        cache_dir(&c, Some(Path::new("/home/u"))).unwrap(),
        Path::new("/home/u/.cache/codebase-memory-mcp")
    );
    c.index_path = Some("/idx".into());
    assert_eq!(cache_dir(&c, None).unwrap(), Path::new("/idx"));
}

#[test]
fn the_cache_dir_without_home_or_index_path_is_an_error() {
    let err = cache_dir(&cm(None), None).unwrap_err();
    assert!(
        err.to_string().contains("index_path"),
        "error must name the fix: {err}"
    );
}

#[test]
fn the_cache_dir_expands_a_leading_tilde_in_index_path() {
    let dir = cache_dir(&cm(Some("~/idx")), None).ok();
    assert!(
        dir.as_deref().is_some_and(Path::is_absolute),
        "a relative '~' directory would be a wrong cache: {dir:?}"
    );
}

#[test]
fn the_command_gets_the_cache_dir_of_the_server() {
    let env_of = |c: &CodebaseMemory| {
        command_with(
            Path::new("codebase-memory-mcp"),
            c,
            &["allow-root", "--list"],
        )
        .ok()?
        .get_envs()
        .find(|(k, _)| *k == std::ffi::OsStr::new("CBM_CACHE_DIR"))
        .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    };
    assert_eq!(env_of(&cm(None)), None);
    assert_eq!(env_of(&cm(Some("/idx"))).as_deref(), Some("/idx"));
}

#[test]
fn a_path_is_inside_a_root_or_the_reason_names_the_fix() {
    let roots = vec![PathBuf::from("/work/proj"), PathBuf::from("/srv/x")];
    assert_eq!(path_outside_roots(Path::new("/work/proj"), &roots), None);
    assert_eq!(
        path_outside_roots(Path::new("/work/proj/sub/dir"), &roots),
        None
    );
    let reason = path_outside_roots(Path::new("/work/projx"), &roots).unwrap();
    assert!(
        reason.contains("/work/projx") && reason.contains("/work/proj, /srv/x"),
        "{reason}"
    );
    assert!(
        reason.contains("allow-root") && reason.contains("Do not run allow-root yourself"),
        "{reason}"
    );
    assert!(path_outside_roots(Path::new("/tmp/elsewhere"), &roots).is_some());
    assert!(path_outside_roots(Path::new("/tmp"), &[]).is_some());
}

#[test]
fn an_empty_record_lets_every_path_through_the_guard() {
    assert_eq!(decide("/usr", vec![]), None);
}

#[test]
fn the_guard_passes_a_repo_in_a_recorded_root_and_denies_others() {
    let base = tempfile::tempdir().unwrap();
    let inside = base.path().join("inside");
    let repo = inside.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let outside = base.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let listed = vec![inside];
    assert_eq!(decide(repo.to_str().unwrap(), listed.clone()), None);
    let reason = decide(outside.to_str().unwrap(), listed).unwrap();
    assert!(reason.contains("allow-root"), "{reason}");
}

#[test]
fn the_guard_denies_a_path_that_does_not_exist_and_a_dot_dot_escape() {
    let base = tempfile::tempdir().unwrap();
    let inside = base.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    let listed = vec![inside.clone()];
    let missing = inside.join("gone");
    let reason = decide(missing.to_str().unwrap(), listed.clone()).unwrap();
    assert!(reason.contains("does not resolve"), "{reason}");
    let escape = inside.join("..");
    assert!(decide(escape.to_str().unwrap(), listed).is_some());
}

#[test]
fn a_link_out_of_a_root_is_denied_and_a_link_into_one_passes() {
    let base = tempfile::tempdir().unwrap();
    let inside = base.path().join("inside");
    let outside = base.path().join("outside");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let out_link = inside.join("out");
    std::os::unix::fs::symlink(&outside, &out_link).unwrap();
    let in_link = outside.join("in");
    std::os::unix::fs::symlink(&inside, &in_link).unwrap();
    let listed = vec![inside];
    assert!(decide(out_link.to_str().unwrap(), listed.clone()).is_some());
    assert_eq!(decide(in_link.to_str().unwrap(), listed), None);
}

#[test]
fn the_guard_does_not_apply_without_a_codebase_memory_entry() {
    assert_eq!(
        guard_decision("/anywhere", &[], Some(&std::collections::BTreeSet::new())),
        None
    );
}

proptest! {
    #[test]
    fn every_parsed_root_is_an_absolute_line_of_the_input(listing in "(\\PC{0,30}\n){0,6}") {
        let Ok(roots) = parse_roots(&listing) else {
            return Ok(());
        };
        for root in roots {
            prop_assert!(root.is_absolute());
            prop_assert!(listing.lines().any(|l| l.trim() == root.to_string_lossy()));
        }
    }

    #[test]
    fn a_shell_word_is_never_empty(path in "\\PC{0,40}") {
        prop_assert!(!crate::util::shell_word(Path::new(&path)).is_empty());
    }

    #[test]
    fn a_path_under_any_root_is_inside_and_a_path_under_none_is_outside(
        roots in prop::collection::vec("/[a-z]{1,6}(/[a-z]{1,6}){0,2}", 1..4),
        pick in 0usize..4,
        tail in "(/[a-z]{1,6}){0,3}",
        other in "/zz[0-9]{1,4}(/[a-z]{1,6}){0,2}",
    ) {
        let roots: Vec<PathBuf> = roots.into_iter().map(PathBuf::from).collect();
        let inside = PathBuf::from(format!("{}{tail}", roots[pick % roots.len()].display()));
        prop_assert_eq!(path_outside_roots(&inside, &roots), None);
        prop_assert!(path_outside_roots(Path::new(&other), &roots).is_some());
    }
}
