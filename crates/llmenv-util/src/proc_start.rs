//! When a process started, read from `ps`. A process id is reused after its process exits, so the
//! start time tells a process from an older process with the same id (#2904, #3795).

use std::time::{Duration, SystemTime};

/// `ps` answers in milliseconds. A stuck `ps` must not hold the proxy check or the task store lock.
const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// One field of `ps` for `pid`: `flags` pick the field, and `what` names it in an error. `None`
/// means the process is gone. `label` names the spawn in the trace.
pub fn ps_value(
    label: &str,
    pid: u32,
    flags: &[&str],
    what: &str,
) -> anyhow::Result<Option<String>> {
    // An absolute path: a `ps` earlier on PATH could answer for any pid.
    let mut cmd = std::process::Command::new("/bin/ps");
    cmd.args(flags)
        .args(["-p", &pid.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = crate::process::run_traced(label, cmd, PS_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("cannot run /bin/ps to read the {what} of pid {pid}: {e}"))?;
    ps_field(pid, &out, what)
}

/// Read the `ps` result for `pid`. `ps -p` exits 1 with no output for a pid that does not exist.
/// Any other failure is an error, so a broken `ps` does not read as "pid gone" (#2923).
pub fn ps_field(
    pid: u32,
    out: &std::process::Output,
    what: &str,
) -> anyhow::Result<Option<String>> {
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        return Ok((!text.is_empty()).then_some(text));
    }
    if out.status.code() == Some(1) && text.is_empty() && out.stderr.is_empty() {
        return Ok(None);
    }
    anyhow::bail!(
        "/bin/ps failed to read the {what} of pid {pid} ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// When the process `pid` started, from its elapsed time in `ps`. `None` when `ps` cannot
/// tell. The cause is noticed, except for a process that is gone, which is only logged (#3858).
pub fn process_start(pid: u32) -> Option<SystemTime> {
    let text = match ps_value("proc:ps", pid, &["-o", "etime="], "age") {
        Ok(Some(text)) => text,
        // An expected state, so it is a debug line and not a notice (#2923).
        Ok(None) => {
            tracing::debug!("ps found no process {pid}");
            return None;
        }
        Err(e) => {
            crate::agent_notice::record_logged(format!(
                "{e:#}. Fix: check that /bin/ps exists and works."
            ));
            return None;
        }
    };
    let Some(age) = parse_etime(&text) else {
        crate::agent_notice::record_logged(format!(
            "cannot read the age of process {pid} from ps: {text:?}. Fix: report the \
             output of `ps -o etime= -p {pid}`."
        ));
        return None;
    };
    // `etime` has a resolution of one second.
    let start = SystemTime::now().checked_sub(age + Duration::from_secs(1));
    if start.is_none() {
        crate::agent_notice::record_logged(format!(
            "the age of process {pid} from ps is longer than the system clock allows. \
             Fix: check the clock of the machine."
        ));
    }
    start
}

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
    // Each field has at most 7 digits, so the sum cannot overflow.
    Some(Duration::from_secs(
        days * 86_400 + hours * 3_600 + minutes * 60 + seconds,
    ))
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
        let start = process_start(std::process::id()).unwrap();
        assert!(start <= SystemTime::now());
    }

    proptest! {
        // Arbitrary text never panics, and a value that reads has fields in range.
        #[test]
        fn parse_etime_never_panics_on_any_text(
            text in prop_oneof!["\\PC{0,20}", "[0-9:\\-]{0,16}"],
        ) {
            if let Some(age) = parse_etime(&text) {
                prop_assert!(age.as_secs() < 10_000_000 * 86_400 + 86_400);
            }
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
