//! Content-scope matching: one walk of the working directory for every content scope (#703).

use crate::config::ContentScope;

/// Log a walk error. An error at the root means no entry can be read, so it is an error. An error
/// below the root skips one entry, so it is a warning (#3070).
fn report_walk_error(err: &walkdir::Error, cwd: &std::path::Path) {
    if err.depth() == 0 {
        tracing::error!(
            cwd = %cwd.display(),
            error = %err,
            "content scopes cannot read the working directory, so none of them match. Fix: check \
             the permissions of the working directory."
        );
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
/// Returns the `id`s of scopes whose glob matched.
///
/// A working directory that cannot be read gives no matches. That case is logged at error, so
/// the empty result has a visible cause (#3070).
#[must_use]
pub(crate) fn matches_content_all<'a>(
    scopes: &'a [ContentScope],
    cwd: &std::path::Path,
) -> std::collections::BTreeSet<&'a str> {
    let mut pending: Vec<(&str, globset::GlobMatcher, Option<usize>)> = scopes
        .iter()
        .filter_map(|s| match globset::Glob::new(&s.r#match.glob) {
            Ok(glob) => Some((s.id.as_str(), glob.compile_matcher(), s.r#match.depth)),
            // The scope cannot match at all, so this is logged where a user sees it (#3070 review).
            Err(e) => {
                tracing::error!(
                    scope = %s.id,
                    glob = %s.r#match.glob,
                    error = %e,
                    "content scope has an invalid glob, so it never matches. Fix: correct the \
                     glob in config.yaml."
                );
                None
            }
        })
        .collect();

    let mut matched = std::collections::BTreeSet::new();
    if pending.is_empty() {
        return matched;
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

    for entry in walker {
        if pending.is_empty() {
            break;
        }
        let Ok(entry) = entry.inspect_err(|e| report_walk_error(e, cwd)) else {
            continue;
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
    matched
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;
    use crate::config::{ContentMatch, ContentScope};
    use proptest::prelude::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    const GLOBS: [&str; 7] = [
        "*.rs",
        "**/*.rs",
        "src/**",
        "*.md",
        "**/deep/*",
        "src/*.rs",
        "missing/**",
    ];
    const DEPTHS: [Option<usize>; 5] = [None, Some(0), Some(1), Some(2), Some(3)];

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

    /// A fixed tree with files at several depths.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for file in [
            "top.rs",
            "top.md",
            "src/lib.rs",
            "src/notes.md",
            "src/deep/mod.rs",
            "src/deep/deeper/x.rs",
        ] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().expect("a file has a parent"))
                .expect("create the folders");
            std::fs::write(&path, "x").expect("write a fixture file");
        }
        dir
    }

    /// The reference: one full walk per scope, as each content scope walked before #703.
    fn naive_matches<'a>(scopes: &'a [ContentScope], cwd: &Path) -> BTreeSet<&'a str> {
        scopes
            .iter()
            .filter(|s| {
                let Ok(glob) = globset::Glob::new(&s.r#match.glob) else {
                    return false;
                };
                let matcher = glob.compile_matcher();
                let mut walk = walkdir::WalkDir::new(cwd).follow_links(false);
                if let Some(depth) = s.r#match.depth {
                    walk = walk.max_depth(depth);
                }
                walk.into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| !e.file_type().is_dir())
                    .any(|e| {
                        e.path()
                            .strip_prefix(cwd)
                            .is_ok_and(|rel| matcher.is_match(rel))
                    })
            })
            .map(|s| s.id.as_str())
            .collect()
    }

    #[test]
    fn no_scopes_match_nothing() {
        let dir = fixture();
        assert!(matches_content_all(&[], dir.path()).is_empty());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// One shared walk gives the same matches as one walk per scope, for any scope set.
        #[test]
        fn one_walk_matches_what_a_walk_per_scope_matches(
            picks in prop::collection::vec((0..GLOBS.len(), 0..DEPTHS.len()), 1..5),
        ) {
            let dir = fixture();
            let scopes: Vec<ContentScope> = picks
                .iter()
                .enumerate()
                .map(|(i, (g, d))| scope(&format!("s{i}"), GLOBS[*g], DEPTHS[*d]))
                .collect();
            prop_assert_eq!(
                matches_content_all(&scopes, dir.path()),
                naive_matches(&scopes, dir.path())
            );
        }
    }
}
