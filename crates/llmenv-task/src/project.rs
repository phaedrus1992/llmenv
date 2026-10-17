//! Project tag resolution (mandatory-sessions design, #493-follow-up): a
//! `{name}-{hash}` tag computed fresh on every invocation from cwd, stored as
//! metadata on a `Session` (never used to partition the task/session store —
//! see `docs/superpowers/specs/2026-07-21-task-project-scoping-design.md`).

use std::path::Path;

use sha2::{Digest, Sha256};

/// Resolve the project tag for the current process's working directory,
/// reading ambient `$HOME`. Thin wrapper over [`resolve_project_tag`] for the
/// common "wherever the agent invoked `llmenv` from" case — every `llmenv
/// task` invocation and hook runs with cwd set to the project directory.
///
/// # Errors
/// Propagates a failure to read the current working directory.
pub fn current_tag() -> std::io::Result<String> {
    let cwd = std::env::current_dir()?;
    let home = match std::env::var("HOME") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "cannot compute the task project tag: HOME is not valid UTF-8",
            ));
        }
    }
    .filter(|h| !h.is_empty())
    .map(std::path::PathBuf::from);
    Ok(resolve_project_tag(&cwd, home.as_deref()))
}

/// Resolve the project tag for `cwd`: `{name}-{hash}`, where `hash` is the
/// first 10 hex characters of `SHA-256(canonicalized absolute root path)`.
///
/// Resolution order:
/// 1. Walk up from `cwd` looking for a `.git` entry (file or directory —
///    existence check only, a worktree/submodule pointer's target is never
///    resolved). If found, that directory is the root. If a `.llmenv.yaml`
///    also exists in that same directory, its `id` field is used as `name`
///    instead of the root's basename (the root itself is still the git
///    root either way).
/// 2. If no `.git` is found anywhere walking up: fall back to the existing
///    `.llmenv.yaml` marker discovery (bounded at `home`, when given).
/// 3. If neither is found: `cwd` itself is the root, named from its
///    basename.
///
/// `name` is passed through [`super::slugify`] so the tag stays fs-safe and
/// bounded-length regardless of source (directory basename, `.llmenv.yaml`
/// `id`, or a Unicode-heavy path) — reuses the same sanitizer task titles
/// already go through rather than a second one.
#[must_use]
fn resolve_project_tag(cwd: &Path, home: Option<&Path>) -> String {
    let (root, raw_name) = find_git_root(cwd)
        .map(|root| {
            let name = read_llmenv_yaml_id(&root).unwrap_or_else(|| basename(&root));
            (root, name)
        })
        .or_else(|| {
            let env = llmenv_scope::matcher::Env {
                cwd: cwd.to_string_lossy().into_owned(),
                home: home.map(Path::to_path_buf),
                ..llmenv_scope::matcher::Env::empty()
            };
            llmenv_scope::matcher::discover_project(&env).map(|p| (p.root, p.id))
        })
        .unwrap_or_else(|| (cwd.to_path_buf(), basename(cwd)));

    let mut name = super::slugify(&raw_name);
    if name.is_empty() {
        name = "project".to_string();
    }
    let hash = hash_root(&root);
    format!("{name}-{hash}")
}

/// Walk `start` upward looking for a `.git` file or directory (existence
/// check only — a worktree/submodule pointer's target is never resolved).
/// Unbounded upward (unlike the `.llmenv.yaml` fallback): a `.git` entry is
/// definitionally the true project root wherever it lives, there's no
/// "hostile marker above home" concern the way there is for a
/// user-droppable `.llmenv.yaml`.
fn find_git_root(start: &Path) -> Option<std::path::PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

fn read_llmenv_yaml_id(dir: &Path) -> Option<String> {
    let path = dir.join(".llmenv.yaml");
    // A missing marker is the normal case (silent). A present-but-malformed
    // one is a real config error that silently changes the project tag's name
    // component — and the tag is the equality key for session scoping, so a
    // silent swap orphans open sessions. Warn on it, matching `list_sessions`'
    // tolerate-but-warn policy for corrupt store files, rather than lumping it
    // in with "absent".
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn_tag_change(
                &format!("could not read {} for the project id: {e}", path.display()),
                "check the permissions of that file",
            );
            return None;
        }
    };
    let value: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn_tag_change(
                &format!(
                    "{} is not valid YAML, so its project id is ignored and the directory name \
                     is used. This changes the project tag: {e}",
                    path.display()
                ),
                "correct the YAML in that file",
            );
            return None;
        }
    };
    match value.get("id") {
        // No `id` key is the normal case (the marker just sets tags/bundles);
        // fall back to the basename silently.
        None => None,
        Some(id) => match id.as_str() {
            Some(s) => Some(s.to_string()),
            // Present but not a string (`id: 123`, `id: [..]`) — the same
            // silent tag-swap the malformed-YAML branch guards against,
            // reached through a different malformation. Warn, don't swallow.
            None => {
                warn_tag_change(
                    &format!(
                        "{} has an `id` that is not a string, so it is ignored and the directory \
                         name is used. This changes the project tag",
                        path.display()
                    ),
                    "quote the `id` value in that file",
                );
                None
            }
        },
    }
}

/// Report a problem that changes the project tag. The tag scopes sessions, so a silent change
/// hides open sessions. Stderr reaches a CLI caller only; a hook's stderr is dropped, so the
/// problem is also logged and noticed for the agent (#2746).
fn warn_tag_change(problem: &str, fix: &str) {
    eprintln!("llmenv: {problem}");
    tracing::error!("project tag: {problem}");
    llmenv_util::agent_notice::record(format!("llmenv: {problem}. Fix: {fix}."));
}

fn basename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string()
}

fn hash_root(root: &Path) -> String {
    // Canonicalizing makes the tag stable regardless of how the root was
    // reached (symlinks, `.`/`..`). A failure (removed/permission-denied
    // root) falls back to the raw path so a tag is still produced, but log
    // it — an unstable tag (canonicalize failing on one call, succeeding on
    // another for the same root) would silently re-scope sessions.
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|e| {
        warn_tag_change(
            &format!(
                "could not resolve the project root {}, so the raw path is hashed. The project \
                 tag can change if this failure comes and goes: {e}",
                root.display()
            ),
            "check that the project folder exists and can be read",
        );
        root.to_path_buf()
    });
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::TempDir;

    #[test]
    fn a_malformed_marker_id_is_an_agent_notice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".llmenv.yaml"), "id: [1, 2]\n").unwrap();
        assert_eq!(read_llmenv_yaml_id(dir.path()), None);
        let notices = llmenv_util::agent_notice::current().unwrap_or_default();
        let marker = dir.path().join(".llmenv.yaml").display().to_string();
        assert!(
            notices.contains(&marker) && notices.contains("Fix: quote the `id`"),
            "{notices}"
        );
    }

    #[test]
    fn git_dir_root_uses_directory_basename_when_no_llmenv_yaml() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("my-repo");
        std::fs::create_dir_all(root.join(".git")).expect("test");
        let sub = root.join("src").join("nested");
        std::fs::create_dir_all(&sub).expect("test");

        let tag = resolve_project_tag(&sub, None);
        assert!(tag.starts_with("my-repo-"), "tag was {tag}");
        assert_eq!(tag.len(), "my-repo-".len() + 10);
    }

    #[test]
    fn git_file_worktree_pointer_counts_as_a_git_root() {
        // Existence-only check per the design doc: a `.git` *file* (worktree
        // or submodule pointer) counts, its target is never resolved.
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("worktree-repo");
        std::fs::create_dir_all(&root).expect("test");
        std::fs::write(root.join(".git"), "gitdir: ../elsewhere/.git/worktrees/x").expect("test");

        let tag = resolve_project_tag(&root, None);
        assert!(tag.starts_with("worktree-repo-"), "tag was {tag}");
    }

    #[test]
    fn llmenv_yaml_id_overrides_basename_when_colocated_with_git_root() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("basename-ignored");
        std::fs::create_dir_all(root.join(".git")).expect("test");
        std::fs::write(root.join(".llmenv.yaml"), "id: custom-id\n").expect("test");

        let tag = resolve_project_tag(&root, None);
        assert!(tag.starts_with("custom-id-"), "tag was {tag}");
    }

    #[test]
    fn non_string_llmenv_yaml_id_falls_back_to_basename() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("fallback-repo");
        std::fs::create_dir_all(root.join(".git")).expect("test");
        // `id` present but not a string — must not be used as the name; falls
        // back to the directory basename (and warns, tested by not panicking).
        std::fs::write(root.join(".llmenv.yaml"), "id: 123\n").expect("test");

        let tag = resolve_project_tag(&root, None);
        assert!(tag.starts_with("fallback-repo-"), "tag was {tag}");
    }

    #[test]
    fn malformed_llmenv_yaml_falls_back_to_basename() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("broken-repo");
        std::fs::create_dir_all(root.join(".git")).expect("test");
        std::fs::write(root.join(".llmenv.yaml"), "id: [unterminated\n").expect("test");

        let tag = resolve_project_tag(&root, None);
        assert!(tag.starts_with("broken-repo-"), "tag was {tag}");
    }

    #[test]
    fn no_git_anywhere_falls_back_to_llmenv_yaml_walkup() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("marker-project");
        std::fs::create_dir_all(&root).expect("test");
        std::fs::write(root.join(".llmenv.yaml"), "id: marker-id\n").expect("test");
        let sub = root.join("nested");
        std::fs::create_dir_all(&sub).expect("test");

        let tag = resolve_project_tag(&sub, Some(dir.path()));
        assert!(tag.starts_with("marker-id-"), "tag was {tag}");
    }

    #[test]
    fn neither_git_nor_marker_falls_back_to_literal_cwd_basename() {
        let dir = TempDir::new().expect("test");
        let cwd = dir.path().join("plain-dir");
        std::fs::create_dir_all(&cwd).expect("test");

        let tag = resolve_project_tag(&cwd, Some(dir.path()));
        assert!(tag.starts_with("plain-dir-"), "tag was {tag}");
    }

    #[test]
    fn same_root_always_produces_the_same_tag() {
        let dir = TempDir::new().expect("test");
        let root = dir.path().join("stable-repo");
        std::fs::create_dir_all(root.join(".git")).expect("test");

        let a = resolve_project_tag(&root, None);
        let b = resolve_project_tag(&root, None);
        assert_eq!(a, b);
    }

    proptest::proptest! {
        #[test]
        fn tag_is_bounded_length_and_fs_safe(name in "[a-zA-Z0-9_-]{1,40}") {
            let dir = tempfile::TempDir::new().unwrap();
            let root = dir.path().join(&name);
            std::fs::create_dir_all(root.join(".git")).unwrap();
            let tag = resolve_project_tag(&root, None);
            prop_assert!(tag.len() <= 80, "tag too long: {tag}");
            prop_assert!(
                tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "tag has unsafe chars: {tag}"
            );
        }
    }
}
