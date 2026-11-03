//! Throttle backend implementations.
//!
//! `ThrottleBackend` is the trait; `UmansBackend` is the only impl.
//! `backend_for` selects by `cfg.backend` name. `fetch_cached` wraps fetch
//! with a file-system TTL cache at `{state_dir}/throttle/{backend}-usage.json`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::Throttle;
use crate::materialize::cache::{AgeSource, file_stamp};

/// Normalized usage snapshot from any backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageSnapshot {
    /// Requests remaining in the current window, if known.
    pub(crate) remaining: Option<u64>,
    /// Total request limit for the window, if known.
    pub(crate) limit: Option<u64>,
    /// ISO8601 timestamp when the window resets, if known.
    pub(crate) resets_at: Option<String>,
    /// True when the server is deprioritizing us (low-priority queue or
    /// boxed_until in the future).
    pub(crate) penalized: bool,
}

/// Fetch fresh usage data from the backend.
// ponytail: single backend impl; trait exists so a second backend (e.g. anthropic) can slot in via backend_for
pub trait ThrottleBackend {
    /// Fetch a fresh `UsageSnapshot` from the backend.
    ///
    /// # Errors
    /// Returns an error if the backend is unreachable or returns invalid data.
    fn fetch_usage(&self) -> anyhow::Result<UsageSnapshot>;
}

/// Cached fetch: return the on-disk snapshot if it is fresher than `cache_ttl`
/// seconds; otherwise call the backend and write the result.
///
/// Cache path: `{state_dir}/throttle/{backend}-usage.json`.
/// No locking — concurrent hook invocations may double-fetch; that is acceptable.
pub fn fetch_cached(
    backend: &dyn ThrottleBackend,
    state_dir: &Path,
    cfg: &Throttle,
) -> anyhow::Result<UsageSnapshot> {
    // Guard against path traversal via a malicious backend name.
    if !cfg
        .backend
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        anyhow::bail!(
            "throttle: backend name '{}' contains disallowed characters (only [a-z0-9_-] permitted)",
            cfg.backend
        );
    }

    let cache_dir = state_dir.join("throttle");
    let cache_file = cache_dir.join(format!("{}-usage.json", cfg.backend));

    if let Some(snap) = try_read_cache(&cache_file, cfg.cache_ttl) {
        return Ok(snap);
    }

    let snap = backend.fetch_usage()?;

    // Best-effort cache write — failure is not fatal.
    if let Err(e) = write_cache(&cache_dir, &cache_file, &snap) {
        crate::agent_notice::record_logged(format!(
            "llmenv: the throttle cache could not be saved ({e:#}), so each prompt fetches usage \
             again. Fix: check the permissions of {}.",
            cache_dir.display()
        ));
    }

    Ok(snap)
}

/// The cached snapshot, or `None` for a missing, stale, or unusable cache. A missing cache is
/// the normal cold start, so it is quiet. Any other failure is noticed, because every prompt
/// then fetches usage again (#2746).
fn try_read_cache(cache_file: &std::path::Path, cache_ttl: u64) -> Option<UsageSnapshot> {
    let unusable = |what: &str, e: &dyn std::fmt::Display| {
        crate::agent_notice::record_logged(format!(
            "llmenv: the throttle cache {} {what} ({e}), so usage was fetched again. Fix: check \
             the permissions of that file, or delete it.",
            cache_file.display()
        ));
    };
    let meta = match std::fs::metadata(cache_file) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            unusable("cannot be read", &e);
            return None;
        }
    };
    // A restored cache (`cp -p`, `tar -x`) keeps its old mtime, but its change time is new.
    // The state sweeps judge age the same way, so a restored cache is not fetched again.
    let stamp = file_stamp(&meta, AgeSource::MtimeOrCtime)
        .inspect_err(|e| unusable("has no readable mtime", e))
        .ok()?;
    let age = match SystemTime::now().duration_since(stamp) {
        Ok(d) => d,
        Err(e) => {
            let ahead = format!("{} s ahead", e.duration().as_secs());
            unusable("has a modified time in the future", &ahead);
            return None;
        }
    };
    if age >= Duration::from_secs(cache_ttl) {
        return None;
    }
    let bytes = crate::paths::bounded::state_bytes(cache_file)
        .inspect_err(|e| unusable("cannot be read", e))
        .ok()?;
    match serde_json::from_slice::<UsageSnapshot>(&bytes) {
        Ok(snap) => Some(snap),
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the throttle cache {} is corrupt ({e}), so usage was fetched again. \
                 Fix: delete that file.",
                cache_file.display()
            ));
            None
        }
    }
}

fn write_cache(
    cache_dir: &std::path::Path,
    cache_file: &std::path::Path,
    snap: &UsageSnapshot,
) -> anyhow::Result<()> {
    crate::paths::create_dir_owner_only(cache_dir)
        .with_context(|| format!("creating throttle cache directory {}", cache_dir.display()))?;
    let bytes = serde_json::to_vec(snap)?;
    crate::paths::write_owner_only(cache_file, &bytes).context("writing cache file")?;
    Ok(())
}

/// Select the backend implementation for the given config.
///
/// Returns `None` for unknown backends (with an agent notice) rather than
/// panicking, so hooks always exit 0.
pub fn backend_for(cfg: &Throttle) -> Option<Box<dyn ThrottleBackend>> {
    match cfg.backend.as_str() {
        "umans" => Some(Box::new(UmansBackend)),
        other => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the throttle backend '{other}' is unknown, so throttling is off. Fix: \
                 set the throttle backend to `umans`."
            ));
            None
        }
    }
}

/// Umans backend: reads `~/.umans/config.json` for `api_endpoint` and
/// `api_token`, then GETs `{api_endpoint}/v1/usage` with a Bearer token.
pub struct UmansBackend;

#[derive(Deserialize)]
struct UmansConfig {
    api_endpoint: String,
    api_token: String,
}

/// Wire shape of the umans `/v1/usage` response.
#[derive(Deserialize)]
struct UmansUsageBody {
    limits: Option<UmansLimits>,
    window: Option<UmansWindow>,
    usage: Option<UmansUsage>,
}

#[derive(Deserialize)]
struct UmansLimits {
    requests: Option<UmansLimitRequests>,
}

#[derive(Deserialize)]
struct UmansLimitRequests {
    limit: Option<u64>,
}

#[derive(Deserialize)]
struct UmansWindow {
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct UmansUsage {
    remaining_requests: Option<u64>,
    priority: Option<UmansPriority>,
}

#[derive(Deserialize)]
struct UmansPriority {
    low: Option<bool>,
    boxed_until: Option<String>,
}

impl ThrottleBackend for UmansBackend {
    fn fetch_usage(&self) -> anyhow::Result<UsageSnapshot> {
        let config_path = crate::paths::expand_tilde("~/.umans/config.json")?;
        let raw = crate::paths::bounded::state_text(Path::new(&config_path))?;
        let umans_cfg: UmansConfig = serde_json::from_str(&raw).context("parsing umans config")?;

        let url = format!("{}/v1/usage", umans_cfg.api_endpoint.trim_end_matches('/'));
        // Require https to protect the Bearer token from cleartext exposure.
        if !url.starts_with("https://") {
            anyhow::bail!(
                "umans api_endpoint must use https (got: {}); refusing to send Bearer token in cleartext",
                umans_cfg.api_endpoint
            );
        }
        let (host, addrs) = crate::hook_run::mcp_client::validate_url_production(
            &url,
            crate::hook_run::mcp_client::SsrfPolicy::PublicOnly,
            Duration::from_secs(10),
        )
        .context("umans api_endpoint SSRF check")?;
        let raw = crate::throttle::umans_fetch::fetch_usage_body(
            &url,
            &umans_cfg.api_token,
            &host,
            &addrs,
        )?;
        let body: UmansUsageBody =
            serde_json::from_slice(&raw).context("parsing umans usage response")?;
        map_umans_body(body)
    }
}

/// Map the raw umans body to a normalized `UsageSnapshot`.
fn map_umans_body(body: UmansUsageBody) -> anyhow::Result<UsageSnapshot> {
    let remaining = body.usage.as_ref().and_then(|u| u.remaining_requests);
    let limit = body
        .limits
        .as_ref()
        .and_then(|l| l.requests.as_ref())
        .and_then(|r| r.limit);
    let resets_at = body.window.as_ref().and_then(|w| w.resets_at.clone());

    let penalized = body
        .usage
        .as_ref()
        .and_then(|u| u.priority.as_ref())
        .map(is_penalized)
        .unwrap_or(false);

    Ok(UsageSnapshot {
        remaining,
        limit,
        resets_at,
        penalized,
    })
}

fn is_penalized(p: &UmansPriority) -> bool {
    let low = p.low.unwrap_or(false);
    let boxed = p
        .boxed_until
        .as_deref()
        .map(is_future_timestamp)
        .unwrap_or(false);
    low || boxed
}

/// True if the RFC3339 timestamp string represents a time in the future.
/// Returns false on parse errors (fail-safe). Parsing is delegated to `jiff`,
/// which handles `Z`, numeric offsets (`+00:00`), and fractional seconds.
fn is_future_timestamp(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    match s.parse::<jiff::Timestamp>() {
        Ok(ts) => ts > jiff::Timestamp::now(),
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "llmenv: the throttle backend sent a boxed_until value '{s}' that is not a \
                 timestamp ({e}), so a rate-limit penalty may be missed. Fix: report the value \
                 to the llmenv project."
            ));
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
