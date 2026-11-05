//! Claude.ai usage-window widgets (`usage_5h` / `usage_7d`): the used
//! percentage, the bar, the reset label, and the pace marker for one
//! rate-limit window.

use super::reset_clock::{ClockStyle, format_reset_clock};
use super::usage_state::usage_delta;
use super::usage_state::usage_state_dir;
use super::{
    BarStyle, DEFAULT_BAR_WIDTH, EngineData, RateLimitWindow, now_unix, pct_and_bar,
    resolve_bar_width,
};
use std::path::Path;

/// Seconds in one day. A usage window longer than this shows the weekday of its reset.
const ONE_DAY_SECS: i64 = 86_400;

/// `usage_5h`/`usage_7d` dispatch: resolves the window, default thresholds,
/// and default format that differ between the two, then delegates to the
/// shared [`render_usage`] backend.
pub(super) fn render_usage_widget(
    name: &str,
    data: &EngineData,
    cfg: Option<&llmenv_config::WidgetConfig>,
    use_color: bool,
) -> String {
    let state = usage_state_dir();
    let (window, window_secs, defaults, fmt) = if name == "usage_5h" {
        (
            data.rate_limits.as_ref().and_then(|r| r.five_hour.as_ref()),
            FIVE_HOUR_SECS,
            [70, 90],
            "5h {pct}%{delta}{pace} ➡{reset}",
        )
    } else {
        (
            data.rate_limits.as_ref().and_then(|r| r.seven_day.as_ref()),
            SEVEN_DAY_SECS,
            [60, 80],
            "7d {pct}%{delta}{pace} ➡{reset}",
        )
    };
    render_usage(
        &UsageArgs {
            window,
            now: now_unix(),
            window_secs,
            thresholds: cfg.and_then(|c| c.thresholds).unwrap_or(defaults),
            state_dir: state.as_deref(),
            state_key: name,
            use_color,
        },
        cfg,
        fmt,
    )
}

/// Humanize the seconds from `now` until `resets_at`: `"3h04m"` past an hour,
/// `"23m"` under. A reset already in the past clamps to `"0m"`.
pub(super) fn humanize_until(resets_at: i64, now: i64) -> String {
    super::super::peak::humanize(resets_at.saturating_sub(now))
}

/// The `{reset}` text. The default is the countdown from [`humanize_until`].
/// `display: clock_24h` and `display: clock_12h` show the local clock time at
/// reset instead. A window longer than a day adds the weekday. When the system
/// time zone or the reset time cannot be read, the countdown is shown and a
/// warning is logged, so the label is never a clock time in the wrong zone.
fn reset_label(
    resets_at: i64,
    now: i64,
    window_secs: i64,
    cfg: Option<&llmenv_config::WidgetConfig>,
) -> String {
    let style = match cfg.and_then(|c| c.display.as_deref()) {
        Some("clock_24h") => ClockStyle::TwentyFourHour,
        Some("clock_12h") => ClockStyle::TwelveHour,
        _ => return humanize_until(resets_at, now),
    };
    let tz = match jiff::tz::TimeZone::try_system() {
        Ok(tz) => tz,
        Err(e) => {
            tracing::warn!(
                "usage reset: cannot read the system time zone, so the countdown is shown: {e}"
            );
            return humanize_until(resets_at, now);
        }
    };
    match format_reset_clock(resets_at, &tz, style, window_secs > ONE_DAY_SECS) {
        Some(label) => label,
        None => {
            tracing::warn!(
                "usage reset time {resets_at} is out of range, so the countdown is shown"
            );
            humanize_until(resets_at, now)
        }
    }
}

/// Render a Claude.ai usage window (`five_hour` / `seven_day`). Empty when the
/// window is absent (not a subscriber, or before the first API response) or
/// carries a non-finite percentage. `{pct}` is the clamped, rounded used
/// percentage, `{bar}` a 10-cell fill of it, `{reset}` the time until the
/// window resets.
/// Inputs for a usage-window widget, bundled to keep the argument count sane.
pub(super) struct UsageArgs<'a> {
    pub(super) window: Option<&'a RateLimitWindow>,
    pub(super) now: i64,
    pub(super) window_secs: i64,
    pub(super) thresholds: [u8; 2],
    /// Directory for delta state files (`None` disables delta tracking).
    pub(super) state_dir: Option<&'a Path>,
    pub(super) state_key: &'a str,
    pub(super) use_color: bool,
}

/// Render a Claude.ai usage window: threshold-colored percentage, a per-cell
/// bar (filled = threshold color, empty dim) with a bright pace-target marker,
/// reset countdown or clock time, over/under-pace indicator, and a delta from
/// the last render. `{pct}`/`{bar}`/`{reset}`/`{pace}`/`{delta}` are the placeholders.
pub(super) fn render_usage(
    args: &UsageArgs,
    cfg: Option<&llmenv_config::WidgetConfig>,
    default_format: &str,
) -> String {
    let Some(window) = args.window else {
        return String::new();
    };
    let Some(raw) = window.used_percentage else {
        return String::new();
    };
    if !raw.is_finite() {
        return String::new();
    }
    let pct = raw.clamp(0.0, 100.0);
    let width = resolve_bar_width(cfg, DEFAULT_BAR_WIDTH);
    let reset = window
        .resets_at
        .map(|r| reset_label(r, args.now, args.window_secs, cfg))
        .unwrap_or_default();
    let (pace, marker) = match window.resets_at {
        Some(r) => pace_and_target(pct, r, args.window_secs, args.now, width),
        None => (String::new(), None),
    };
    let style = BarStyle::Threshold {
        thresholds: args.thresholds,
        marker,
    };
    let (pct_str, bar) = pct_and_bar(pct, width, &style, args.use_color);
    let delta = usage_delta(args.state_dir, args.state_key, pct, args.now);
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or(default_format);
    format
        .replace("{pct}", &pct_str)
        .replace("{bar}", &bar)
        .replace("{reset}", &reset)
        .replace("{pace}", &pace)
        .replace("{delta}", &delta)
}

/// The over/under-pace indicator plus the bar column of the linear-pace
/// target. `⇡N%` when usage is ahead of the elapsed window time (burning too
/// fast), `⇣N%` when behind, empty within ±0.5% (with a leading space so it
/// drops out cleanly). The marker column is `None` at the bar extremes, where
/// a marker reads oddly.
fn pace_and_target(
    pct: f64,
    resets_at: i64,
    window_secs: i64,
    now: i64,
    width: usize,
) -> (String, Option<usize>) {
    if window_secs <= 0 {
        return (String::new(), None);
    }
    let remaining = (resets_at - now).clamp(0, window_secs);
    let target = (window_secs - remaining) as f64 / window_secs as f64 * 100.0;
    let over = pct - target;
    let abs = over.abs().round() as i64;
    let indicator = if over > 0.5 {
        format!(" \u{21e1}{abs}%") // ⇡
    } else if over < -0.5 {
        format!(" \u{21e3}{abs}%") // ⇣
    } else {
        String::new()
    };
    let pos = (target / 100.0 * width as f64).round() as i64;
    let marker = (pos > 0 && (pos as usize) < width).then_some(pos as usize);
    (indicator, marker)
}

/// Window durations in seconds for the pace calculation.
pub(super) const FIVE_HOUR_SECS: i64 = 5 * 3600;
const SEVEN_DAY_SECS: i64 = 7 * 86_400;

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails loudly when its fixture cannot be set up"
)]
mod tests {
    use super::*;

    /// Fixed clock for the tests. Only the offsets from it are asserted.
    const NOW: i64 = 1_000_000_000;

    fn display_cfg(display: &str) -> llmenv_config::WidgetConfig {
        llmenv_config::WidgetConfig {
            display: Some(display.to_string()),
            ..Default::default()
        }
    }

    proptest::proptest! {
        /// A display that names no clock style is the countdown, for any reset time, clock, and
        /// window. A clock style never panics and never gives an empty label (#3064).
        #[test]
        fn reset_label_is_the_countdown_unless_a_clock_is_named(
            resets_at in proptest::prelude::any::<i64>(),
            now in proptest::prelude::any::<i64>(),
            window in proptest::prelude::any::<i64>(),
            display in proptest::option::of("[a-z_0-9]{0,10}"),
        ) {
            let cfg = display.as_deref().map(display_cfg);
            let label = reset_label(resets_at, now, window, cfg.as_ref());
            match display.as_deref() {
                Some("clock_24h" | "clock_12h") => proptest::prop_assert!(!label.is_empty()),
                _ => proptest::prop_assert_eq!(label, humanize_until(resets_at, now)),
            }
        }
    }

    #[test]
    fn reset_label_defaults_to_countdown() {
        assert_eq!(reset_label(NOW + 1380, NOW, FIVE_HOUR_SECS, None), "23m");
    }

    #[test]
    fn reset_label_with_unknown_display_keeps_countdown() {
        let cfg = display_cfg("bogus");
        assert_eq!(
            reset_label(NOW + 1380, NOW, FIVE_HOUR_SECS, Some(&cfg)),
            "23m"
        );
    }

    #[test]
    fn reset_label_short_window_clock_has_no_weekday() {
        let cfg = display_cfg("clock_12h");
        let label = reset_label(NOW + 7200, NOW, FIVE_HOUR_SECS, Some(&cfg));
        assert!(!label.contains(' '), "{label}");
        assert!(label.ends_with("am") || label.ends_with("pm"), "{label}");
    }

    #[test]
    fn reset_label_24h_clock_has_no_meridiem() {
        let cfg = display_cfg("clock_24h");
        let label = reset_label(NOW + 7200, NOW, FIVE_HOUR_SECS, Some(&cfg));
        assert!(!label.ends_with("am") && !label.ends_with("pm"), "{label}");
        assert_eq!(label.len(), 5, "{label}");
        assert_eq!(label.as_bytes()[2], b':', "{label}");
    }

    #[test]
    fn reset_label_week_window_clock_leads_with_weekday() {
        let cfg = display_cfg("clock_24h");
        let label = reset_label(NOW + 86_400, NOW, SEVEN_DAY_SECS, Some(&cfg));
        let (day, clock) = label.split_once(' ').expect("weekday then clock");
        assert!(
            ["mon", "tue", "wed", "thu", "fri", "sat", "sun"].contains(&day),
            "{label}"
        );
        assert_eq!(clock.len(), 5, "{label}");
    }
}
