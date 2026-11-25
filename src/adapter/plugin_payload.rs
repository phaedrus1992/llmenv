//! The payload directory of a plugin.

use std::path::PathBuf;

/// Resolve the on-disk payload directory for a plugin.
///
/// External plugins (`install_path = Some`) use that path directly.
/// First-party plugins look up their marketplace `install_location`.
///
/// Shared across adapters: previously lived in `crush.rs` and was
/// cross-imported by opencode via `super::crush::resolve_plugin_payload`.
pub(super) fn resolve_plugin_payload(
    plugin: &crate::plugins::resolve::ResolvedPlugin,
    marketplaces: &[crate::plugins::resolve::ResolvedMarketplace],
) -> anyhow::Result<PathBuf> {
    // P2-5/#534: guard before any path join, regardless of which path is taken.
    if !crate::paths::is_valid_short_name(&plugin.plugin) {
        anyhow::bail!("plugin name '{}' is not a valid name", plugin.plugin);
    }
    if let Some(p) = &plugin.install_path {
        return Ok(PathBuf::from(p));
    }
    let mkt = marketplaces
        .iter()
        .find(|m| m.name == plugin.marketplace)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "plugin '{}': marketplace '{}' not found in resolved marketplaces",
                plugin.plugin,
                plugin.marketplace
            )
        })?;
    let install_location = mkt.install_location.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "plugin '{}': marketplace '{}' has no install_location (not yet synced?)",
            plugin.plugin,
            plugin.marketplace
        )
    })?;
    Ok(PathBuf::from(install_location).join(&plugin.plugin))
}
