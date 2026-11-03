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

// #1186: write_cache's cache directory must be owner-only, not just the
// cache file it writes inside it.
#[cfg(unix)]
#[test]
fn write_cache_creates_dir_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("throttle");
    let cache_file = cache_dir.join("umans-usage.json");
    let snap = UsageSnapshot {
        remaining: Some(1),
        limit: Some(2),
        resets_at: None,
        penalized: false,
    };
    write_cache(&cache_dir, &cache_file, &snap).unwrap();
    let mode = std::fs::metadata(&cache_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "cache dir must be owner-only, got {mode:o}");
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

#[test]
fn a_restored_cache_with_an_old_mtime_is_still_used() {
    let dir = tempfile::tempdir().unwrap();
    let cache_file = dir.path().join("usage.json");
    let snap = UsageSnapshot {
        remaining: Some(7),
        limit: Some(200),
        resets_at: None,
        penalized: false,
    };
    std::fs::write(&cache_file, serde_json::to_vec(&snap).unwrap()).unwrap();
    // Setting the mtime also sets the change time to now, as a restore by `cp -p` or `tar -x` does.
    let day_ago = SystemTime::now() - Duration::from_secs(86_400);
    filetime::set_file_mtime(&cache_file, filetime::FileTime::from_system_time(day_ago)).unwrap();

    assert_eq!(try_read_cache(&cache_file, 300), Some(snap));
}
