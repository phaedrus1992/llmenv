//! The recall time budget lives in `llmenv-mcp`, because `McpHttpClient` holds a `Deadline`.

pub(crate) use llmenv_mcp::recall_deadline::{Cut, Deadline, within};
