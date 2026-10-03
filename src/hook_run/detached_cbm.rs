//! Runs the codebase-memory index as a detached child that observes the indexer's exit status
//! (#2396). The parent (`trigger_codebase_memory_index`) writes a checkpoint and starts this
//! wrapper in place of the indexer, so a failed index leaves the checkpoint for `llmenv doctor`.
//! The wrapper's stderr is the bounded `index.log`, and the indexer inherits it.

use std::path::Path;

use anyhow::Context;

use crate::hook_run::checkpoint;

/// The inputs of one index job: what the wrapper needs to build the indexer command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct IndexInputs {
    pub(crate) project_root: String,
    pub(crate) index_path: Option<String>,
    /// `CBM_MEM_BUDGET_MB` for the indexer (#2154).
    #[serde(default)]
    pub(crate) mem_budget_mb: Option<u32>,
    /// The file that receives the indexer's JSON result on stdout (#2154).
    #[serde(default)]
    pub(crate) result_path: Option<String>,
}

/// Child entrypoint: read the inputs from `checkpoint_path`, run the indexer, wait for it, and
/// delete the checkpoint when it exits 0. A failure leaves the file and logs at error level.
///
/// # Errors
/// The checkpoint cannot be read, the indexer cannot start, or it exits non-zero.
pub fn run_cbm_index(checkpoint_path: &Path) -> anyhow::Result<()> {
    run_cbm_index_with(checkpoint_path, crate::hook_run::index_command).inspect_err(|e| {
        tracing::error!("cbm-index-run: indexing failed: {e:#}");
    })
}

/// [`run_cbm_index`] with the command builder injected, so a test can run a stub indexer.
fn run_cbm_index_with(
    checkpoint_path: &Path,
    build: impl FnOnce(&Path, Option<&str>, Option<u32>) -> std::process::Command,
) -> anyhow::Result<()> {
    let loaded = checkpoint::load(checkpoint_path)?;
    let inputs: IndexInputs = serde_json::from_value(loaded.inputs)
        .context("the checkpoint holds no project root for the indexer")?;
    let mut cmd = build(
        Path::new(&inputs.project_root),
        inputs.index_path.as_deref(),
        inputs.mem_budget_mb,
    );
    // The parent points this process's stderr at the index log; the indexer shares it.
    cmd.stderr(std::process::Stdio::inherit());
    // The indexer writes its result to a file of its own, which replaces the last result when the
    // indexer exits. A run that is killed, or that cannot start, leaves the last result alone, and
    // two runs never write to one file.
    let pending = inputs.result_path.as_deref().and_then(|final_path| {
        let pending = pending_result_path(Path::new(final_path));
        match crate::hook_run::create_result_file(&pending) {
            Ok(file) => {
                cmd.stdout(std::process::Stdio::from(file));
                Some((pending, final_path.to_string()))
            }
            Err(e) => {
                tracing::warn!(
                    "cannot create the index result file {}, so doctor cannot report the result: {e}",
                    pending.display()
                );
                None
            }
        }
    });
    let status = cmd.status();
    if let Some((pending, final_path)) = &pending {
        publish_result(pending, Path::new(final_path), status.is_ok());
    }
    let status = status.context("cannot start codebase-memory-mcp")?;
    anyhow::ensure!(status.success(), "codebase-memory-mcp exited with {status}");
    checkpoint::complete(checkpoint_path)
}

/// The file one run writes before it replaces the result: unique to this process.
fn pending_result_path(result: &Path) -> std::path::PathBuf {
    let name = result.file_name().unwrap_or_default().to_string_lossy();
    result.with_file_name(format!("{name}.{}.pending", std::process::id()))
}

/// Move the pending result over the last result when the indexer ran (`started`), and remove it
/// when the indexer never started.
fn publish_result(pending: &Path, result: &Path, started: bool) {
    if started {
        if let Err(e) = std::fs::rename(pending, result) {
            tracing::warn!("cannot save the index result to {}: {e}", result.display());
        }
    } else if let Err(e) = std::fs::remove_file(pending) {
        tracing::debug!("cannot remove {}: {e}", pending.display());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::hook_run::checkpoint::{Checkpoint, JobKind};

    fn written(dir: &Path) -> std::path::PathBuf {
        let inputs = serde_json::to_value(IndexInputs {
            project_root: "/repo".into(),
            index_path: None,
            mem_budget_mb: None,
            result_path: None,
        })
        .unwrap();
        checkpoint::write(dir, &Checkpoint::new(JobKind::CbmIndex, inputs, None))
            .unwrap()
            .unwrap()
    }

    fn shell(script: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    proptest::proptest! {
        #[test]
        fn index_inputs_survive_serialization(
            root in ".{0,40}", index in proptest::option::of(".{0,40}"),
            budget in proptest::option::of(proptest::prelude::any::<u32>()),
            result in proptest::option::of(".{0,40}"),
        ) {
            let inputs = IndexInputs {
                project_root: root,
                index_path: index,
                mem_budget_mb: budget,
                result_path: result,
            };
            let back: IndexInputs =
                serde_json::from_value(serde_json::to_value(&inputs).unwrap()).unwrap();
            proptest::prop_assert_eq!(back, inputs);
        }
    }

    #[test]
    fn the_real_entry_point_reports_an_indexer_that_fails() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = serde_json::to_value(IndexInputs {
            project_root: "/definitely/not/a/project/root".into(),
            index_path: None,
            mem_budget_mb: None,
            result_path: None,
        })
        .unwrap();
        let file = checkpoint::write(
            dir.path(),
            &Checkpoint::new(JobKind::CbmIndex, inputs, None),
        )
        .unwrap()
        .unwrap();
        // Either codebase-memory-mcp is missing, or it rejects the path: both are errors.
        assert!(run_cbm_index(&file).is_err());
        assert!(file.exists());
    }

    #[test]
    fn exit_zero_completes_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let file = written(dir.path());
        run_cbm_index_with(&file, |_, _, _| shell("exit 0")).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn a_non_zero_exit_leaves_the_checkpoint_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = written(dir.path());
        let err = run_cbm_index_with(&file, |_, _, _| shell("exit 3")).unwrap_err();
        assert!(err.to_string().contains("exited"), "{err}");
        assert!(file.exists());
    }

    #[test]
    fn the_indexer_gets_the_project_root_and_index_path_from_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = serde_json::to_value(IndexInputs {
            project_root: "/repo".into(),
            index_path: Some("/idx".into()),
            mem_budget_mb: Some(4096),
            result_path: None,
        })
        .unwrap();
        let file = checkpoint::write(
            dir.path(),
            &Checkpoint::new(JobKind::CbmIndex, inputs, None),
        )
        .unwrap()
        .unwrap();
        let mut seen = None;
        run_cbm_index_with(&file, |root, idx, budget| {
            seen = Some((root.to_path_buf(), idx.map(str::to_string), budget));
            shell("exit 0")
        })
        .unwrap();
        assert_eq!(
            seen,
            Some(("/repo".into(), Some("/idx".to_string()), Some(4096)))
        );
    }

    #[test]
    fn the_indexer_stdout_goes_to_the_result_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = dir.path().join("result.json");
        let inputs = serde_json::to_value(IndexInputs {
            project_root: "/repo".into(),
            index_path: None,
            mem_budget_mb: None,
            result_path: Some(result.display().to_string()),
        })
        .unwrap();
        let file = checkpoint::write(
            dir.path(),
            &Checkpoint::new(JobKind::CbmIndex, inputs, None),
        )
        .unwrap()
        .unwrap();
        run_cbm_index_with(&file, |_, _, _| shell("echo '{\"status\":\"ok\"}'")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&result).unwrap().trim(),
            r#"{"status":"ok"}"#
        );
    }

    fn with_result(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let result = dir.join("result.json");
        let inputs = serde_json::to_value(IndexInputs {
            project_root: "/repo".into(),
            index_path: None,
            mem_budget_mb: None,
            result_path: Some(result.display().to_string()),
        })
        .unwrap();
        let file = checkpoint::write(dir, &Checkpoint::new(JobKind::CbmIndex, inputs, None))
            .unwrap()
            .unwrap();
        (file, result)
    }

    #[test]
    fn a_failed_run_still_publishes_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let (file, result) = with_result(dir.path());
        std::fs::write(&result, "the old result").unwrap();
        let script = "echo '{\"status\":\"error\"}'; exit 3";
        assert!(run_cbm_index_with(&file, |_, _, _| shell(script)).is_err());
        assert_eq!(
            std::fs::read_to_string(&result).unwrap().trim(),
            r#"{"status":"error"}"#
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".pending"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_indexer_that_cannot_start_keeps_the_last_result() {
        let dir = tempfile::tempdir().unwrap();
        let (file, result) = with_result(dir.path());
        std::fs::write(&result, "the old result").unwrap();
        let missing = || std::process::Command::new("llmenv-no-such-indexer");
        assert!(run_cbm_index_with(&file, |_, _, _| missing()).is_err());
        assert_eq!(std::fs::read_to_string(&result).unwrap(), "the old result");
        assert!(!pending_result_path(&result).exists());
    }

    #[test]
    fn the_last_result_stays_until_the_indexer_exits() {
        let dir = tempfile::tempdir().unwrap();
        let (file, result) = with_result(dir.path());
        std::fs::write(&result, "the old result").unwrap();
        let seen = std::cell::RefCell::new(String::new());
        let watch = format!("cat '{}' > /dev/null; echo '{{}}'", result.display());
        run_cbm_index_with(&file, |_, _, _| {
            // While the indexer is being built the old result is still in place.
            *seen.borrow_mut() = std::fs::read_to_string(&result).unwrap();
            shell(&watch)
        })
        .unwrap();
        assert_eq!(*seen.borrow(), "the old result");
        assert_eq!(std::fs::read_to_string(&result).unwrap().trim(), "{}");
    }

    #[test]
    fn a_result_file_is_owner_only_and_replaces_a_symlink_instead_of_following_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim");
        std::fs::write(&target, "keep me").unwrap();
        let link = dir.path().join("result.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let file = crate::hook_run::create_result_file(&link).unwrap();
        drop(file);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_checkpoint_from_before_the_result_file_still_loads() {
        let value = serde_json::json!({ "project_root": "/repo", "index_path": null });
        let inputs: IndexInputs = serde_json::from_value(value).unwrap();
        assert_eq!((inputs.mem_budget_mb, inputs.result_path), (None, None));
    }

    #[test]
    fn a_checkpoint_without_a_project_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint::new(JobKind::CbmIndex, serde_json::json!({}), None);
        let file = checkpoint::write(dir.path(), &cp).unwrap().unwrap();
        assert!(run_cbm_index_with(&file, |_, _, _| shell("exit 0")).is_err());
        assert!(file.exists());
    }
}
