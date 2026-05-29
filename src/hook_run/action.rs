//! The memory action each lifecycle event performs, and how it maps to an ICM
//! MCP tool call.

use serde_json::{Value, json};

use crate::hook_run::mcp_client::McpHttpClient;

/// One memory action against the ICM MCP backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Inject the session wake-up pack (`icm_wake_up`).
    WakeUp,
    /// Inject recalled context for the active tags/project (`icm_memory_recall`).
    Recall,
    /// Best-effort store of the active scope context (`icm_memory_store`).
    Store,
}

impl Action {
    /// The ICM MCP tool this action calls.
    pub fn tool_name(self) -> &'static str {
        match self {
            Action::WakeUp => "icm_wake_up",
            Action::Recall => "icm_memory_recall",
            Action::Store => "icm_memory_store",
        }
    }

    /// Build the `arguments` object for this action's tool call. `query` is the
    /// recall query (active tags/project), `chunk` is the llmenv context chunk
    /// used as store content. Unused fields are ignored per action.
    pub fn arguments(self, query: &str, chunk: &str) -> Value {
        match self {
            Action::WakeUp => json!({}),
            Action::Recall => json!({ "query": query }),
            Action::Store => json!({ "content": chunk }),
        }
    }

    /// Execute the action: call the tool and return its text result.
    ///
    /// # Errors
    /// Propagates any client/network error from `call_tool`.
    pub async fn run(
        self,
        client: &McpHttpClient,
        query: &str,
        chunk: &str,
    ) -> anyhow::Result<String> {
        client
            .call_tool(self.tool_name(), self.arguments(query, chunk))
            .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn action_tool_name_mapping() {
        assert_eq!(Action::WakeUp.tool_name(), "icm_wake_up");
        assert_eq!(Action::Recall.tool_name(), "icm_memory_recall");
        assert_eq!(Action::Store.tool_name(), "icm_memory_store");
    }

    #[test]
    fn wakeup_arguments_are_empty_object() {
        let args = Action::WakeUp.arguments("query text", "chunk text");
        assert_eq!(args, serde_json::json!({}));
    }

    #[test]
    fn recall_arguments_carry_query() {
        let args = Action::Recall.arguments("rust, work", "chunk");
        assert_eq!(args["query"], serde_json::json!("rust, work"));
    }

    #[test]
    fn store_arguments_carry_content() {
        let args = Action::Store.arguments("query", "## llmenv context\n...");
        assert_eq!(args["content"], serde_json::json!("## llmenv context\n..."));
    }
}
