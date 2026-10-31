//! Read of the `.llmenv.yaml` project marker file.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::matcher::{Env, ResolvedProject};
use super::tags::sanitize_tags;

/// Schema for the body of `.llmenv.yaml` (project marker file).
/// All fields optional; an empty file is valid.
#[derive(Debug, Default, Deserialize)]
struct ProjectFile {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    enable_bundles: Vec<String>,
    #[serde(default)]
    disable_bundles: Vec<String>,
    /// Capture unknown fields for warning emission.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_yaml::Value>,
}

/// Maximum length (in bytes) for the project description. Anything longer
/// is truncated and a warning is logged. The description is surfaced into
/// LLM context chunks; a hard cap prevents a malformed or hostile marker
/// from bloating every prompt.
pub(crate) const MAX_DESCRIPTION_BYTES: usize = 1024;

/// The largest `.llmenv.yaml` that llmenv reads. A cloned repository controls the file, and a
/// marker holds a few short fields, so a larger file is a mistake or an attack (#2942).
pub const MAX_MARKER_FILE_BYTES: u64 = 64 * 1024;

/// Read a marker under its link policy (#2946).
pub use super::marker_link::read_marker;

/// Parse `.llmenv.yaml` file into a `ProjectFile`. Empty file → all defaults.
/// Malformed YAML → log warning and return defaults. The `description`
/// field is truncated to `MAX_DESCRIPTION_BYTES` if oversized.
fn read_project_file(path: &std::path::Path) -> ProjectFile {
    let body = match super::marker_link::read_marker(path, MAX_MARKER_FILE_BYTES) {
        Ok(Some(body)) => body,
        Ok(None) => return ProjectFile::default(),
        Err(e) => {
            // Hook stderr is dropped, so the agent notice is the one place the user sees this.
            llmenv_util::agent_notice::record_logged(format!(
                "llmenv: cannot read {}, so its tags and bundles are off: {e}. Fix: correct or \
                 remove that file",
                path.display()
            ));
            return ProjectFile::default();
        }
    };
    if body.trim().is_empty() {
        return ProjectFile::default();
    }
    match serde_yaml::from_str::<ProjectFile>(&body) {
        Ok(mut pf) => {
            if let Some(desc) = pf.description.as_mut()
                && truncate_at_char_boundary(desc, MAX_DESCRIPTION_BYTES)
            {
                tracing::warn!(
                    "project marker file {} has description >{} bytes; truncating",
                    path.display(),
                    MAX_DESCRIPTION_BYTES
                );
            }
            pf
        }
        Err(e) => {
            tracing::error!(
                "project marker file {} is not valid YAML: {e}; using defaults",
                path.display()
            );
            ProjectFile::default()
        }
    }
}

/// Discover project by walking cwd upward looking for `.llmenv.yaml`.
/// When found, parse and return a `ResolvedProject` with all fields resolved
/// (defaults applied, unknown fields collected). If YAML is malformed, log a
/// warning and return a minimal `ResolvedProject` with id/name from the
/// folder basename.
///
/// The walk is bounded at `$HOME`: a marker at `~/.llmenv.yaml` activates,
/// but the walk does not ascend above home. This prevents a hostile marker
/// dropped in e.g. `/tmp` (on a shared host) or `/Volumes/...` from being
/// picked up. When `$HOME` is unknown, only the cwd itself is checked.
#[must_use]
pub fn discover_project(env: &Env) -> Option<ResolvedProject> {
    let mut cur = std::path::PathBuf::from(&env.cwd);
    loop {
        let marker_path = cur.join(".llmenv.yaml");
        // A dangling link counts as a marker, so its read error is logged instead of a silent
        // walk to a parent marker.
        if std::fs::symlink_metadata(&marker_path).is_ok() {
            let pf = read_project_file(&marker_path);
            let basename = cur
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("llmenv")
                .to_string();
            let id = pf.id.unwrap_or_else(|| basename.clone());
            let name = pf.name.unwrap_or_else(|| basename.clone());
            let unknown_fields: Vec<String> = pf
                .extra
                .keys()
                .filter(|k| {
                    !matches!(
                        k.as_str(),
                        "id" | "name"
                            | "description"
                            | "tags"
                            | "enable_bundles"
                            | "disable_bundles"
                    )
                })
                .cloned()
                .collect();
            return Some(ResolvedProject {
                root: cur,
                id,
                name,
                description: pf.description,
                tags: sanitize_tags(pf.tags, ".llmenv.yaml"),
                enable_bundles: sanitize_tags(pf.enable_bundles, ".llmenv.yaml enable_bundles"),
                disable_bundles: sanitize_tags(pf.disable_bundles, ".llmenv.yaml disable_bundles"),
                unknown_fields,
            });
        }
        // Stop the walk once we've checked $HOME (or if home is unknown,
        // after checking only cwd). This blocks markers above home from
        // activating.
        match &env.home {
            Some(h) if cur == *h => break,
            None => break,
            _ => {}
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Cut `text` to at most `max` bytes at a char boundary, so the result stays valid UTF-8.
/// Returns whether it cut anything.
fn truncate_at_char_boundary(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let cut = (0..=max)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    text.truncate(cut);
    true
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    proptest::proptest! {
        #[test]
        fn a_cut_is_the_longest_char_prefix_within_the_cap(text in ".{0,40}", max in 0usize..48) {
            let mut cut = text.clone();
            let changed = truncate_at_char_boundary(&mut cut, max);
            proptest::prop_assert_eq!(changed, text.len() > max);
            proptest::prop_assert!(text.starts_with(&cut));
            let longest = text
                .char_indices()
                .map(|(i, c)| i + c.len_utf8())
                .filter(|&end| end <= max)
                .max()
                .unwrap_or(0);
            if changed {
                proptest::prop_assert_eq!(cut.len(), longest);
            } else {
                proptest::prop_assert_eq!(cut, text);
            }
        }
    }

    #[test]
    fn malformed_project_file_is_logged_at_error_level() {
        // #2666: the default log filter is ERROR, so a warn here never reaches the user.
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let marker = temp_dir.path().join(".llmenv.yaml");
        std::fs::write(&marker, "description: [unclosed\n").expect("write marker");
        let logs = crate::test_log_capture::capture_logs(|| {
            assert_eq!(read_project_file(&marker).description, None);
        });
        assert!(logs.contains("ERROR"), "expected an ERROR line: {logs}");
        assert!(logs.contains("not valid YAML"), "{logs}");
    }

    #[test]
    fn a_fifo_or_oversized_marker_gives_defaults_at_once() {
        let temp_dir = tempfile::TempDir::new().expect("tempdir");
        let marker = temp_dir.path().join(".llmenv.yaml");
        let made = std::process::Command::new("mkfifo")
            .arg(&marker)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        assert_eq!(read_project_file(&marker).id, None);
        std::fs::remove_file(&marker).expect("remove fifo");
        let max = usize::try_from(MAX_MARKER_FILE_BYTES).expect("cap fits usize");
        let mut body = b"id: big\n".to_vec();
        body.resize(max + 1, b'#');
        std::fs::write(&marker, &body).expect("write marker");
        assert_eq!(read_project_file(&marker).id, None);
        body.truncate(max);
        std::fs::write(&marker, &body).expect("write marker");
        assert_eq!(read_project_file(&marker).id.as_deref(), Some("big"));
    }

    #[test]
    fn a_dangling_marker_link_is_the_marker_and_its_error_is_logged() {
        let home = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(home.path().join(".llmenv.yaml"), "id: parent\n").expect("write");
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).expect("mkdir");
        std::os::unix::fs::symlink(repo.join("gone.yaml"), repo.join(".llmenv.yaml"))
            .expect("symlink");
        let env = super::Env {
            cwd: repo.to_string_lossy().to_string(),
            home: Some(home.path().to_path_buf()),
            ..super::Env::empty()
        };
        let logs = crate::test_log_capture::capture_logs(|| {
            let project = super::discover_project(&env).expect("discover");
            assert_eq!(
                project.root, repo,
                "the walk must stop at the dangling link"
            );
            assert_eq!(project.id, "repo");
        });
        assert!(logs.contains("does not exist"), "{logs}");
    }
}
