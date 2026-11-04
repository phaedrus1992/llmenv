//! `llmenv login`: capture Claude Code credentials into the auth cache (#172).

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::adapter::AgentAdapter as _;
use crate::adapter::claude_code::ClaudeCodeAdapter;
use crate::config::Config;
use crate::paths;

/// `llmenv login [--global]` (#172): capture credentials via `claude auth login`
/// and store them in the stable auth cache.
///
/// Without `--global`: updates only the current materialized folder's auth.
/// With `--global`: updates the stable cache (inherited by all future folders).
pub(super) fn run_login(global: bool) -> anyhow::Result<()> {
    let config = Config::load(&paths::config_path()?)?;
    // Intentionally Claude Code-only. Crush does have `crush login`, but it
    // persists tokens under $CRUSH_GLOBAL_DATA — already llmenv's durable state
    // dir — so its auth survives a hash change with no help from us (#544).
    let adapter_root =
        PathBuf::from(paths::expand_tilde(&config.cache.cache_dir)?).join(ClaudeCodeAdapter.name());

    if global {
        eprintln!("Capturing global auth (will be inherited by all new folders)...");
        run_login_capture(&adapter_root, None)?;
    } else {
        // Only inject into a folder that llmenv manages (i.e. under adapter_root).
        // Reject CLAUDE_CONFIG_DIR pointing at an arbitrary directory — that would
        // write auth tokens + a manifest dotfile somewhere unexpected.
        let current_folder = crate::env_var::utf8_var("CLAUDE_CONFIG_DIR")?
            .filter(|p| crate::util::is_within(&adapter_root, p))
            .map(PathBuf::from);
        if current_folder.is_none() {
            eprintln!(
                "note: CLAUDE_CONFIG_DIR is not set or not under the llmenv adapter root — \
                 capturing global auth only. Run `llmenv export` then re-run without \
                 --global to apply to the current folder."
            );
        }
        run_login_capture(&adapter_root, current_folder.as_deref())?;
    }
    Ok(())
}

/// Launch `claude auth login` in a temp dir, capture the resulting auth, and
/// save it to the stable cache. Optionally also writes to `current_folder`.
pub(super) fn run_login_capture(
    adapter_root: &Path,
    current_folder: Option<&Path>,
) -> anyhow::Result<()> {
    let tmp = tempfile::TempDir::new()
        .map_err(|e| anyhow::anyhow!("creating temp dir for login: {e}"))?;

    let claude = crate::path_lookup::resolve_program("claude")?;
    let status = std::process::Command::new(claude)
        .args(["auth", "login"])
        .env("CLAUDE_CONFIG_DIR", tmp.path())
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| {
            anyhow::anyhow!(
                "running `claude auth login` failed: {e}. \
             Is the Claude Code CLI installed and on PATH?"
            )
        })?;

    if !status.success() {
        anyhow::bail!("`claude auth login` exited with status {status}");
    }

    let entry = crate::auth::read_auth_from_dir(tmp.path())?.ok_or_else(|| {
        anyhow::anyhow!(
            "`claude auth login` completed but no oauthAccount found in the result. \
             Try running `claude auth login` directly."
        )
    })?;

    crate::auth::save_auth_entry(adapter_root, &entry)?;
    eprintln!("[llmenv] login: saved auth for {}", entry.email);
    capture_login_credentials(
        crate::auth::credentials::Backend::detect(),
        adapter_root,
        tmp.path(),
        current_folder,
    )?;

    if let Some(folder) = current_folder
        && folder.is_dir()
    {
        crate::auth::inject_auth_into_claude_json(folder, &entry)?;
        // Update manifest auth_status to Explicit.
        match crate::materialize::manifest::CacheManifest::read(folder) {
            Ok(Some(mut manifest)) => {
                manifest.auth_status = crate::materialize::manifest::AuthStatus {
                    source: crate::materialize::manifest::AuthSource::Explicit,
                    id: Some(entry.uuid),
                    email: Some(entry.email),
                };
                manifest.write(folder)?;
            }
            Ok(None) => {} // no manifest yet — benign
            Err(e) => tracing::warn!(
                "failed to read cache manifest in {:?} for auth status update: {e}",
                folder,
            ),
        }
    }
    Ok(())
}

/// Capture the OAuth credential a fresh `claude auth login` produced (#1057).
///
/// Overwrites the cache unconditionally — unlike the export path, the user just
/// authenticated, so the new token always wins. Also writes it straight into
/// `current_folder` so the session in progress is logged in without re-exporting.
///
/// Non-fatal when the login dir yields no credential: `oauthAccount` was already
/// captured, so identity inheritance still works.
///
/// # Errors
/// Returns an error when the cache or folder write fails.
fn capture_login_credentials(
    backend: crate::auth::credentials::Backend,
    adapter_root: &Path,
    login_dir: &Path,
    current_folder: Option<&Path>,
) -> anyhow::Result<()> {
    use crate::auth::credentials;
    let Some(creds) = credentials::read_backend(backend, login_dir)? else {
        tracing::debug!("login produced no OAuth credential; only the account identity was cached");
        return Ok(());
    };
    // A dead blob would replace a good cache entry and clear the dead-read marker, so refuse it.
    if creds.is_expired_now() {
        anyhow::bail!(
            "login produced an expired OAuth credential, so it was not saved. Fix: run `llmenv login` again."
        );
    }
    credentials::save_cached(adapter_root, &creds)?;
    eprintln!("[llmenv] login: saved OAuth credential to durable state");
    if let Some(folder) = current_folder.filter(|f| f.is_dir()) {
        credentials::write_backend(backend, folder, &creds)?;
    }
    // The login dir is a temp dir about to be removed; on macOS its keychain
    // item would outlive it, so drop it now that the blob is cached.
    if let Err(e) = drop_login_entry(backend, login_dir) {
        tracing::error!(
            "could not drop the login dir credential entry: {e}. Fix: run `llmenv clean`, or delete the keychain item by hand."
        );
    }
    Ok(())
}

/// Drop the keychain item that a login dir owns, when the backend keeps one. A file backend keeps
/// the credential in the dir, and the caller removes that dir.
fn drop_login_entry(
    backend: crate::auth::credentials::Backend,
    login_dir: &Path,
) -> anyhow::Result<bool> {
    match backend {
        crate::auth::credentials::Backend::File => {
            // The file backend keeps the credential in the folder, so the folder is not used here.
            let _ = login_dir;
            Ok(false)
        }
        #[cfg(target_os = "macos")]
        crate::auth::credentials::Backend::Keychain => crate::auth::credentials::forget(login_dir),
    }
}

#[cfg(test)]
mod tests;

/// Import auth from an existing `.claude.json` file into the stable cache (#172).
pub(super) fn import_auth_from_file(source: &Path, adapter_root: &Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(source).with_context(|| format!("reading {}", source.display()))?;
    let doc: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", source.display()))?;
    let entry = crate::auth::extract_auth_entry(&doc).ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no oauthAccount block — not logged in. \
             Try `llmenv login` to authenticate.",
            source.display()
        )
    })?;
    crate::auth::save_auth_entry(adapter_root, &entry)?;
    eprintln!("[llmenv] login: imported auth for {}", entry.email);
    Ok(())
}
