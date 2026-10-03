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
    build: impl FnOnce(&Path, Option<&str>) -> std::process::Command,
) -> anyhow::Result<()> {
    let loaded = checkpoint::load(checkpoint_path)?;
    let inputs: IndexInputs = serde_json::from_value(loaded.inputs)
        .context("the checkpoint holds no project root for the indexer")?;
    let mut cmd = build(
        Path::new(&inputs.project_root),
        inputs.index_path.as_deref(),
    );
    // The parent points this process's stderr at the index log; the indexer shares it.
    cmd.stderr(std::process::Stdio::inherit());
    let status = cmd.status().context("cannot start codebase-memory-mcp")?;
    anyhow::ensure!(status.success(), "codebase-memory-mcp exited with {status}");
    checkpoint::complete(checkpoint_path)
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
        ) {
            let inputs = IndexInputs { project_root: root, index_path: index };
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
        run_cbm_index_with(&file, |_, _| shell("exit 0")).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn a_non_zero_exit_leaves_the_checkpoint_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = written(dir.path());
        let err = run_cbm_index_with(&file, |_, _| shell("exit 3")).unwrap_err();
        assert!(err.to_string().contains("exited"), "{err}");
        assert!(file.exists());
    }

    #[test]
    fn the_indexer_gets_the_project_root_and_index_path_from_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = serde_json::to_value(IndexInputs {
            project_root: "/repo".into(),
            index_path: Some("/idx".into()),
        })
        .unwrap();
        let file = checkpoint::write(
            dir.path(),
            &Checkpoint::new(JobKind::CbmIndex, inputs, None),
        )
        .unwrap()
        .unwrap();
        let mut seen = None;
        run_cbm_index_with(&file, |root, idx| {
            seen = Some((root.to_path_buf(), idx.map(str::to_string)));
            shell("exit 0")
        })
        .unwrap();
        assert_eq!(seen, Some(("/repo".into(), Some("/idx".to_string()))));
    }

    #[test]
    fn a_checkpoint_without_a_project_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint::new(JobKind::CbmIndex, serde_json::json!({}), None);
        let file = checkpoint::write(dir.path(), &cp).unwrap().unwrap();
        assert!(run_cbm_index_with(&file, |_, _| shell("exit 0")).is_err());
        assert!(file.exists());
    }
}
