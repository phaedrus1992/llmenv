//! Peak / off-peak billing-window indicator, computed entirely client-side
//! from the wall clock — Claude Code sends no peak-window data on stdin.
//! Anthropic's peak window is weekdays 05:00–11:00 America/Los_Angeles; this
//! mirrors the reference tool's calculation, including a hand-rolled Pacific
//! DST offset (no `chrono`/`time` dependency in this binary).

use std::time::{SystemTime, UNIX_EPOCH};

/// Whether now is inside the peak window, and seconds until the window
/// boundary flips (until peak ends if in peak; until the next peak starts
/// otherwise).
pub struct PeakInfo {
    pub is_peak: bool,
    pub countdown_secs: i64,
}

fn now_unix() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

/// Render the `peak` widget: `△ peak 3h03m` in peak, `▽ off-peak 2h` otherwise.
/// `{symbol}` / `{label}` / `{countdown}` are the format placeholders.
pub fn render_peak(cfg: Option<&llmenv_config::WidgetConfig>) -> String {
    let peak = is_peak_time(now_unix());
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
    let format = cfg
        .and_then(|c| c.format.as_deref())
        .unwrap_or("{symbol} {label} {countdown}");
    format
        .replace("{symbol}", symbol)
        .replace("{label}", label)
        .replace("{countdown}", &countdown)
        .trim_end()
        .to_string()
}

fn humanize(secs: i64) -> String {
    let secs = secs.max(0);
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else {
        format!("{mins}m")
    }
}

/// Peak = a weekday (Mon–Fri) between 05:00 and 11:00 Pacific.
fn is_peak_time(now_unix: i64) -> PeakInfo {
    let local_unix = now_unix + pacific_utc_offset(now_unix);
    let day_secs = local_unix.rem_euclid(86_400);
    let day_of_epoch = local_unix.div_euclid(86_400);
    // 1970-01-01 was a Thursday. Map to 0=Mon .. 6=Sun.
    let weekday = (day_of_epoch + 3).rem_euclid(7);
    let is_weekday = weekday < 5;
    let hour = day_secs / 3600;
    let is_peak = is_weekday && (5..11).contains(&hour);

    let countdown_secs = if is_peak {
        // Until 11:00 today.
        (11 * 3600 - day_secs).max(0)
    } else if is_weekday && hour < 5 {
        // Before peak today.
        (5 * 3600 - day_secs).max(0)
    } else if is_weekday && hour >= 11 {
        // After peak today: next peak tomorrow, or Monday if it's Friday.
        let days_until = if weekday == 4 { 3 } else { 1 };
        days_until * 86_400 + 5 * 3600 - day_secs
    } else {
        // Weekend: next peak is Monday 05:00.
        let days_until_monday = if weekday == 5 { 2 } else { 1 };
        days_until_monday * 86_400 + 5 * 3600 - day_secs
    };

    PeakInfo {
        is_peak,
        countdown_secs,
    }
}

/// UTC offset (seconds) for `America/Los_Angeles`: PDT (UTC-7) from the second
/// Sunday of March to the first Sunday of November, PST (UTC-8) otherwise.
fn pacific_utc_offset(now_unix: i64) -> i64 {
    let days_since_epoch = now_unix.div_euclid(86_400);
    let year = 1970 + days_since_epoch * 400 / 146_097;
    let march_dst_start = nth_sunday_of_month(year, 3, 2) * 86_400 + 10 * 3600;
    let november_dst_end = nth_sunday_of_month(year, 11, 1) * 86_400 + 9 * 3600;
    if now_unix >= march_dst_start && now_unix < november_dst_end {
        -7 * 3600 // PDT
    } else {
        -8 * 3600 // PST
    }
}

/// Days since epoch for the Nth Sunday of a given month/year (proleptic
/// Gregorian, via a days-from-civil calculation).
fn nth_sunday_of_month(year: i64, month: i64, n: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let first_of_month = era * 146_097 + doe - 719_468;
    let dow = (first_of_month + 3).rem_euclid(7); // 0=Monday
    let first_sunday = first_of_month + (7 - dow) % 7;
    first_sunday + (n - 1) * 7
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn weekday_during_peak_hours_is_peak() {
        // 2026-04-16 08:00 PDT = 15:00 UTC (Thursday).
        assert!(is_peak_time(1_776_351_600).is_peak);
    }

    #[test]
    fn weekday_outside_peak_hours_is_off_peak() {
        // 2026-04-16 14:00 PDT = 21:00 UTC (Thursday).
        assert!(!is_peak_time(1_776_373_200).is_peak);
    }

    #[test]
    fn weekend_is_always_off_peak() {
        // 2026-04-18 08:00 PDT (Saturday).
        assert!(!is_peak_time(1_776_524_400).is_peak);
    }

    #[test]
    fn winter_uses_pst_offset() {
        // 2026-01-15 09:00 PST = 17:00 UTC (Thursday) → peak.
        assert!(is_peak_time(1_768_496_400).is_peak);
    }

    #[test]
    fn humanize_hours_and_minutes() {
        assert_eq!(humanize(3 * 3600 + 3 * 60), "3h03m");
        assert_eq!(humanize(45 * 60), "45m");
        assert_eq!(humanize(-5), "0m");
    }
}
