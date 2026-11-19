//! When the engine process of the caller started, read from `ps`. A process id is reused after its
//! process exits, so the start time tells a session of this engine from a session of an older
//! process with the same id (#2904).

use std::time::{Duration, SystemTime};

/// When the engine process `pid` started, from its elapsed time in `ps`. `None` when `ps` cannot
/// tell. The cause is noticed, except for a process that is gone, which is only logged (#3858).
pub(in crate::task::session) fn engine_start(pid: u32) -> Option<SystemTime> {
    let mut cmd = std::process::Command::new("/bin/ps");
    cmd.args(["-o", "etime=", "-p", &pid.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = match crate::process::run_traced("task:ps", cmd, PS_TIMEOUT) {
        Ok(out) if out.status.success() => out,
        // `ps -p` exits 1 with no output for a pid that does not exist (#2923). That is an
        // expected state, so it is a debug line and not a notice.
        Ok(out)
            if out.status.code() == Some(1) && out.stdout.is_empty() && out.stderr.is_empty() =>
        {
            tracing::debug!("ps found no engine process {pid}");
            return None;
        }
        Ok(out) => {
            crate::agent_notice::record_logged(format!(
                "/bin/ps failed to read the age of engine process {pid} ({}): {}. Fix: check that \
                 /bin/ps works.",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            return None;
        }
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "cannot run /bin/ps to read the age of engine process {pid}: {e}. Fix: check that \
                 /bin/ps exists and works."
            ));
            return None;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(age) = parse_etime(text.trim()) else {
        crate::agent_notice::record_logged(format!(
            "cannot read the age of engine process {pid} from ps: {text:?}. Fix: report the \
             output of `ps -o etime= -p {pid}`."
        ));
        return None;
    };
    // `etime` has a resolution of one second.
    let start = SystemTime::now().checked_sub(age + Duration::from_secs(1));
    if start.is_none() {
        crate::agent_notice::record_logged(format!(
            "the age of engine process {pid} from ps is longer than the system clock allows. \
             Fix: check the clock of the machine."
        ));
    }
    start
}

/// `ps` answers in milliseconds. The bound keeps a stuck `ps` from holding the task store lock.
const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// Parse the `etime` of `ps`: `[[dd-]hh:]mm:ss`.
fn parse_etime(text: &str) -> Option<Duration> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (number(days, u64::MAX)?, clock),
        None => (0, text),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [m, s] => (0, number(m, 59)?, number(s, 59)?),
        [h, m, s] => (number(h, 23)?, number(m, 59)?, number(s, 59)?),
        _ => return None,
    };
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hours * 3_600 + minutes * 60 + seconds)?;
    Some(Duration::from_secs(secs))
}

/// An ASCII decimal number of at most 7 digits, no more than `max`.
fn number(text: &str, max: u64) -> Option<u64> {
    let shape_ok = (1..=7).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
    let value: u64 = shape_ok.then(|| text.parse().ok()).flatten()?;
    (value <= max).then_some(value)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn etime_reads_every_ps_shape() {
        assert_eq!(parse_etime("00:05"), Some(Duration::from_secs(5)));
        assert_eq!(parse_etime("01:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(
            parse_etime("2-00:00:01"),
            Some(Duration::from_secs(172_801))
        );
    }

    #[test]
    fn etime_refuses_a_malformed_value() {
        for text in [
            "", "5", "1:2:3:4", "00:60", "24:00:00", "a:00", "-1:00", "1-", "00:٣",
        ] {
            assert_eq!(parse_etime(text), None, "{text:?}");
        }
    }

    #[test]
    fn this_process_started_before_now() {
        let start = engine_start(std::process::id()).unwrap();
        assert!(start <= SystemTime::now());
    }

    /// The shape `[[dd-]hh:]mm:ss`, as a second statement of the rule: digits only, at most 7
    /// digits a field, hours below 24, minutes and seconds below 60.
    fn is_ps_shape(text: &str) -> bool {
        let re =
            regex::Regex::new(r"^(?:([0-9]{1,7})-)?(?:([0-9]{1,7}):)?([0-9]{1,7}):([0-9]{1,7})$")
                .unwrap();
        let Some(c) = re.captures(text) else {
            return false;
        };
        let at = |i: usize| c.get(i).map(|m| m.as_str().parse::<u64>().unwrap());
        at(2).is_none_or(|h| h <= 23) && at(3).unwrap() <= 59 && at(4).unwrap() <= 59
    }

    proptest! {
        // Arbitrary text never panics, and a value is read exactly when it has the ps shape.
        #[test]
        fn parse_etime_reads_exactly_the_ps_shape(
            text in prop_oneof!["\\PC{0,20}", "[0-9:\\-]{0,16}"],
        ) {
            prop_assert_eq!(parse_etime(&text).is_some(), is_ps_shape(&text));
        }

        #[test]
        fn etime_round_trips(days in 0u64..400, h in 0u64..24, m in 0u64..60, s in 0u64..60) {
            let text = if days > 0 {
                format!("{days}-{h:02}:{m:02}:{s:02}")
            } else {
                format!("{h:02}:{m:02}:{s:02}")
            };
            let expected = Duration::from_secs(days * 86_400 + h * 3_600 + m * 60 + s);
            prop_assert_eq!(parse_etime(&text), Some(expected));
        }
    }
}
