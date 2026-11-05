//! The per-turn recall reads the session's own project and a fixed list of global topics, and
//! no other project (#2950).
//!
//! A recall with no project filter returns the records of every project, so a fact about one
//! repository reached the turns of another. The topics that hold facts for every project, such as
//! `preferences`, are named in `features.memory[].global_topics`, and only they are read across
//! projects.

use crate::hook_run::action::{Action, RecallQuery, Reply};
use crate::hook_run::mcp_client::McpHttpClient;

/// The records that one global topic recall returns at most, as the main recall of a turn.
const TOPIC_LIMIT: u8 = 10;

/// The recall of each global topic for `text`, across every project.
fn topic_queries(text: &str, topics: &[String], limit: u8) -> Vec<RecallQuery> {
    topics
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|topic| RecallQuery {
            query: text.to_string(),
            topic: Some(topic.to_string()),
            keyword: None,
            project: Some(String::new()),
            limit,
        })
        .collect()
}

/// The records of the global topics, one call per topic. `None` with no topic, an error when
/// every call failed. A call that failed beside one that answered gets an agent notice, because
/// the records of that topic are missing from the turn.
pub(crate) async fn recall(
    client: &McpHttpClient,
    text: &str,
    topics: &[String],
    limit: u8,
) -> Option<anyhow::Result<Reply>> {
    let queries = topic_queries(text, topics, limit);
    if queries.is_empty() {
        return None;
    }
    let mut records = Vec::new();
    let mut answered = 0usize;
    let mut failed = Vec::new();
    for q in queries {
        let topic = q.topic.clone().unwrap_or_default();
        let found = Action::RecallQuery(q).run(client, "", "").await;
        match found.and_then(Reply::into_records) {
            Ok(found) => {
                answered += 1;
                records.extend(found);
            }
            Err(e) => {
                tracing::error!("memory recall of the global topic {topic} failed: {e:#}");
                failed.push((topic, e));
            }
        }
    }
    if answered == 0 {
        let names: Vec<&str> = failed.iter().map(|(t, _)| t.as_str()).collect();
        let causes: Vec<String> = failed.iter().map(|(t, e)| format!("{t}: {e:#}")).collect();
        return Some(Err(anyhow::anyhow!(
            "memory recall of the global topics {} failed: {}",
            names.join(", "),
            causes.join("; ")
        )));
    }
    for (topic, _) in &failed {
        crate::agent_notice::record(partial_notice(topic));
    }
    Some(Ok(Reply::Records(records)))
}

/// The notice for one global topic whose recall failed while another topic answered.
fn partial_notice(topic: &str) -> String {
    format!(
        "llmenv: the memories of the global topic `{topic}` were skipped this turn: its icm recall \
         failed. Fix: run `llmenv doctor` to check the memory server."
    )
}

/// The turn recall actions of the stateless path: an empty `Recall` reads every project, so it is
/// replaced by the global topic recalls, and a project recall gets them beside it.
pub(crate) fn turn_actions(actions: Vec<Action>, text: &str, topics: &[String]) -> Vec<Action> {
    let had_recall = actions.iter().any(|a| matches!(a, Action::Recall(_)));
    let mut out: Vec<Action> = actions
        .into_iter()
        .filter(|a| !matches!(a, Action::Recall(project) if project.is_empty()))
        .collect();
    if had_recall {
        out.extend(
            topic_queries(text, topics, TOPIC_LIMIT)
                .into_iter()
                .map(Action::RecallQuery),
        );
    }
    out
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests;
