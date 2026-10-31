//! Moves the `projects/` trees that older layouts left in hashed folders into the durable store.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use super::PROJECTS_DIR;
use super::fold::move_dir_newest_wins;

/// Move every `projects/` tree stranded in an old hashed folder into the durable
/// store, newest mtime winning per file. Returns how many trees were folded.
///
/// Handles both layouts: `<root>/<shape>/projects` (loose, strict) and
/// `<root>/<version>/<shape>/projects` (normal) — the old code only ever scanned
/// the flat one, which is why transcripts piled up unmigrated. The now-empty
/// source directories are left for `llmenv clean` to reclaim.
///
/// # Errors
/// Returns an error when a tree cannot be folded into the durable store.
pub(crate) fn migrate_stranded_projects(
    adapter_root: &Path,
    state_dir: &Path,
) -> anyhow::Result<usize> {
    let target = state_dir.join(PROJECTS_DIR);
    let mut folded = 0usize;
    for src in stranded_projects_dirs(adapter_root, state_dir)? {
        let skipped = move_dir_newest_wins(&src, &target)
            .with_context(|| format!("folding stranded transcripts from {}", src.display()))?;
        if !skipped.is_empty() {
            tracing::warn!(
                "{} entries stay in {}, which the fold does not move",
                skipped.len(),
                src.display()
            );
        }
        folded = folded.saturating_add(1);
    }
    Ok(folded)
}

/// Every stranded `projects/` directory under `adapter_root`, at both the flat
/// and version-nested depths. Excludes the durable store and already-linked
/// folders (following a link would fold the store into itself).
fn stranded_projects_dirs(adapter_root: &Path, state_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for level1 in child_dirs(adapter_root)? {
        if level1 == state_dir {
            continue;
        }
        push_projects_dir(&level1, &mut out);
        for level2 in child_dirs(&level1)? {
            push_projects_dir(&level2, &mut out);
        }
    }
    Ok(out)
}

/// Immediate subdirectories of `dir`, skipping symlinks and staging dirs.
///
/// An absent `dir` has no children. Any other failure is an error, not an empty list. An empty
/// list would let the caller write the migrated marker, and the transcripts behind the failure
/// would never move. The caller retries on the next start.
fn child_dirs(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let Some(read_dir) = crate::paths::read_dir_optional(dir)
        .with_context(|| format!("cannot scan {} for transcripts", dir.display()))?
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in read_dir {
        let entry =
            entry.with_context(|| format!("cannot list {} for transcripts", dir.display()))?;
        if entry.file_name().as_encoded_bytes().ends_with(b".tmp") {
            continue;
        }
        let file_type = entry.file_type().with_context(|| {
            format!(
                "cannot read the type of {}, so its transcripts are not migrated",
                entry.path().display()
            )
        })?;
        if file_type.is_dir() {
            out.push(entry.path());
        }
    }
    Ok(out)
}

/// Push `<dir>/projects` onto `out` when it is a real directory (not a link).
fn push_projects_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let candidate = dir.join(PROJECTS_DIR);
    if std::fs::symlink_metadata(&candidate).is_ok_and(|md| md.file_type().is_dir()) {
        out.push(candidate);
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, reason = "a test: a failed setup must stop it")]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn an_unreadable_stranded_folder_fails_the_migration_instead_of_reporting_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        let adapter_root = dir.path().join("adapter");
        let locked = adapter_root.join("v1");
        std::fs::create_dir_all(&locked).expect("create the stranded folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("make the folder unreadable");
        let state = dir.path().join("state");
        let result = migrate_stranded_projects(&adapter_root, &state);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("restore the folder mode");
        assert!(
            result.is_err(),
            "an unreadable folder must not look migrated: {result:?}"
        );
    }
}
