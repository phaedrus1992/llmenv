//! Config-drift detection: compares the content hash an agent booted with
//! against a freshly computed current hash.
//!
//! [`run_check_stale`] (in `cli`) owns the CLI subcommand's auto-fix path,
//! which re-materializes via a specific adapter — that dependency on
//! `adapter` must not live here, or `materialize` would depend on `adapter`
//! and recreate the `adapter <-> materialize` cycle the crate-coupling
//! design resolved elsewhere. [`report_if_stale`] covers everything else:
//! detect drift, print a warning if drifted.

/// Outcome of comparing the content hash the agent booted with against the
/// hash llmenv would render now (see [`stale_status`]).
///
/// #196: drift is detected by *content hash*, not folder name. In version
/// mode the folder name is stable across edits (`1.2`), so only the hash
/// recorded in the booted folder's `.llmenv-manifest.json` reveals an
/// in-place change. This is one code path for both
/// [`crate::config::HashingMode`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleStatus {
    /// Booted hash matches the current one — the session is up to date.
    Fresh,
    /// Config drifted since the agent booted; the user should restart.
    Stale { booted: String, current: String },
    /// No booted hash to compare against (llmenv didn't boot this agent, or
    /// the booted folder predates the manifest dotfile).
    Unknown,
}

impl StaleStatus {
    /// True only when the booted config no longer matches the current one.
    #[must_use]
    pub fn is_drift(&self) -> bool {
        matches!(self, StaleStatus::Stale { .. })
    }
}

/// Compare the content hash the agent booted with against the freshly
/// computed current hash. `booted` is the `content_hash` read from the
/// booted folder's manifest dotfile; `None` when the agent wasn't booted by
/// llmenv or the booted folder has no manifest.
#[must_use]
pub fn stale_status(booted: Option<&str>, current: &str) -> StaleStatus {
    match booted {
        None => StaleStatus::Unknown,
        Some(b) if b == current => StaleStatus::Fresh,
        Some(b) => StaleStatus::Stale {
            booted: b.to_string(),
            current: current.to_string(),
        },
    }
}

/// Detect config drift (booted vs. current content hash) and print a
/// warning to stderr if drifted. The auto-fix path (re-materializing via a
/// specific adapter) stays in `cli::run_check_stale`, which owns the
/// `adapter` dependency that auto-fix needs — see this module's doc comment.
pub(crate) fn report_if_stale(use_color: bool) -> anyhow::Result<()> {
    let booted = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .and_then(|dir| {
            crate::materialize::manifest::CacheManifest::read(&dir)
                .ok()
                .flatten()
                .map(|m| m.content_hash)
        });

    let config_path = crate::paths::config_path()?;
    let config = crate::config::Config::load(&config_path)?;
    let config_dir = crate::paths::config_dir()?;

    let env = crate::scope::matcher::Env::detect();
    let active = crate::scope::evaluate(&config, &env);

    let firing = crate::bundle_select::firing_bundles(&config.bundle, &active, None);

    let current =
        match crate::materialize::build_manifest(&config, &config_dir, &active, &firing, false)? {
            Some((manifest, _)) => crate::materialize::cache::hash_manifest(&manifest)?,
            None => return Ok(()),
        };

    match stale_status(booted.as_deref(), &current) {
        StaleStatus::Stale { .. } => {
            let warn = llmenv_util::doctor_warning(use_color);
            eprintln!(
                "{warn} llmenv config changed in place; restart your agent to load it. \
                 (Bundles, MCP wiring, or plugin paths changed since this session started.)"
            );
        }
        StaleStatus::Fresh => {}
        StaleStatus::Unknown => {
            tracing::debug!(
                "check-stale: no booted manifest hash to compare against; \
                 drift detection skipped (current hash would be {current})"
            );
        }
    }
    Ok(())
}
