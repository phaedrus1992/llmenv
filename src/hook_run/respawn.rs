//! The restart of a background job from its checkpoint, in the folder of its first run.

use std::io::Write as _;

use anyhow::Context as _;

use super::checkpoint;
use super::{
    consolidation_run_command, detached_child_log_path, icm_store_command,
    redirect_stderr_to_detached_log,
};

/// Start the child for one stale checkpoint, with the inputs and the working directory of the
/// first run. The memory backend and the project come from that directory, so a checkpoint whose
/// directory is gone is not run: it would store into the project of the session that resumes it.
pub(super) fn respawn_job(
    cp: &checkpoint::Checkpoint,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    respawn_job_with_exe(cp, path, std::env::current_exe)
}

/// [`respawn_job`] with the image lookup passed in, so a test can make it fail.
fn respawn_job_with_exe(
    cp: &checkpoint::Checkpoint,
    path: &std::path::Path,
    current_exe: impl FnOnce() -> std::io::Result<std::path::PathBuf>,
) -> anyhow::Result<()> {
    use checkpoint::JobKind;
    let cwd = cp.cwd.as_deref().map(std::path::Path::new).ok_or_else(|| {
        anyhow::anyhow!("the working directory of the first run was not recorded")
    })?;
    // A stat error that is not "gone" can be temporary, so name the real cause.
    match std::fs::metadata(cwd) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => anyhow::bail!(
            "the working directory of the first run ({}) is not a folder",
            cwd.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "the working directory of the first run ({}) is gone",
            cwd.display()
        ),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "cannot use the working directory of the first run ({})",
                    cwd.display()
                )
            });
        }
    }
    // Spawns this same binary as a detached child. The path is this process's own image, not input.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    let exe = current_exe().context("cannot resolve current_exe")?;
    let stdin = cp.inputs.to_string();
    match cp.kind {
        JobKind::IcmStore => spawn_detached(icm_store_command(exe, Some(path)), Some(&stdin), cwd),
        JobKind::SessionLogRecord => spawn_detached(
            crate::session_log::detached::record_command(exe, Some(path)),
            Some(&stdin),
            cwd,
        ),
        JobKind::Consolidation => {
            spawn_detached(consolidation_run_command(exe, Some(path)), None, cwd)
        }
        // Not resumable: `resume_pending` skips it, because `SessionStart` starts the indexer.
        JobKind::CbmIndex => Ok(()),
    }
}

/// Spawn `cmd` as a detached child in `cwd` with the shared bounded stderr log, and write `stdin`
/// to it. Not waited on.
pub(crate) fn spawn_detached(
    mut cmd: std::process::Command,
    stdin: Option<&str>,
    cwd: &std::path::Path,
) -> anyhow::Result<()> {
    cmd.current_dir(cwd);
    redirect_stderr_to_detached_log(&mut cmd, detached_child_log_path());
    crate::mcp::proxy::detach_process_group(&mut cmd);
    let mut child = crate::trace::spawn_counted("respawn:job", &mut cmd)?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spawn_detached_runs_in_the_directory_and_feeds_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "cat > out.txt; pwd > cwd.txt"])
            .stdin(std::process::Stdio::piped());
        spawn_detached(cmd, Some("payload"), dir.path()).unwrap();
        let out = dir.path().join("out.txt");
        for _ in 0..200 {
            // The shell creates the file before `pwd` writes it, so wait for content.
            let written = std::fs::read_to_string(dir.path().join("cwd.txt"))
                .is_ok_and(|text| !text.is_empty());
            if written {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(std::fs::read_to_string(out).unwrap(), "payload");
        let cwd = std::fs::read_to_string(dir.path().join("cwd.txt")).unwrap();
        assert_eq!(
            std::path::Path::new(cwd.trim()).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn a_job_whose_working_directory_is_gone_is_not_resumed() {
        let mut cp = checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, json!({}), None);
        cp.cwd = Some("/definitely/not/a/directory".into());
        let err = respawn_job(&cp, std::path::Path::new("/s/checkpoints/x.json")).unwrap_err();
        assert!(err.to_string().contains("is gone"), "{err}");
        cp.cwd = None;
        let err = respawn_job(&cp, std::path::Path::new("/s/checkpoints/x.json")).unwrap_err();
        assert!(err.to_string().contains("not recorded"), "{err}");
    }

    #[test]
    fn a_job_whose_image_cannot_be_resolved_is_a_failed_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut cp = checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, json!({}), None);
        cp.cwd = Some(dir.path().display().to_string());
        let err = respawn_job_with_exe(&cp, std::path::Path::new("/s/x.json"), || {
            Err(std::io::Error::other("no image"))
        })
        .expect_err("a failed lookup is a failed start, which uses an attempt");
        assert!(
            format!("{err:#}").contains("cannot resolve current_exe"),
            "{err:#}"
        );
    }

    #[test]
    fn a_working_directory_that_is_a_file_is_not_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let mut cp = checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, json!({}), None);
        cp.cwd = Some(file.display().to_string());
        let err = respawn_job(&cp, std::path::Path::new("/s/x.json")).unwrap_err();
        assert!(err.to_string().contains("is not a folder"), "{err}");
    }

    #[test]
    fn a_working_directory_that_cannot_be_read_names_the_cause() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir_all(locked.join("cwd")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut cp = checkpoint::Checkpoint::new(checkpoint::JobKind::IcmStore, json!({}), None);
        cp.cwd = Some(locked.join("cwd").display().to_string());
        let err = respawn_job(&cp, std::path::Path::new("/s/x.json")).unwrap_err();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        let text = format!("{err:#}");
        assert!(text.contains("cannot use the working directory"), "{text}");
        assert!(
            !text.contains("is gone"),
            "a stat error is not a gone folder: {text}"
        );
    }
}
