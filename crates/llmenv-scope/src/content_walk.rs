//! Content-scope matching: one walk of the working directory for every content scope (#703).

use llmenv_config::ContentScope;

/// Log a walk error. An error at the root means no entry can be read, so it is an error. An error
/// below the root skips one entry, so it is a warning (#3070).
fn report_walk_error(err: &walkdir::Error, cwd: &std::path::Path) {
    if err.depth() == 0 {
        let fix = match err.io_error().map(std::io::Error::kind) {
            Some(std::io::ErrorKind::NotFound) => {
                "Fix: run the command from a directory that exists"
            }
            _ => "Fix: check the permissions of the working directory",
        };
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: content scopes cannot read the working directory {}, so none of them match \
             ({err}). {fix}.",
            cwd.display()
        ));
    } else {
        tracing::warn!(error = %err, "walkdir entry error; skipping");
    }
}

/// Each content scope previously triggered its own `walkdir` traversal
/// (#703) — N active content scopes meant N full tree walks on every hook
/// fire and export. Here all globs are compiled up front and evaluated
/// per entry against a single walk; a scope drops out of the pending set as
/// soon as it matches, and the walk ends early once none remain pending.
///
/// Returns the `id`s of scopes whose glob matched, and the `id`s whose state is unknown.
///
/// A scope is unknown when it did not match and the walk could not read part of the tree. The
/// part it could not read may hold a match. A working directory that cannot be read makes every
/// scope with a valid glob unknown. That case is logged at error, so the result has a visible
/// cause (#3070, #3330).
#[must_use]
pub fn matches_content_all<'a>(
    scopes: &'a [ContentScope],
    cwd: &std::path::Path,
) -> ContentMatches<'a> {
    let mut pending: Vec<(&str, globset::GlobMatcher, Option<usize>)> = scopes
        .iter()
        .filter_map(|s| match globset::Glob::new(&s.r#match.glob) {
            Ok(glob) => Some((s.id.as_str(), glob.compile_matcher(), s.r#match.depth)),
            // The scope cannot match at all, so this is logged where a user sees it (#3070 review).
            Err(e) => {
                llmenv_util::agent_notice::record_logged(format!(
                    "llmenv: content scope '{}' has an invalid glob '{}' ({e}), so it never \
                     matches. Fix: correct the glob in config.yaml.",
                    s.id, s.r#match.glob
                ));
                None
            }
        })
        .collect();

    let mut matched = std::collections::BTreeSet::new();
    if pending.is_empty() {
        return ContentMatches::default();
    }

    // Cap the walk at the loosest per-scope depth limit; any scope with no
    // limit forces an unbounded walk (short-circuits to `None` below).
    let max_depth = pending
        .iter()
        .map(|(_, _, d)| *d)
        .try_fold(0usize, |acc, d| d.map(|d| acc.max(d)));

    let mut walker = walkdir::WalkDir::new(cwd).follow_links(false);
    if let Some(depth) = max_depth {
        walker = walker.max_depth(depth);
    }

    // The shallowest depth at which the walk could not read an entry. A scope whose `depth` limit
    // stops above the entries below that depth cannot have matched there, so it is not unknown.
    let mut shallowest_failure: Option<usize> = None;
    for entry in walker {
        if pending.is_empty() {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                report_walk_error(&e, cwd);
                shallowest_failure =
                    Some(shallowest_failure.map_or(e.depth(), |d| d.min(e.depth())));
                continue;
            }
        };
        if entry.file_type().is_dir() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(cwd) else {
            // walkdir only yields paths under root, so this is a walkdir bug. It logs and skips in
            // every build, so a debug build does not behave differently from a release build.
            tracing::warn!(
                path = ?entry.path(),
                cwd = ?cwd,
                "walkdir yielded path outside root; skipping",
            );
            continue;
        };
        let entry_depth = entry.depth();
        pending.retain(|(id, matcher, depth)| {
            if depth.is_some_and(|d| entry_depth > d) {
                return true;
            }
            if matcher.is_match(relative) {
                matched.insert(*id);
                false
            } else {
                true
            }
        });
    }
    let unknown = shallowest_failure.map_or_else(std::collections::BTreeSet::new, |failed_at| {
        pending
            .iter()
            .filter(|(_, _, depth)| depth.is_none_or(|d| d > failed_at))
            .map(|(id, _, _)| *id)
            .collect()
    });
    ContentMatches { matched, unknown }
}

/// The result of one content walk: each scope is matched, unmatched, or unknown (#3330).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ContentMatches<'a> {
    /// Scopes whose glob matched.
    pub matched: std::collections::BTreeSet<&'a str>,
    /// Scopes that did not match, while part of the tree could not be read.
    pub unknown: std::collections::BTreeSet<&'a str>,
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::{ContentMatches, matches_content_all};
    use llmenv_config::{ContentMatch, ContentScope};
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    const GLOBS: &[&str] = &["*.rs", "*.md", "a/*", "**/b.rs", "c.*", "*.go"];

    /// The naive form: one full walk per scope, with the depth limit applied to each file.
    fn naive(scopes: &[ContentScope], cwd: &Path) -> BTreeSet<String> {
        let files: Vec<(std::path::PathBuf, usize)> = walkdir::WalkDir::new(cwd)
            .into_iter()
            .map(|e| e.expect("walk the fixture"))
            .filter(|e| !e.file_type().is_dir())
            .map(|e| {
                (
                    e.path().strip_prefix(cwd).expect("under cwd").to_path_buf(),
                    e.depth(),
                )
            })
            .collect();
        scopes
            .iter()
            .filter(|s| {
                let matcher = globset::Glob::new(&s.r#match.glob)
                    .expect("a fixture glob is valid")
                    .compile_matcher();
                files.iter().any(|(path, depth)| {
                    s.r#match.depth.is_none_or(|d| *depth <= d) && matcher.is_match(path)
                })
            })
            .map(|s| s.id.clone())
            .collect()
    }

    fn file_path() -> impl Strategy<Value = String> {
        proptest::collection::vec(prop::sample::select(vec!["a", "b", "c"]), 0..3).prop_flat_map(
            |dirs| {
                prop::sample::select(vec!["b.rs", "x.md", "c.txt", "y.rs"])
                    .prop_map(move |name| [dirs.join("/"), name.to_string()].join("/"))
            },
        )
    }

    proptest! {
        /// One shared walk with early exit and per-scope depth gives the same scopes as one full
        /// walk per scope (#703).
        #[test]
        fn one_walk_matches_one_walk_per_scope(
            files in proptest::collection::vec(file_path(), 0..8),
            specs in proptest::collection::vec(
                (prop::sample::select(GLOBS), proptest::option::of(1usize..4)),
                0..6,
            ),
        ) {
            let dir = tempfile::tempdir().expect("tempdir");
            for file in &files {
                let path = dir.path().join(file.trim_start_matches('/'));
                std::fs::create_dir_all(path.parent().expect("a file has a parent"))
                    .expect("mkdir");
                std::fs::write(&path, "").expect("write");
            }
            let scopes: Vec<ContentScope> = specs
                .iter()
                .enumerate()
                .map(|(i, (glob, depth))| ContentScope {
                    id: format!("s{i}"),
                    r#match: ContentMatch { glob: (*glob).to_string(), depth: *depth },
                    tags: Vec::new(),
                })
                .collect();

            let found = matches_content_all(&scopes, dir.path());
            let shared: BTreeSet<String> = found.matched.iter().map(|s| (*s).to_string()).collect();
            prop_assert_eq!(shared, naive(&scopes, dir.path()));
            // A readable tree leaves no scope unknown, and no scope is both matched and unknown.
            prop_assert!(found.unknown.is_empty());
            prop_assert!(found.matched.is_disjoint(&found.unknown));
        }
    }

    fn scope(id: &str, glob: &str, depth: Option<usize>) -> ContentScope {
        ContentScope {
            id: id.to_string(),
            r#match: ContentMatch {
                glob: glob.to_string(),
                depth,
            },
            tags: Vec::new(),
        }
    }

    #[test]
    fn no_scopes_match_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            matches_content_all(&[], dir.path()),
            ContentMatches::default()
        );
    }

    // #3330: a working directory that cannot be read gives an unknown state, and the notice names it.
    #[test]
    fn an_unreadable_working_directory_is_unknown_and_says_why() {
        let dir = tempfile::tempdir().expect("temp dir");
        let gone = dir.path().join("content-walk-gone-3070");
        let scopes = [scope("a", "**/*.rs", None)];
        let found = matches_content_all(&scopes, &gone);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        llmenv_util::agent_notice::forget_containing("content-walk-gone-3070");
        assert!(found.matched.is_empty());
        assert_eq!(found.unknown, BTreeSet::from(["a"]));
        assert!(notices.contains("a directory that exists"), "{notices}");
        assert!(
            notices.contains("content-walk-gone-3070") && notices.contains("Fix:"),
            "{notices}"
        );
    }

    // #3330: an unreadable subdirectory leaves an unmatched scope unknown. A scope that matched
    // stays matched.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_subdirectory_leaves_only_unmatched_scopes_unknown() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("a.rs"), "").expect("fixture");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("fixture");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("fixture");
        let scopes = [scope("rs", "*.rs", None), scope("md", "*.md", None)];
        let found = matches_content_all(&scopes, dir.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("fixture");
        if std::fs::read_dir(&locked).is_ok() && found.unknown.is_empty() {
            return; // running as root: the mode does not block the read
        }
        assert_eq!(found.matched, BTreeSet::from(["rs"]));
        assert_eq!(found.unknown, BTreeSet::from(["md"]));
    }

    // #3330: an unreadable folder at depth 1 hides entries at depth 2 and below. A scope whose depth
    // limit stops at 1 could not have matched there, so it is not unknown.
    #[cfg(unix)]
    #[test]
    fn a_depth_limit_above_the_unreadable_folder_leaves_the_scope_not_unknown() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("fixture");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("fixture");
        let scopes = [
            scope("shallow", "*.md", Some(1)),
            scope("deep", "*.md", Some(3)),
            scope("open", "*.md", None),
        ];
        let found = matches_content_all(&scopes, dir.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("fixture");
        if found.unknown.is_empty() {
            return; // running as root: the mode does not block the read
        }
        assert_eq!(found.unknown, BTreeSet::from(["deep", "open"]));
    }
}
