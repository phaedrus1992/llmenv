//! The detached `llmenv consolidation-run` child that a session end starts (#1133, #2396).

use super::{checkpoint, detached_child_log_path, redirect_stderr_to_detached_log};

/// The `llmenv consolidation-run` child, before its stderr log and process
/// group are set.
pub(super) fn consolidation_run_command(
    exe: std::path::PathBuf,
    checkpoint: Option<&std::path::Path>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("consolidation-run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    if let Some(path) = checkpoint {
        cmd.arg("--checkpoint").arg(path);
    }
    cmd
}

/// Spawn a detached child to run post-session consolidation. Best-effort
/// fire-and-forget — spawn failures are logged at error level and the caller
/// never waits on the child. The child's stderr goes to the shared bounded log
/// rather than `/dev/null` so its own failures are diagnosable (#1133).
pub(super) fn post_session_consolidation() {
    post_session_consolidation_in(checkpoint::spawner_state_dir().as_deref());
}

/// [`post_session_consolidation`] with the state dir for the checkpoint given (#2396). The
/// checkpoint holds the working directory, because the child derives the project from it and a
/// resumed run must start in the same place.
pub(super) fn post_session_consolidation_in(state_dir: Option<&std::path::Path>) {
    // Spawns this same binary as a detached child. The path is this process's own image, not input.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: memory consolidation is skipped, because llmenv cannot find its own \
                 program: {e}. Fix: run `llmenv doctor`."
            ));
            return;
        }
    };
    // The run tag keeps two sessions that end in one directory from sharing a checkpoint file, and
    // it scopes the rule ids to this run (#2397).
    let checkpoint = checkpoint::begin(
        state_dir,
        checkpoint::JobKind::Consolidation,
        &serde_json::json!({ "run_tag": checkpoint::run_tag() }),
        None,
    );
    let mut cmd = consolidation_run_command(exe, checkpoint.as_deref());
    redirect_stderr_to_detached_log(&mut cmd, detached_child_log_path());
    crate::mcp::proxy::detach_process_group(&mut cmd);
    if let Err(e) = crate::trace::spawn_counted("consolidation:run", &mut cmd) {
        // The checkpoint written above stays, so a later session can run the job again (#3506).
        crate::agent_notice::record_logged(format!(
            "llmenv: memory consolidation is skipped, because its job cannot start: {e}. Its \
             checkpoint stays, so a later session can run it again. Fix: run `llmenv doctor`."
        ));
    }
    // Not waited on: the child is process-group-detached and outlives us.
}
