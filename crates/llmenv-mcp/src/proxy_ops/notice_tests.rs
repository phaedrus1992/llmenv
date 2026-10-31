use super::*;

/// #3204: an attribution line that cannot be written names its log for the agent.
#[test]
fn unwritable_attribution_log_is_recorded_for_the_agent() {
    let dir = tempfile::tempdir().expect("test");
    let pid_path = dir.path().join("mcp-proxy.pid");
    // A directory at the log path makes the append fail.
    let log_path = crate::proxy::log_path_for(&pid_path);
    std::fs::create_dir(&log_path).expect("test");
    record_spawn(&pid_path, 4242, SpawnSource::Export);
    let notices = llmenv_util::agent_notice::current().unwrap_or_default();
    assert!(
        notices.contains(&log_path.display().to_string()),
        "a failed attribution write must name the log: {notices}"
    );
}
