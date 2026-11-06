//! Test mocks that answer a recall in the JSON form that icm 0.11.5 gives (#3369).

/// A JSON recall reply with one record for each `[<topic>] <summary>` record of `text`. The id
/// is the text hash, so two records with the same text share an id, as one icm record does.
pub(crate) fn recall_json(text: &str) -> String {
    let records: Vec<serde_json::Value> = crate::hook_run::action::split_recall_records(text)
        .into_iter()
        .filter_map(|record| {
            let (topic, summary) = record.strip_prefix('[')?.split_once("] ")?;
            Some(serde_json::json!({
                "id": crate::hook_run::session_ledger::record_hash(&record),
                "topic": topic,
                "summary": summary,
                "importance": "medium",
            }))
        })
        .collect();
    serde_json::Value::Array(records).to_string()
}

/// A JSON-RPC tool reply whose text is `body`, or [`recall_json`] of `body` when the request
/// asks for the JSON recall form and `body` is not JSON already.
#[derive(Debug, Clone)]
pub(crate) struct ToolReply(pub(crate) String);

impl wiremock::Respond for ToolReply {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let asked_json = String::from_utf8_lossy(&request.body).contains(r#""format":"json""#);
        let is_json = serde_json::from_str::<Vec<serde_json::Value>>(&self.0).is_ok();
        let text = if asked_json && !is_json {
            recall_json(&self.0)
        } else {
            self.0.clone()
        };
        wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"content": [{"type": "text", "text": text}]},
        }))
    }
}
