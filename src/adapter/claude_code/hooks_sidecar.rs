//! The hooks sidecar that reconcile reads back, so it can tell owned hooks from foreign ones (#991).

use std::path::Path;

/// Read the previously-rendered hooks sidecar. Returns `None` when absent or
/// unparseable — reconcile then falls back to union-only behavior.
pub(super) fn read_hooks_sidecar(path: &Path) -> Option<serde_json::Value> {
    // A missing sidecar is the normal first-run case. Any other failure makes reconcile
    // union-only, so it is logged for the user (#2721).
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: cannot read hooks sidecar {}, so stale hooks are not removed ({e}). \
                 Fix: check the file's permissions, or delete it to start fresh.",
                path.display()
            ));
            return None;
        }
    };
    serde_json::from_slice(&bytes)
        .inspect_err(|e| {
            tracing::error!(
                "hooks sidecar {} is not valid JSON, hooks are not reconciled: {e}",
                path.display()
            );
        })
        .ok()
}
