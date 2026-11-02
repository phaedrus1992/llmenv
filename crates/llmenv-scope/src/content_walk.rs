//! Content-scope matching: one walk of the working directory for every content scope (#703).

use llmenv_config::ContentScope;

/// Log a walk error. An error at the root means no entry can be read, so it is an error. An error
/// below the root skips one entry, so it is a warning (#3070).
fn report_walk_error(err: &walkdir::Error, cwd: &std::path::Path) {
    if err.depth() == 0 {
        llmenv_util::agent_notice::record_logged(format!(
            "llmenv: content scopes cannot read the working directory {}, so none of them match \
             ({err}). Fix: check the permissions of the working directory.",
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
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::matches_content_all;
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

            let shared: BTreeSet<String> = matches_content_all(&scopes, dir.path())
                .into_iter()
                .map(str::to_string)
                .collect();
            prop_assert_eq!(shared, naive(&scopes, dir.path()));
        }
    }
}
