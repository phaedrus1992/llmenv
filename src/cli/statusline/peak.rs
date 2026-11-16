//! Peak / off-peak billing-window indicator, computed entirely client-side
//! from the wall clock — Claude Code sends no peak-window data on stdin.
//! Anthropic's peak window is weekdays 05:00–11:00 America/Los_Angeles. The zone
//! comes from the jiff zone database, the same source the reset clock uses, so
//! its DST rules are not hand-rolled (#3063).

use std::time::{SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use jiff::civil::Weekday;
use jiff::tz::TimeZone;

use super::UNKNOWN_MARKER;

/// The zone whose wall clock sets the peak window.
const PEAK_ZONE: &str = "America/Los_Angeles";

/// Whether now is inside the peak window, and seconds until the window
/// boundary flips (until peak ends if in peak; until the next peak starts
/// otherwise).
pub struct PeakInfo {
    pub is_peak: bool,
    pub countdown_secs: i64,
}

/// The wall clock as seconds since the epoch.
///
/// # Errors
/// Fails when the clock is before 1970, or when the seconds do not fit in an `i64`. A zero
/// here would place "now" in 1970 and report a wrong window with no sign of it.
fn now_unix() -> anyhow::Result<i64> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("the system clock is before 1970: {e}"))?;
    Ok(i64::try_from(since_epoch.as_secs())?)
}

/// Render the `peak` widget: `△ peak 3h03m` in peak, `▽ off-peak 2h` otherwise.
/// `{symbol}` / `{label}` / `{countdown}` are the format placeholders.
pub fn render_peak(cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    render_peak_info(cfg, now_unix().and_then(is_peak_time))
}

/// [`render_peak`] for a window that the caller computed. When the window cannot be computed,
/// the configured format stays and each placeholder shows the unknown marker (#3071).
fn render_peak_info(
    cfg: Option<&llmenv_config::WidgetConfig>,
    peak: anyhow::Result<PeakInfo>,
) -> String {
    let (symbol, label, countdown) = match peak {
        Ok(peak) => {
            let (symbol, label) = if peak.is_peak {
                ("\u{25b3}", "peak") // △
            } else {
                ("\u{25bd}", "off-peak") // ▽
            };
            let countdown = if peak.countdown_secs > 0 {
                humanize(peak.countdown_secs)
            } else {
                String::new()
            };
            (symbol.to_owned(), label.to_owned(), countdown)
        }
        Err(e) => {
            tracing::warn!("peak widget: cannot compute the window for {PEAK_ZONE}: {e:#}");
            let marker = || UNKNOWN_MARKER.to_owned();
            (marker(), marker(), marker())
        }
    };
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{symbol} {label} {countdown}");
    format
        .replace("{symbol}", &symbol)
        .replace("{label}", &label)
        .replace("{countdown}", &countdown)
        .trim_end()
        .to_string()
}

pub(super) fn humanize(secs: i64) -> String {
    let secs = secs.max(0);
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else {
        format!("{mins}m")
    }
}

/// Peak = a weekday (Mon–Fri) between 05:00 and 11:00 Pacific. The boundary is an
/// instant in the zone, so a DST change inside the countdown is counted correctly.
///
/// # Errors
/// Fails when the zone database has no `America/Los_Angeles` zone, or when `now_unix` is
/// outside the range jiff can represent.
fn is_peak_time(now_unix: i64) -> anyhow::Result<PeakInfo> {
    let tz = TimeZone::get(PEAK_ZONE)?;
    let civil = Timestamp::from_second(now_unix)?
        .to_zoned(tz.clone())
        .datetime();
    let is_weekday = !matches!(civil.weekday(), Weekday::Saturday | Weekday::Sunday);
    let hour = civil.hour();
    let is_peak = is_weekday && (5..11).contains(&hour);

    let boundary = if is_peak {
        // Until 11:00 today.
        civil.date().at(11, 0, 0, 0).to_zoned(tz)?
    } else if is_weekday && hour < 5 {
        // Before peak today.
        civil.date().at(5, 0, 0, 0).to_zoned(tz)?
    } else {
        // After peak today, or on a weekend: the next peak starts at 05:00 on a weekday.
        let mut next = civil.date().tomorrow()?;
        while matches!(next.weekday(), Weekday::Saturday | Weekday::Sunday) {
            next = next.tomorrow()?;
        }
        next.at(5, 0, 0, 0).to_zoned(tz)?
    };

    Ok(PeakInfo {
        is_peak,
        countdown_secs: boundary.timestamp().as_second() - now_unix,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn weekday_during_peak_hours_is_peak() {
        // 2026-04-16 08:00 PDT = 15:00 UTC (Thursday).
        assert!(is_peak_time(1_776_351_600).unwrap().is_peak);
    }

    #[test]
    fn weekday_outside_peak_hours_is_off_peak() {
        // 2026-04-16 14:00 PDT = 21:00 UTC (Thursday).
        assert!(!is_peak_time(1_776_373_200).unwrap().is_peak);
    }

    #[test]
    fn weekend_is_always_off_peak() {
        // 2026-04-18 08:00 PDT (Saturday).
        assert!(!is_peak_time(1_776_524_400).unwrap().is_peak);
    }

    #[test]
    fn winter_uses_pst_offset() {
        // 2026-01-15 09:00 PST = 17:00 UTC (Thursday) → peak.
        assert!(is_peak_time(1_768_496_400).unwrap().is_peak);
    }

    #[test]
    fn summer_uses_pdt_offset_so_the_peak_ends_in_three_hours() {
        // 2026-07-15 08:00 PDT = 15:00 UTC (Wednesday). 11:00 PDT is 3 h later.
        let peak = is_peak_time(1_784_127_600).unwrap();
        assert!(peak.is_peak);
        assert_eq!(peak.countdown_secs, 3 * 3600);
    }

    #[test]
    fn a_countdown_across_spring_forward_counts_the_real_hours() {
        // 2026-03-06 12:00 PST = 20:00 UTC (Friday, after peak). The next peak starts
        // Monday 2026-03-09 05:00 PDT = 12:00 UTC, which is 64 h later.
        let peak = is_peak_time(1_772_827_200).unwrap();
        assert!(!peak.is_peak);
        assert_eq!(peak.countdown_secs, 64 * 3600);
    }

    #[test]
    fn an_unreadable_zone_database_keeps_the_format_and_shows_the_marker() {
        let failed = || Err(anyhow::anyhow!("no zone database"));
        let logs = crate::test_log_capture::capture_logs(|| {
            assert_eq!(render_peak_info(None, failed()), "? ? ?");
            let cfg = llmenv_config::WidgetConfig {
                format: Some("{label} in {countdown}".to_string()),
                ..Default::default()
            };
            assert_eq!(render_peak_info(Some(&cfg), failed()), "? in ?");
        });
        assert_eq!(logs.matches("WARN").count(), 2, "{logs}");
        assert!(logs.contains("no zone database"), "{logs}");
    }

    #[test]
    fn humanize_hours_and_minutes() {
        assert_eq!(humanize(3 * 3600 + 3 * 60), "3h03m");
        assert_eq!(humanize(45 * 60), "45m");
        assert_eq!(humanize(-5), "0m");
    }
}
