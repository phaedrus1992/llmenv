//! Checks of the `cache` section and the transcript retention.

use super::{Config, ValidateError};
use llmenv_paths::has_parent_component;

/// The cache folder, the two retention ages, and the transcript retention.
pub(super) fn validate_cache(config: &Config) -> Result<(), ValidateError> {
    if !is_safe_cache_dir(&config.cache.cache_dir) {
        return Err(ValidateError::CacheDirTraversal(
            config.cache.cache_dir.clone(),
        ));
    }
    if config.cache.cache_retention_hours == Some(0) {
        return Err(ValidateError::CacheRetentionInvalid);
    }
    if config.cache.tmp_retention_hours == Some(0) {
        return Err(ValidateError::TmpRetentionInvalid);
    }
    if let Some(days) = config
        .session_log
        .as_ref()
        .and_then(|s| s.transcript.as_ref())
        .and_then(|t| t.retention_days)
        && days == 0
    {
        return Err(ValidateError::RetentionDaysInvalid);
    }
    Ok(())
}

pub(super) fn is_safe_cache_dir(dir: &str) -> bool {
    if dir.is_empty() || dir.len() > 4096 {
        return false;
    }
    // Parse components rather than substring-match so traversal can't slip
    // through as `foo/..` (no trailing slash) or via host-OS separators.
    !dir.contains('\0') && !has_parent_component(dir)
}
