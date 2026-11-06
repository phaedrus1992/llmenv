//! The codebase-memory indexer process: its command, the job wrapper, and the result file
//! (#365, #2154, #2396).

use super::session_state;

/// Builds the `codebase-memory-mcp cli index_repository` subprocess command
/// for `project_root`, without spawning it — kept separate so tests can
/// assert on the command shape without launching a real process (#365).
///
/// `CBM_CACHE_DIR` is only set when `cm.index_path` is explicit (#1493);
/// `CBM_ALLOWED_ROOT` is never set (#1495). `CBM_MEM_BUDGET_MB` is set when
/// `cm.mem_budget_mb` is (#2154). Same treatment as `resolve_codebase_memory`
/// (`src/mcp/resolve.rs`), which this mirrors so the SessionStart auto-index
/// and the MCP server launch agree on scope and budget.
/// The indexer command for `project_root`. The checkpointed wrapper (`detached_cbm`) rebuilds it
/// from a checkpoint file (#2396).
///
/// `index_path` MUST already be expanded (see [`expanded_index_path`]). A literal `~` would reach
/// the child as a directory name.
pub(super) fn index_command(
    program: &std::path::Path,
    project_root: &std::path::Path,
    index_path: Option<&str>,
    mem_budget_mb: Option<u32>,
) -> std::process::Command {
    // repo_path must become JSON text regardless (the CLI arg is a JSON
    // string), so this lossy step is unavoidable here — unlike the env var
    // below, which can carry the raw OsStr straight through.
    let args_json =
        serde_json::json!({ "repo_path": project_root.display().to_string() }).to_string();
    let mut cmd = std::process::Command::new(program);
    cmd.args(["cli", "index_repository", &args_json])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(index_path) = index_path {
        cmd.env("CBM_CACHE_DIR", index_path);
    }
    if let Some(budget) = mem_budget_mb {
        cmd.env("CBM_MEM_BUDGET_MB", budget.to_string());
    }
    cmd
}

/// Where the last index result of `project_root` is kept, under `cache_dir`. The cache directory
/// is shared by every project, so the file name carries a stable key of the project root. The
/// writer and `llmenv doctor` both call this (#2154).
pub(crate) fn index_result_path(
    cache_dir: &std::path::Path,
    project_root: &std::path::Path,
) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        project_root.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = project_root.to_string_lossy().into_owned().into_bytes();
    let digest = Sha256::digest(&bytes);
    let key: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    cache_dir.join(format!("index-result-{key}.json"))
}

/// Send the stdout of an indexer that `cmd` starts directly to the result file. When `cmd` is the
/// wrapper it does this for the indexer it starts (#2154).
pub(super) fn direct_index_stdout(
    cmd: &mut std::process::Command,
    wrapped: bool,
    result_path: &std::path::Path,
) {
    if !wrapped {
        cmd.stdout(result_stdout(result_path));
    }
}

/// Create the result file at `path`, owner-only and new. An existing file or symlink at `path` is
/// removed first, so the open can neither follow a link nor reuse a file that another user made.
pub(super) fn create_result_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // A failed removal shows below: `create_new` then fails on what is still there.
    if let Some(e) = session_state::removal_error_to_log(std::fs::remove_file(path)) {
        crate::agent_notice::record_logged(format!(
            "llmenv: cannot remove the old index result file {}, so the new result cannot be \
             saved: {e}. Fix: remove the file.",
            path.display()
        ));
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// The stdout for an indexer that reports its result as JSON: the result file, or null when the
/// file cannot be created (a missing result is a smaller problem than skipping the index).
pub(super) fn result_stdout(path: &std::path::Path) -> std::process::Stdio {
    match create_result_file(path) {
        Ok(file) => std::process::Stdio::from(file),
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: cannot create the index result file {}, so doctor cannot report the \
                 result: {e}. Fix: check that the state folder is writable.",
                path.display()
            ));
            std::process::Stdio::null()
        }
    }
}

/// Whether an index job runs through the `cbm-index-run` wrapper: it needs a checkpoint and an
/// executable to run it with (#2396).
pub(super) fn uses_wrapper(
    checkpoint: Option<&std::path::Path>,
    exe: Option<&std::path::Path>,
) -> bool {
    checkpoint.is_some() && exe.is_some()
}

/// The process that runs one index job: `llmenv cbm-index-run` when there is a checkpoint and an
/// executable to run it with, else the indexer itself (`direct`) (#2396).
pub(super) fn index_job_command(
    checkpoint: Option<&std::path::Path>,
    exe: Option<std::path::PathBuf>,
    direct: impl FnOnce() -> std::process::Command,
) -> std::process::Command {
    match (checkpoint, exe) {
        (Some(path), Some(exe)) => {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("cbm-index-run")
                .arg("--checkpoint")
                .arg(path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null());
            cmd
        }
        _ => direct(),
    }
}

#[cfg(all(test, unix))]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn a_result_file_that_cannot_be_made_is_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        // A folder with an entry at the result path can be neither removed as a file nor
        // created as one (#3449).
        let path = dir.path().join("index-result-3449.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        let _stdout = result_stdout(&path);
        let notices = crate::agent_notice::current().unwrap_or_default();
        assert!(
            notices.contains("cannot remove the old index result file"),
            "{notices}"
        );
        assert!(
            notices.contains("cannot create the index result file"),
            "{notices}"
        );
        assert!(notices.contains("index-result-3449.json"), "{notices}");
    }
}
