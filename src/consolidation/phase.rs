//! The checkpoint steps of a consolidation run, and its notice for the agent.

use crate::hook_run::checkpoint;

/// Record the phase (and the model summary, once there is one) in the checkpoint. Fail-soft.
pub(super) fn note_phase(checkpoint: Option<&std::path::Path>, phase: &str, summary: Option<&str>) {
    let Some(path) = checkpoint else {
        return;
    };
    let result = checkpoint::update(path, |c| {
        c.phase = phase.to_string();
        if let Some(summary) = summary {
            c.inputs["summary"] = serde_json::json!(summary);
        }
    });
    if let Err(e) = result {
        notice(&format!(
            "consolidation: cannot update the checkpoint: {e:#}"
        ));
    }
}

/// The model summary a previous run saved, when this run resumes after the model step.
pub(super) fn saved_summary(checkpoint: Option<&std::path::Path>) -> Option<String> {
    let loaded = checkpoint::load(checkpoint?)
        .inspect_err(|e| {
            notice(&format!(
                "consolidation: the model summary is lost, so the run asks again: {e:#}"
            ));
        })
        .ok()?;
    if loaded.phase != "summarized" {
        return None;
    }
    loaded.inputs["summary"].as_str().map(str::to_string)
}

/// Delete the checkpoint of a run with nothing left to retry. Fail-soft.
pub(super) fn settle(checkpoint: Option<&std::path::Path>) {
    if let Some(path) = checkpoint
        && let Err(e) = checkpoint::complete(path)
    {
        notice(&format!("consolidation: {e:#}"));
    }
}

/// Record a consolidation failure for the agent. The job runs as a detached child, so the
/// notice reaches the agent through the notice spool (#3398).
pub(super) fn notice(msg: &str) {
    crate::agent_notice::record_logged(format!(
        "llmenv: {msg}. Fix: run `llmenv doctor` to check the memory server and the consolidation \
         backend."
    ));
}
