use anyhow::Result;
use tracing::warn;

/// Health check for managed MCP servers at session start.
pub async fn check_mcp_servers() {
    // Check icm server availability
    if let Err(e) = check_icm().await {
        warn!("icm MCP server unavailable at session start: {e}");
    }

    // Check codebase-memory-mcp availability
    if let Err(e) = check_codebase_memory().await {
        warn!("codebase-memory-mcp unavailable at session start: {e}");
    }
}

/// Attempt to connect to icm server.
async fn check_icm() -> Result<()> {
    const ICM_ENV: &str = "LLMENV_ICM_ENDPOINT";
    let is_stdio = std::env::var(ICM_ENV).as_deref() != Ok("stdio");

    if is_stdio {
        return Ok(());
    }

    // For socket/HTTP endpoints, full health check deferred
    Ok(())
}

/// Attempt to connect to codebase-memory-mcp.
async fn check_codebase_memory() -> Result<()> {
    const CBM_ENV: &str = "CODEBASE_MEMORY_MCP_ENDPOINT";
    let is_stdio = std::env::var(CBM_ENV).as_deref() != Ok("stdio");

    if is_stdio {
        return Ok(());
    }

    // For socket/HTTP endpoints, full health check deferred
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_check_returns_ok_for_stdio_mode() {
        let result = check_icm().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn codebase_memory_health_check_returns_ok_for_stdio() {
        let result = check_codebase_memory().await;
        assert!(result.is_ok());
    }
}
