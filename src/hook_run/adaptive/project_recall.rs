//! The session project's own memories, recalled for the `SessionStart` wake-up pack.

use crate::agent_notice;
use crate::hook_run::action::{Action, RecallQuery, Reply};
use crate::hook_run::mcp_client::McpHttpClient;

use super::cut::{drop_cut_items, note_cut_items};
use super::mcp_health;
use super::{MAIN_LIMIT, SCOPE_RECORD, query};

/// The session project's own memories for the wake-up pack, and whether the call answered. The
/// icm wake-up fills its budget with global records first, so this project-scoped recall runs
/// beside it (#2756).
pub(super) async fn project_memories(client: &McpHttpClient, project: &str) -> (String, bool) {
    let q = RecallQuery {
        project: Some(project.to_string()),
        ..query(project, MAIN_LIMIT)
    };
    let answered = Action::RecallQuery(q).run(client, "", "").await;
    match answered.and_then(Reply::into_records) {
        Ok(records) => {
            let records: Vec<String> = records
                .into_iter()
                .map(|r| r.text)
                .filter(|r| !r.starts_with(SCOPE_RECORD))
                .collect();
            let (kept, cut) = drop_cut_items(&records.join("\n"));
            note_cut_items(cut);
            (kept, true)
        }
        Err(e) => {
            tracing::error!("session start: project memories not recalled for {project}: {e:#}");
            agent_notice::record(format!(
                "llmenv: project memories were skipped at session start: icm recall failed ({}). \
                 Fix: run `llmenv doctor` to check the memory server.",
                mcp_health::tidy_reason(&format!("{e:#}"))
            ));
            (String::new(), false)
        }
    }
}
