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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    let cache_dir = state_dir.join("throttle");
    let cache_file = cache_dir.join(format!("{}-usage.json", cfg.backend));

    if let Ok(meta) = std::fs::metadata(&cache_file)
        && let Ok(modified) = meta.modified()
    {
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::MAX);
        if age < Duration::from_secs(cfg.cache_ttl)
            && let Ok(bytes) = std::fs::read(&cache_file)
            && let Ok(snap) = serde_json::from_slice::<UsageSnapshot>(&bytes)
        {
            return Ok(snap);
        }
    }

    let snap = backend.fetch_usage()?;

    // Best-effort cache write — failure is not fatal.
    if std::fs::create_dir_all(&cache_dir).is_ok()
        && let Ok(bytes) = serde_json::to_vec(&snap)
    {
        let _ = std::fs::write(&cache_file, bytes);
    }

    Ok(snap)
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
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("umans usage API returned {}", resp.status());
        }
        resp.json::<UmansUsageBody>()
            .await
            .context("parsing umans usage response")
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
        .map(|p| {
            let low = p.low.unwrap_or(false);
            let boxed = p
                .boxed_until
                .as_deref()
                .map(is_future_timestamp)
                .unwrap_or(false);
            low || boxed
        })
        .unwrap_or(false);

    Ok(UsageSnapshot {
        remaining,
        limit,
        resets_at,
        penalized,
    })
}

/// True if the RFC3339 timestamp string represents a time in the future.
/// Returns false on parse errors (fail-safe). Parsing is delegated to `jiff`,
/// which handles `Z`, numeric offsets (`+00:00`), and fractional seconds.
fn is_future_timestamp(s: &str) -> bool {
    s.parse::<jiff::Timestamp>()
        .is_ok_and(|ts| ts > jiff::Timestamp::now())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
}
