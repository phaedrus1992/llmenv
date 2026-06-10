//! Auth sync detection loop for `run_export`.
//!
//! On every `export` call, compares the `oauthAccount` UUID in the materialized
//! `.claude.json` against what the manifest recorded. When they differ (e.g. the
//! user ran `claude auth login` inside an active session), the stable cache is
//! refreshed and the manifest is updated so the next export sees the change.

use std::path::Path;

use crate::materialize::manifest::{AuthSource, AuthStatus, CacheManifest};

/// Detect and sync an in-session login change. Infallible from the caller's
/// perspective — all errors are traced at debug and swallowed so `run_export`
/// never fails because of auth sync.
pub fn sync_auth_on_export(config_dir: &Path, adapter_root: &Path, manifest: &mut CacheManifest) {
    if let Err(e) = try_sync(config_dir, adapter_root, manifest) {
        tracing::debug!("auth sync (non-fatal): {e}");
    }
}

fn try_sync(
    config_dir: &Path,
    adapter_root: &Path,
    manifest: &mut CacheManifest,
) -> anyhow::Result<()> {
    let Some(entry) = super::read_auth_from_dir(config_dir)? else {
        return Ok(());
    };
    let recorded_id = manifest.auth_status.id.as_deref().unwrap_or("");
    if entry.uuid == recorded_id {
        return Ok(());
    }
    super::save_auth_entry(adapter_root, &entry)?;
    eprintln!("[llmenv] auth: {} (session login detected)", entry.email);
    manifest.auth_status = AuthStatus {
        source: AuthSource::Inherited,
        id: Some(entry.uuid),
        email: Some(entry.email),
    };
    manifest.write(config_dir)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::materialize::manifest::CacheManifest;

    fn make_manifest(uuid: Option<&str>) -> CacheManifest {
        let mut m = CacheManifest::new("hash", vec![PathBuf::from("CLAUDE.md")]);
        if let Some(id) = uuid {
            m.auth_status = AuthStatus {
                source: AuthSource::Inherited,
                id: Some(id.to_string()),
                email: Some("existing@test.com".to_string()),
            };
        }
        m
    }

    fn write_claude_json(dir: &std::path::Path, uuid: &str, email: &str) {
        let doc = serde_json::json!({
            "oauthAccount": { "id": uuid, "emailAddress": email }
        });
        std::fs::write(
            dir.join(".claude.json"),
            serde_json::to_string(&doc).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn noop_when_uuids_match() {
        let tmp = tempfile::tempdir().unwrap();
        let uuid = "aaaa0000-0000-0000-0000-000000000000";
        write_claude_json(tmp.path(), uuid, "same@test.com");
        let mut manifest = make_manifest(Some(uuid));
        let original_source = manifest.auth_status.source;
        sync_auth_on_export(tmp.path(), tmp.path(), &mut manifest);
        assert_eq!(manifest.auth_status.source, original_source);
    }

    #[test]
    fn updates_cache_when_uuid_differs() {
        let config_tmp = tempfile::tempdir().unwrap();
        let cache_tmp = tempfile::tempdir().unwrap();
        let new_uuid = "bbbb1111-0000-0000-0000-000000000000";
        write_claude_json(config_tmp.path(), new_uuid, "new@test.com");
        let mut manifest = make_manifest(Some("old-uuid-0000-0000-0000-000000000000"));

        // Write the manifest file so detect can rewrite it.
        manifest.write(config_tmp.path()).unwrap();
        sync_auth_on_export(config_tmp.path(), cache_tmp.path(), &mut manifest);

        assert_eq!(manifest.auth_status.id.as_deref(), Some(new_uuid));
        // Cache file was written.
        let entries = crate::auth::load_all_auth_entries(cache_tmp.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uuid, new_uuid);
    }

    #[test]
    fn noop_when_no_claude_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = make_manifest(None);
        sync_auth_on_export(tmp.path(), tmp.path(), &mut manifest);
        assert_eq!(manifest.auth_status.source, AuthSource::None);
    }
}
