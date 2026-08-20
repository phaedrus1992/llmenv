pub mod proxy {
    pub use llmenv_mcp::proxy::{
        EnsureOutcome, default_pid_path, detach_process_group, ensure_running,
        ensure_running_within, is_alive, open_bounded_log, probe_tcp, spawn_mcp_proxy,
    };
}

pub mod resolve {
    pub use llmenv_mcp::resolve::{
        CODEBASE_MEMORY_MCP_NAME, MEMORY_MCP_NAME, ResolveError, ResolvedKind, ResolvedMcp,
        codebase_memory_paths, memory_is_tag_active, resolve_bundle_mcps,
        resolve_codebase_memory_entries, resolve_mcps,
    };
}
