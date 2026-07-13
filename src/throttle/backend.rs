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

/// Normalized usage snapshot from any backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageSnapshot {
    /// Requests remaining in the current window, if known.
    pub remaining: Option<u64>,
    /// Total request limit for the window, if known.
    pub limit: Option<u64>,
    /// ISO8601 timestamp when the window resets, if known.
    pub resets_at: Option<String>,
    /// True when the server is deprioritizing us (low-priority queue or
    /// boxed_until in the future).
    pub penalized: bool,
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
        eprintln!("llmenv throttle: cache write failed (non-fatal): {e}");
    }

    Ok(snap)
}

fn try_read_cache(cache_file: &std::path::Path, cache_ttl: u64) -> Option<UsageSnapshot> {
    let meta = std::fs::metadata(cache_file)
        .inspect_err(|e| tracing::warn!("throttle cache stat failed: {e}"))
        .ok()?;
    let modified = meta
        .modified()
        .inspect_err(|e| tracing::warn!("throttle cache mtime failed: {e}"))
        .ok()?;
    let age = match SystemTime::now().duration_since(modified) {
        Ok(d) => d,
        Err(_) => {
            eprintln!("llmenv throttle: cache clock skew detected, treating cache as stale");
            return None;
        }
    };
    if age >= Duration::from_secs(cache_ttl) {
        return None;
    }
    let bytes = std::fs::read(cache_file)
        .inspect_err(|e| {
            tracing::warn!("throttle cache read failed (falling back to live fetch): {e}")
        })
        .ok()?;
    match serde_json::from_slice::<UsageSnapshot>(&bytes) {
        Ok(snap) => Some(snap),
        Err(e) => {
            eprintln!("llmenv throttle: cache file corrupt (falling back to live fetch): {e}");
            None
        }
    }
}

fn write_cache(
    cache_dir: &std::path::Path,
    cache_file: &std::path::Path,
    snap: &UsageSnapshot,
) -> anyhow::Result<()> {
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
        anyhow::bail!("create_dir_all failed: {e}");
    }
    let bytes = serde_json::to_vec(snap)?;
    crate::paths::write_owner_only(cache_file, &bytes).context("writing cache file")?;
    Ok(())
}

/// Select the backend implementation for the given config.
///
/// Returns `None` for unknown backends (with a stderr diagnostic) rather than
/// panicking, so hooks always exit 0.
pub fn backend_for(cfg: &Throttle) -> Option<Box<dyn ThrottleBackend>> {
    match cfg.backend.as_str() {
        "umans" => Some(Box::new(UmansBackend)),
        other => {
            eprintln!("llmenv throttle: unknown backend '{other}' — skipping");
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
        let config_path = crate::paths::expand_tilde("~/.umans/config.json");
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading umans config: {config_path}"))?;
        let umans_cfg: UmansConfig = serde_json::from_str(&raw).context("parsing umans config")?;

        let url = format!("{}/v1/usage", umans_cfg.api_endpoint.trim_end_matches('/'));
        // Require https to protect the Bearer token from cleartext exposure.
        if !url.starts_with("https://") {
            anyhow::bail!(
                "umans api_endpoint must use https (got: {}); refusing to send Bearer token in cleartext",
                umans_cfg.api_endpoint
            );
        }
        let _ = crate::hook_run::mcp_client::validate_url_production(
            &url,
            crate::hook_run::mcp_client::SsrfPolicy::PublicOnly,
            Duration::from_secs(10),
        )
        .context("umans api_endpoint SSRF check")?;
        let body = fetch_json_blocking(&url, &umans_cfg.api_token)?;
        map_umans_body(body)
    }
}

/// Blocking HTTP GET returning parsed JSON. Uses tokio block_on + reqwest async.
fn fetch_json_blocking(url: &str, token: &str) -> anyhow::Result<UmansUsageBody> {
    let url = url.to_owned();
    let auth = format!("Bearer {token}");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    rt.block_on(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("building reqwest client")?;
        let resp = client
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp
                .bytes()
                .await
                .inspect_err(
                    |e| tracing::warn!(error = %e, url = %url, "failed to read throttle error response body"),
                )
                .unwrap_or_default();
            let preview: String = String::from_utf8_lossy(&body).chars().take(512).collect();
            anyhow::bail!("umans usage API returned {status}: {preview}");
        }
        const MAX_BODY: u64 = 65_536;
        if let Some(len) = resp.content_length()
            && len > MAX_BODY
        {
            anyhow::bail!("umans usage response too large: {len} bytes (limit {MAX_BODY})");
        }
        let bytes = resp.bytes().await.context("reading umans usage response")?;
        if bytes.len() as u64 > MAX_BODY {
            anyhow::bail!(
                "umans usage response too large: {} bytes (limit {MAX_BODY})",
                bytes.len()
            );
        }
        serde_json::from_slice::<UmansUsageBody>(&bytes).context("parsing umans usage response")
    })
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
            eprintln!(
                "llmenv throttle: boxed_until '{s}' could not be parsed \
                 (treating as not-penalized): {e}"
            );
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // Sample body from the spec
    const SAMPLE_BODY: &str = r#"{
        "plan": {"slug": "code_pro"},
        "limits": {"requests": {"limit": 200, "hard_cap": 400, "window_seconds": 18000}},
        "window": {"resets_at": "2026-06-29T23:33:46.154428+00:00", "remaining_minutes": 185},
        "usage": {
            "requests_in_window": 67,
            "remaining_requests": 133,
            "priority": {
                "low": true,
                "boxed_until": "2026-06-29T23:45:36.654505+00:00",
                "reason": "rate_limited"
            }
        }
    }"#;

    #[test]
    fn sample_body_maps_to_snapshot() {
        let body: UmansUsageBody = serde_json::from_str(SAMPLE_BODY).unwrap();
        let snap = map_umans_body(body).unwrap();
        assert_eq!(snap.remaining, Some(133));
        assert_eq!(snap.limit, Some(200));
        assert_eq!(
            snap.resets_at.as_deref(),
            Some("2026-06-29T23:33:46.154428+00:00")
        );
        // penalized because low == true (regardless of boxed_until in past/future)
        assert!(snap.penalized);
    }

    #[test]
    fn penalized_true_when_priority_low() {
        let body: UmansUsageBody = serde_json::from_str(SAMPLE_BODY).unwrap();
        let snap = map_umans_body(body).unwrap();
        assert!(
            snap.penalized,
            "penalized must be true when priority.low == true"
        );
    }

    #[test]
    fn penalized_true_when_boxed_until_in_future() {
        // Use a far-future boxed_until with low=false
        let raw = r#"{
            "limits": {"requests": {"limit": 200}},
            "window": {"resets_at": "2099-01-01T00:00:00Z"},
            "usage": {
                "remaining_requests": 50,
                "priority": {"low": false, "boxed_until": "2099-01-01T00:00:00Z"}
            }
        }"#;
        let body: UmansUsageBody = serde_json::from_str(raw).unwrap();
        let snap = map_umans_body(body).unwrap();
        assert!(
            snap.penalized,
            "penalized must be true when boxed_until is in the future"
        );
    }

    #[test]
    fn penalized_false_when_boxed_until_in_past() {
        let raw = r#"{
            "limits": {"requests": {"limit": 200}},
            "window": {"resets_at": "2020-01-01T00:00:00Z"},
            "usage": {
                "remaining_requests": 150,
                "priority": {"low": false, "boxed_until": "2020-01-01T00:00:00Z"}
            }
        }"#;
        let body: UmansUsageBody = serde_json::from_str(raw).unwrap();
        let snap = map_umans_body(body).unwrap();
        assert!(
            !snap.penalized,
            "penalized must be false when boxed_until is in the past"
        );
    }

    #[test]
    fn future_timestamp_detected() {
        assert!(is_future_timestamp("2099-01-01T00:00:00Z"));
        assert!(is_future_timestamp("2099-01-01T00:00:00.123456+00:00"));
    }

    #[test]
    fn past_timestamp_not_future() {
        assert!(!is_future_timestamp("2020-01-01T00:00:00Z"));
        assert!(!is_future_timestamp("2020-01-01T00:00:00.123456+00:00"));
    }

    #[test]
    fn unparseable_timestamp_is_not_future() {
        // Fail-safe: a garbage timestamp must not be treated as a future penalty.
        assert!(!is_future_timestamp("not-a-timestamp"));
        assert!(!is_future_timestamp(""));
    }

    #[test]
    fn usage_snapshot_json_roundtrip() {
        let snap = UsageSnapshot {
            remaining: Some(133),
            limit: Some(200),
            resets_at: Some("2026-06-29T23:33:46.154428+00:00".to_string()),
            penalized: true,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: UsageSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, decoded);
    }

    #[test]
    fn penalized_cross_product() {
        // low ∈ {None, false, true} × boxed_until ∈ {none, past, future}
        let past = "2020-01-01T00:00:00Z";
        let future = "2099-01-01T00:00:00Z";

        let cases: &[(Option<bool>, Option<&str>, bool)] = &[
            (None, None, false),
            (None, Some(past), false),
            (None, Some(future), true),
            (Some(false), None, false),
            (Some(false), Some(past), false),
            (Some(false), Some(future), true),
            (Some(true), None, true),
            (Some(true), Some(past), true),
            (Some(true), Some(future), true),
        ];

        for &(low, boxed_until, expected) in cases {
            let raw = serde_json::json!({
                "usage": {
                    "remaining_requests": 50,
                    "priority": {
                        "low": low,
                        "boxed_until": boxed_until
                    }
                }
            });
            let body: UmansUsageBody = serde_json::from_value(raw).unwrap();
            let snap = map_umans_body(body).unwrap();
            assert_eq!(
                snap.penalized, expected,
                "low={low:?} boxed_until={boxed_until:?}: expected penalized={expected}"
            );
        }
    }
}
