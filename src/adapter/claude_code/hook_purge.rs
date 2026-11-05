//! Removal of hooks that llmenv or a disabled plugin wrote into `settings.json`.

use super::normalized_hook;

/// Drop from the on-disk `existing` hooks doc any entry that llmenv rendered in
/// the previous round (`prev_owned`) but is not rendering this round (`fresh`).
///
/// This is the owned-vs-foreign distinction (#991): an entry present in
/// `prev_owned` was llmenv's, so if it's gone from `fresh` the user removed it
/// from config and it must be purged rather than preserved by the union. A hook
/// never in `prev_owned` is foreign (a plugin self-registered it) and is left
/// for the union to keep. No-op when there's no sidecar (`prev_owned` is None).
pub(super) fn purge_stale_owned_hooks(
    existing: &mut serde_json::Value,
    prev_owned: Option<&serde_json::Value>,
    fresh: &serde_json::Value,
) {
    let (Some(prev), Some(existing_obj)) = (
        prev_owned.and_then(|v| v.as_object()),
        existing.as_object_mut(),
    ) else {
        return;
    };
    for (event, prev_entries) in prev {
        let Some(prev_arr) = prev_entries.as_array() else {
            continue;
        };
        let fresh_norm: Vec<serde_json::Value> = fresh
            .get(event)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(normalized_hook).collect())
            .unwrap_or_default();
        let stale: Vec<serde_json::Value> = prev_arr
            .iter()
            .map(normalized_hook)
            .filter(|e| !fresh_norm.contains(e))
            .collect();
        if stale.is_empty() {
            continue;
        }
        if let Some(arr) = existing_obj.get_mut(event).and_then(|v| v.as_array_mut()) {
            arr.retain(|e| !stale.contains(&normalized_hook(e)));
        }
    }
}
