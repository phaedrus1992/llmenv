//! Child processes with a deadline. The helper lives in `llmenv_mcp`, so the MCP crate and the
//! root crate share one copy.

pub(crate) use llmenv_mcp::process::run_with_timeout;
